//! Annex-B (H.264/HEVC) bitstream helpers.

use secondscrn_protocol::Codec;

/// Iterates NAL unit payloads (without start codes) in an Annex-B buffer.
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            // Strip the start code and any zero bytes preceding it.
            let mut e = s - 3;
            while e > 0 && data[e - 1] == 0 {
                e -= 1;
            }
            e
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts.into_iter().zip(ends).filter(|(s, e)| s < e).map(move |(s, e)| &data[s..e])
}

fn nal_type(codec: Codec, nal: &[u8]) -> Option<u8> {
    let first = *nal.first()?;
    match codec {
        Codec::H264 => Some(first & 0x1f),
        Codec::H265 => Some((first >> 1) & 0x3f),
        Codec::Av1 => None,
    }
}

/// True if the access unit contains an IDR/IRAP picture.
pub fn is_keyframe(codec: Codec, au: &[u8]) -> bool {
    nal_units(au).filter_map(|n| nal_type(codec, n)).any(|t| match codec {
        Codec::H264 => t == 5,
        Codec::H265 => (16..=21).contains(&t),
        Codec::Av1 => false,
    })
}

/// True if the access unit carries its own parameter sets (SPS, and VPS for
/// HEVC), so a decoder can start from it.
pub fn has_parameter_sets(codec: Codec, au: &[u8]) -> bool {
    let types: Vec<u8> = nal_units(au).filter_map(|n| nal_type(codec, n)).collect();
    match codec {
        Codec::H264 => types.contains(&7) && types.contains(&8),
        Codec::H265 => types.contains(&32) && types.contains(&33) && types.contains(&34),
        Codec::Av1 => true,
    }
}

/// True if the buffer contains picture data (a VCL NAL unit), not only
/// delimiters, parameter sets or SEI. AV1 is always treated as a picture.
pub fn has_picture(codec: Codec, au: &[u8]) -> bool {
    match codec {
        Codec::Av1 => true,
        _ => nal_units(au).filter_map(|n| nal_type(codec, n)).any(|t| match codec {
            Codec::H264 => (1..=5).contains(&t),
            _ => t <= 31,
        }),
    }
}

/// Removes access unit delimiters, which decoders don't need. A delimiter
/// sent ahead of a large fragmented slice was delivered by the WebRTC
/// depacketizer as a frame of its own.
pub fn strip_delimiters(codec: Codec, au: &[u8]) -> Vec<u8> {
    let aud = match codec {
        Codec::H264 => 9,
        Codec::H265 => 35,
        Codec::Av1 => return au.to_vec(),
    };
    let mut out = Vec::with_capacity(au.len());
    for n in nal_units(au).filter(|n| nal_type(codec, n) != Some(aud)) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    out
}

/// Prepends `headers` (Annex-B parameter sets) to a keyframe that lacks them.
pub fn ensure_parameter_sets(codec: Codec, au: Vec<u8>, headers: &[u8]) -> Vec<u8> {
    if headers.is_empty() || codec == Codec::Av1 || !is_keyframe(codec, &au) || has_parameter_sets(codec, &au) {
        return au;
    }
    let mut out = Vec::with_capacity(headers.len() + au.len());
    out.extend_from_slice(headers);
    out.extend_from_slice(&au);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delimiters_are_removed_and_pictures_detected() {
        let au = [0, 0, 0, 1, 0x46, 0x01, 0x50, 0, 0, 0, 1, 0x02, 0x01, 9, 9];
        let stripped = strip_delimiters(Codec::H265, &au);
        assert_eq!(stripped, [0, 0, 0, 1, 0x02, 0x01, 9, 9]);
        assert!(has_picture(Codec::H265, &au));
        assert!(!has_picture(Codec::H265, &[0, 0, 0, 1, 0x46, 0x01, 0x50]));
        assert!(!has_picture(Codec::H265, &[0, 0, 0, 1, 0x40, 0x01, 1, 0, 0, 1, 0x42, 0x01, 2]));
        assert!(has_picture(Codec::H264, &[0, 0, 0, 1, 0x65, 1]));
        assert!(!has_picture(Codec::H264, &[0, 0, 0, 1, 0x09, 0x10]));
    }

    #[test]
    fn splits_nals_with_3_and_4_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        let nals: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]);
    }

    #[test]
    fn detects_h264_and_hevc_keyframes() {
        let idr = [0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 0x88];
        let p = [0, 0, 0, 1, 0x41, 0x9a];
        assert!(is_keyframe(Codec::H264, &idr));
        assert!(has_parameter_sets(Codec::H264, &idr));
        assert!(!is_keyframe(Codec::H264, &p));

        let hevc_idr = [0, 0, 0, 1, 0x26, 0x01, 0xaf];
        let hevc_trail = [0, 0, 0, 1, 0x02, 0x01, 0xd0];
        assert!(is_keyframe(Codec::H265, &hevc_idr));
        assert!(!has_parameter_sets(Codec::H265, &hevc_idr));
        assert!(!is_keyframe(Codec::H265, &hevc_trail));
    }

    #[test]
    fn prepends_missing_parameter_sets_only_to_keyframes() {
        let headers = [0, 0, 0, 1, 0x40, 0x01, 0, 0, 0, 1, 0x42, 0x01, 0, 0, 0, 1, 0x44, 0x01];
        let idr = vec![0, 0, 0, 1, 0x26, 0x01, 0xaf];
        let fixed = ensure_parameter_sets(Codec::H265, idr.clone(), &headers);
        assert!(has_parameter_sets(Codec::H265, &fixed));
        assert_eq!(&fixed[headers.len()..], &idr[..]);
        // Already complete: unchanged.
        assert_eq!(ensure_parameter_sets(Codec::H265, fixed.clone(), &headers), fixed);
        // Non-keyframe: unchanged.
        let trail = vec![0, 0, 0, 1, 0x02, 0x01, 0xd0];
        assert_eq!(ensure_parameter_sets(Codec::H265, trail.clone(), &headers), trail);
    }
}
