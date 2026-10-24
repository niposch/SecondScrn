//! Deterministic BGRA test patterns.
//!
//! The "desktop" pattern mixes what matters for a second monitor: colour bars
//! for colour accuracy, a grey ramp for banding/range errors, 1-pixel
//! black/white and red/blue line pairs for sharpness and chroma bleeding, and
//! flat areas typical of UI backgrounds.

/// 8-bit BGRA image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub bgra: Vec<u8>,
}

impl Image {
    pub fn rgb_at(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.width + x) * 4;
        [self.bgra[i + 2], self.bgra[i + 1], self.bgra[i]]
    }
}

pub const BARS: [[u8; 3]; 8] =
    [[255, 255, 255], [255, 255, 0], [0, 255, 255], [0, 255, 0], [255, 0, 255], [255, 0, 0], [0, 0, 255], [0, 0, 0]];

/// Region layout of [`desktop`], so tests can sample known areas.
pub struct DesktopLayout {
    pub bars_height: usize,
    pub ramp_top: usize,
    pub ramp_height: usize,
    pub lines_top: usize,
}

pub fn desktop_layout(height: usize) -> DesktopLayout {
    let bars_height = height * 3 / 8;
    let ramp_height = height / 8;
    DesktopLayout { bars_height, ramp_top: bars_height, ramp_height, lines_top: bars_height + ramp_height }
}

pub fn desktop(width: usize, height: usize) -> Image {
    let mut bgra = vec![0u8; width * height * 4];
    let layout = desktop_layout(height);
    for y in 0..height {
        for x in 0..width {
            let rgb: [u8; 3] = if y < layout.bars_height {
                BARS[x * BARS.len() / width]
            } else if y < layout.lines_top {
                let v = (x * 255 / (width - 1).max(1)) as u8;
                [v, v, v]
            } else {
                let band = (y - layout.lines_top) * 4 / (height - layout.lines_top).max(1);
                match band {
                    // Vertical 1-px black/white lines (luma sharpness).
                    0 => {
                        if x % 2 == 0 {
                            [0, 0, 0]
                        } else {
                            [255, 255, 255]
                        }
                    }
                    // Red/blue 1-px lines (chroma subsampling stress).
                    1 => {
                        if x % 2 == 0 {
                            [255, 0, 0]
                        } else {
                            [0, 0, 255]
                        }
                    }
                    // Dark-mode IDE-like flat background.
                    2 => [30, 30, 30],
                    _ => [243, 243, 243],
                }
            };
            let i = (y * width + x) * 4;
            bgra[i] = rgb[2];
            bgra[i + 1] = rgb[1];
            bgra[i + 2] = rgb[0];
            bgra[i + 3] = 255;
        }
    }
    Image { width, height, bgra }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_regions_have_expected_colours() {
        let img = desktop(256, 256);
        let l = desktop_layout(256);
        assert_eq!(img.rgb_at(0, 0), [255, 255, 255]);
        assert_eq!(img.rgb_at(255, 0), [0, 0, 0]);
        assert_eq!(img.rgb_at(0, l.ramp_top), [0, 0, 0]);
        assert_eq!(img.rgb_at(255, l.ramp_top), [255, 255, 255]);
        assert_eq!(img.rgb_at(0, l.lines_top), [0, 0, 0]);
        assert_eq!(img.rgb_at(1, l.lines_top), [255, 255, 255]);
        assert_eq!(img.bgra.len(), 256 * 256 * 4);
    }
}
