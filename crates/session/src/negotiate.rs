//! Codec and stream-parameter negotiation.
//!
//! Both sides report capabilities they have *verified* by instantiating the
//! codec (not just by seeing it listed). The host picks the best pair.

use secondscrn_protocol::{
    Codec, CodecCapability, ColorInfo, DisplayRequest, QualityPreset, StreamConfig,
};

use crate::adapt::BitratePolicy;

/// Preference when nothing is forced: HEVC gives the best quality per bit on
/// widely deployed hardware; AV1 is comparable or better but tablet decoders
/// are less common; H.264 is the universal fallback.
pub const DEFAULT_CODEC_ORDER: [Codec; 3] = [Codec::H265, Codec::Av1, Codec::H264];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiationInput<'a> {
    pub encoders: &'a [CodecCapability],
    pub decoders: &'a [CodecCapability],
    pub display: DisplayRequest,
    pub preset: QualityPreset,
    /// User override on the host, then the client's preference.
    pub forced_codec: Option<Codec>,
    pub client_preferred: Option<Codec>,
    pub host_max_fps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NegotiationError {
    #[error("the host has no working hardware video encoder")]
    NoEncoder,
    #[error("no codec is supported by both devices at {0}x{1}")]
    NoCommonCodec(u32, u32),
}

fn fits(cap: &CodecCapability, w: u32, h: u32) -> bool {
    // Decoders usually accept either orientation of their maximum size.
    let (cap_long, cap_short) = (
        cap.max_width.max(cap.max_height),
        cap.max_width.min(cap.max_height),
    );
    w.max(h) <= cap_long && w.min(h) <= cap_short
}

/// Score for an encoder/decoder pair: hardware on both ends first, then the
/// codec order.
fn score(order_index: usize, enc: &CodecCapability, dec: &CodecCapability) -> (u8, usize) {
    let hw = u8::from(!enc.hardware) + u8::from(!dec.hardware);
    (hw, order_index)
}

pub fn negotiate(input: &NegotiationInput<'_>) -> Result<StreamConfig, NegotiationError> {
    if input.encoders.is_empty() {
        return Err(NegotiationError::NoEncoder);
    }
    let (w, h) = (input.display.width, input.display.height);

    let mut order: Vec<Codec> = Vec::with_capacity(4);
    if let Some(c) = input.forced_codec.or(input.client_preferred) {
        order.push(c);
    }
    for c in DEFAULT_CODEC_ORDER {
        if !order.contains(&c) {
            order.push(c);
        }
    }

    let mut best: Option<((u8, usize), &CodecCapability, &CodecCapability)> = None;
    for (i, codec) in order.iter().enumerate() {
        for enc in input
            .encoders
            .iter()
            .filter(|e| e.codec == *codec && fits(e, w, h))
        {
            for dec in input
                .decoders
                .iter()
                .filter(|d| d.codec == *codec && fits(d, w, h))
            {
                // A forced codec wins over the hardware preference.
                let s = if input.forced_codec == Some(*codec) {
                    (0, 0)
                } else {
                    score(i, enc, dec)
                };
                if best.as_ref().is_none_or(|(b, _, _)| s < *b) {
                    best = Some((s, enc, dec));
                }
            }
        }
    }
    let (_, enc, dec) = best.ok_or(NegotiationError::NoCommonCodec(w, h))?;

    let max_fps = [
        input.display.refresh_hz.max(1),
        enc.max_fps,
        dec.max_fps,
        input.host_max_fps,
        input.preset.fps_cap(),
    ]
    .into_iter()
    .filter(|v| *v > 0)
    .min()
    .unwrap_or(60);
    let policy = BitratePolicy::for_stream(enc.codec, w, h, max_fps, input.preset, None);

    Ok(StreamConfig {
        codec: enc.codec,
        width: w,
        height: h,
        max_fps,
        color: ColorInfo::SDR_BT709_LIMITED,
        initial_bitrate_bps: policy.initial,
        encoder: enc.implementation.clone(),
    })
}

trait PresetExt {
    fn fps_cap(self) -> u32;
}

impl PresetExt for QualityPreset {
    fn fps_cap(self) -> u32 {
        match self {
            QualityPreset::LowBandwidth => 30,
            _ => 240,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secondscrn_protocol::Orientation;

    fn cap(codec: Codec, hw: bool, max: (u32, u32), fps: u32) -> CodecCapability {
        CodecCapability {
            codec,
            implementation: format!("{codec}-{}", if hw { "hw" } else { "sw" }),
            hardware: hw,
            max_width: max.0,
            max_height: max.1,
            max_fps: fps,
        }
    }

    fn display(w: u32, h: u32) -> DisplayRequest {
        DisplayRequest {
            width: w,
            height: h,
            refresh_hz: 60,
            dpi: 260,
            orientation: Orientation::Landscape,
        }
    }

    fn input<'a>(
        enc: &'a [CodecCapability],
        dec: &'a [CodecCapability],
        d: DisplayRequest,
    ) -> NegotiationInput<'a> {
        NegotiationInput {
            encoders: enc,
            decoders: dec,
            display: d,
            preset: QualityPreset::Automatic,
            forced_codec: None,
            client_preferred: None,
            host_max_fps: 60,
        }
    }

    const BIG: (u32, u32) = (4096, 2304);

    #[test]
    fn prefers_hevc_when_both_have_hardware() {
        let enc = [
            cap(Codec::H264, true, BIG, 60),
            cap(Codec::H265, true, BIG, 60),
            cap(Codec::Av1, true, BIG, 60),
        ];
        let dec = [
            cap(Codec::H264, true, BIG, 60),
            cap(Codec::H265, true, BIG, 60),
        ];
        let s = negotiate(&input(&enc, &dec, display(2560, 1600))).unwrap();
        assert_eq!(s.codec, Codec::H265);
        assert_eq!((s.width, s.height, s.max_fps), (2560, 1600, 60));
    }

    #[test]
    fn hardware_decoder_beats_codec_order() {
        let enc = [
            cap(Codec::H264, true, BIG, 60),
            cap(Codec::H265, true, BIG, 60),
        ];
        let dec = [
            cap(Codec::H264, true, BIG, 60),
            cap(Codec::H265, false, BIG, 60),
        ];
        assert_eq!(
            negotiate(&input(&enc, &dec, display(1920, 1200)))
                .unwrap()
                .codec,
            Codec::H264
        );
    }

    #[test]
    fn resolution_limits_are_respected_in_either_orientation() {
        let enc = [
            cap(Codec::H264, true, BIG, 60),
            cap(Codec::H265, true, BIG, 60),
        ];
        let dec = [
            cap(Codec::H264, true, BIG, 60),
            cap(Codec::H265, true, (1920, 1080), 60),
        ];
        // Portrait 1080x1920 fits a 1920x1080 decoder.
        assert_eq!(
            negotiate(&input(&enc, &dec, display(1080, 1920)))
                .unwrap()
                .codec,
            Codec::H265
        );
        // 2560x1600 does not; fall back to H.264.
        assert_eq!(
            negotiate(&input(&enc, &dec, display(2560, 1600)))
                .unwrap()
                .codec,
            Codec::H264
        );
    }

    #[test]
    fn forced_codec_wins_and_missing_codec_fails() {
        let enc = [
            cap(Codec::H264, true, BIG, 60),
            cap(Codec::H265, true, BIG, 60),
        ];
        let dec = [
            cap(Codec::H264, true, BIG, 60),
            cap(Codec::H265, true, BIG, 60),
        ];
        let mut i = input(&enc, &dec, display(1920, 1080));
        i.forced_codec = Some(Codec::H264);
        assert_eq!(negotiate(&i).unwrap().codec, Codec::H264);

        let dec_av1 = [cap(Codec::Av1, true, BIG, 60)];
        assert_eq!(
            negotiate(&input(&enc, &dec_av1, display(1920, 1080))),
            Err(NegotiationError::NoCommonCodec(1920, 1080))
        );
        assert_eq!(
            negotiate(&input(&[], &dec, display(1920, 1080))),
            Err(NegotiationError::NoEncoder)
        );
    }

    #[test]
    fn fps_is_the_minimum_of_all_limits() {
        let enc = [cap(Codec::H265, true, BIG, 120)];
        let dec = [cap(Codec::H265, true, BIG, 120)];
        let mut d = display(1920, 1080);
        d.refresh_hz = 120;
        let mut i = input(&enc, &dec, d);
        i.host_max_fps = 90;
        assert_eq!(negotiate(&i).unwrap().max_fps, 90);
        i.preset = QualityPreset::LowBandwidth;
        assert_eq!(negotiate(&i).unwrap().max_fps, 30);
    }

    #[test]
    fn color_is_bt709_limited() {
        let enc = [cap(Codec::H264, true, BIG, 60)];
        let s = negotiate(&input(&enc, &enc, display(1920, 1080))).unwrap();
        assert_eq!(s.color, ColorInfo::SDR_BT709_LIMITED);
        assert!(s.initial_bitrate_bps > 1_000_000);
    }
}
