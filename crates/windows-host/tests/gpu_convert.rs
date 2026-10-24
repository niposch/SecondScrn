//! GPU colour conversion must match the CPU reference exactly (±1 for
//! floating-point rounding differences between GPU and CPU).

#![cfg(windows)]

use secondscrn_graphics::color::{Coefficients, RgbToYuv, bgra_to_nv12};
use secondscrn_graphics::pattern;
use secondscrn_graphics::psnr::max_abs_diff;
use secondscrn_host::{convert, d3d};
use secondscrn_protocol::ColorRange;

#[test]
fn gpu_nv12_matches_cpu_reference() {
    let gpu = match d3d::create(None) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("skipping: no GPU ({e})");
            return;
        }
    };
    for range in [ColorRange::Limited, ColorRange::Full] {
        let matrix = RgbToYuv::new(Coefficients::BT709, range);
        let img = pattern::desktop(320, 240);
        let tex = convert::upload_bgra(&gpu, 320, 240, &img.bgra).unwrap();
        let conv = convert::Converter::new(&gpu, &matrix).unwrap();
        let target = conv.create_target(320, 240).unwrap();
        let srv = conv.source_view(&tex).unwrap();
        conv.convert(&srv, &target);
        let (y, uv) = convert::read_nv12(&gpu, &target.texture).unwrap();
        let (ry, ruv) = bgra_to_nv12(&img.bgra, 320, 240, &matrix);
        assert!(max_abs_diff(&y, &ry) <= 1, "{range:?} luma differs by {}", max_abs_diff(&y, &ry));
        assert!(max_abs_diff(&uv, &ruv) <= 1, "{range:?} chroma differs by {}", max_abs_diff(&uv, &ruv));
        // Spot-check absolute values: top-left bar is white.
        let white = if range == ColorRange::Limited { 235 } else { 255 };
        assert_eq!(y[0], white);
    }
}
