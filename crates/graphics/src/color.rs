//! Y'CbCr conversion per ITU-R BT.709 / BT.601.
//!
//! Inputs are non-linear (gamma-encoded) R'G'B' in 0..=1, exactly what the
//! Windows desktop composes in 8-bit SDR. No linearisation happens: video
//! colour spaces define Y'CbCr on gamma-encoded values.

use secondscrn_protocol::{ColorInfo, ColorRange, MatrixCoefficients};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coefficients {
    pub kr: f32,
    pub kb: f32,
}

impl Coefficients {
    pub const BT709: Coefficients = Coefficients { kr: 0.2126, kb: 0.0722 };
    pub const BT601: Coefficients = Coefficients { kr: 0.299, kb: 0.114 };

    pub fn kg(&self) -> f32 {
        1.0 - self.kr - self.kb
    }

    pub fn for_matrix(m: MatrixCoefficients) -> Self {
        match m {
            MatrixCoefficients::Bt709 => Self::BT709,
            MatrixCoefficients::Bt601 => Self::BT601,
        }
    }
}

/// Affine RGB → YCbCr transform: `out = m · rgb + offset`, all normalised to
/// 0..=1 code values. Laid out as three rows of `[r, g, b, offset]` so it can
/// be uploaded to a GPU constant buffer as three `float4`s.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RgbToYuv {
    pub rows: [[f32; 4]; 3],
}

impl RgbToYuv {
    pub fn new(c: Coefficients, range: ColorRange) -> Self {
        let kg = c.kg();
        // Full-scale signal: Ey in 0..1, Epb/Epr in -0.5..0.5.
        let ey = [c.kr, kg, c.kb];
        let pb_scale = 0.5 / (1.0 - c.kb);
        let pr_scale = 0.5 / (1.0 - c.kr);
        let epb = [-c.kr * pb_scale, -kg * pb_scale, (1.0 - c.kb) * pb_scale];
        let epr = [(1.0 - c.kr) * pr_scale, -kg * pr_scale, -c.kb * pr_scale];
        // Quantisation to 8-bit code values, expressed normalised (/255).
        let (y_scale, y_off, c_scale) = match range {
            ColorRange::Limited => (219.0 / 255.0, 16.0 / 255.0, 224.0 / 255.0),
            ColorRange::Full => (1.0, 0.0, 1.0),
        };
        let c_off = 128.0 / 255.0;
        let row = |e: [f32; 3], s: f32, o: f32| [e[0] * s, e[1] * s, e[2] * s, o];
        RgbToYuv { rows: [row(ey, y_scale, y_off), row(epb, c_scale, c_off), row(epr, c_scale, c_off)] }
    }

    pub fn for_color(info: &ColorInfo) -> Self {
        Self::new(Coefficients::for_matrix(info.matrix), info.range)
    }

    /// Converts one pixel; returns normalised (Y, Cb, Cr).
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let f = |r: &[f32; 4]| r[0] * rgb[0] + r[1] * rgb[1] + r[2] * rgb[2] + r[3];
        [f(&self.rows[0]), f(&self.rows[1]), f(&self.rows[2])]
    }

    /// 8-bit convenience wrapper with rounding and clamping.
    pub fn apply_u8(&self, rgb: [u8; 3]) -> [u8; 3] {
        let n = |v: u8| v as f32 / 255.0;
        let o = self.apply([n(rgb[0]), n(rgb[1]), n(rgb[2])]);
        o.map(|v| (v * 255.0).round().clamp(0.0, 255.0) as u8)
    }
}

/// Inverse transform, used by tests to emulate a decoder + display.
pub fn yuv_to_rgb_u8(yuv: [u8; 3], c: Coefficients, range: ColorRange) -> [u8; 3] {
    let (y, cb, cr) = (yuv[0] as f32, yuv[1] as f32, yuv[2] as f32);
    let (ey, epb, epr) = match range {
        ColorRange::Limited => ((y - 16.0) / 219.0, (cb - 128.0) / 224.0, (cr - 128.0) / 224.0),
        ColorRange::Full => (y / 255.0, (cb - 128.0) / 255.0, (cr - 128.0) / 255.0),
    };
    let r = ey + 2.0 * (1.0 - c.kr) * epr;
    let b = ey + 2.0 * (1.0 - c.kb) * epb;
    let g = (ey - c.kr * r - c.kb * b) / c.kg();
    [r, g, b].map(|v| (v * 255.0).round().clamp(0.0, 255.0) as u8)
}

/// CPU reference for BGRA → NV12 with 2×2 box-filtered chroma, matching the
/// GPU shader. `width`/`height` must be even. Returns (Y plane, interleaved
/// CbCr plane).
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize, m: &RgbToYuv) -> (Vec<u8>, Vec<u8>) {
    assert!(width.is_multiple_of(2) && height.is_multiple_of(2));
    assert_eq!(bgra.len(), width * height * 4);
    let px = |x: usize, y: usize| {
        let i = (y * width + x) * 4;
        [bgra[i + 2] as f32 / 255.0, bgra[i + 1] as f32 / 255.0, bgra[i] as f32 / 255.0]
    };
    let q = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    let mut luma = vec![0u8; width * height];
    for y in 0..height {
        for x in 0..width {
            luma[y * width + x] = q(m.apply(px(x, y))[0]);
        }
    }
    let mut chroma = vec![0u8; width * height / 2];
    for y in 0..height / 2 {
        for x in 0..width / 2 {
            let mut acc = [0f32; 3];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let p = px(2 * x + dx, 2 * y + dy);
                for k in 0..3 {
                    acc[k] += p[k] * 0.25;
                }
            }
            let yuv = m.apply(acc);
            chroma[y * width + 2 * x] = q(yuv[1]);
            chroma[y * width + 2 * x + 1] = q(yuv[2]);
        }
    }
    (luma, chroma)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bt709_limited() -> RgbToYuv {
        RgbToYuv::new(Coefficients::BT709, ColorRange::Limited)
    }

    #[test]
    fn reference_values_bt709_limited() {
        let m = bt709_limited();
        // Values from ITU-R BT.709 / common 8-bit tables.
        assert_eq!(m.apply_u8([0, 0, 0]), [16, 128, 128]);
        assert_eq!(m.apply_u8([255, 255, 255]), [235, 128, 128]);
        assert_eq!(m.apply_u8([255, 0, 0]), [63, 102, 240]);
        assert_eq!(m.apply_u8([0, 255, 0]), [173, 42, 26]);
        assert_eq!(m.apply_u8([0, 0, 255]), [32, 240, 118]);
        assert_eq!(m.apply_u8([128, 128, 128]), [126, 128, 128]);
    }

    #[test]
    fn reference_values_full_range_and_601() {
        let full = RgbToYuv::new(Coefficients::BT709, ColorRange::Full);
        assert_eq!(full.apply_u8([0, 0, 0]), [0, 128, 128]);
        assert_eq!(full.apply_u8([255, 255, 255]), [255, 128, 128]);
        let bt601 = RgbToYuv::new(Coefficients::BT601, ColorRange::Limited);
        assert_eq!(bt601.apply_u8([255, 0, 0]), [81, 90, 240]);
    }

    #[test]
    fn roundtrip_error_is_small_for_all_primaries_and_greys() {
        for range in [ColorRange::Limited, ColorRange::Full] {
            let m = RgbToYuv::new(Coefficients::BT709, range);
            let mut worst = 0i32;
            for r in (0..=255).step_by(15) {
                for g in (0..=255).step_by(15) {
                    for b in (0..=255).step_by(15) {
                        let rgb = [r as u8, g as u8, b as u8];
                        let back = yuv_to_rgb_u8(m.apply_u8(rgb), Coefficients::BT709, range);
                        for k in 0..3 {
                            worst = worst.max((back[k] as i32 - rgb[k] as i32).abs());
                        }
                    }
                }
            }
            // 8-bit limited range quantisation costs up to ~2 code values.
            assert!(worst <= 2, "{range:?}: worst error {worst}");
        }
    }

    #[test]
    fn nv12_reference_layout() {
        // 2x2 block: red, green / blue, white.
        let bgra = [0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255];
        let m = bt709_limited();
        let (y, uv) = bgra_to_nv12(&bgra, 2, 2, &m);
        assert_eq!(y, vec![63, 173, 32, 235]);
        assert_eq!(uv.len(), 2);
        // Chroma of the average colour (0.5, 0.5, 0.5)... red+green+blue+white
        // averages to (0.5, 0.5, 0.5): neutral.
        assert_eq!(uv, vec![128, 128]);
    }
}
