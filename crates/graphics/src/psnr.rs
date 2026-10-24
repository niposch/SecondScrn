//! Peak signal-to-noise ratio for 8-bit planes.

/// PSNR in dB between two equally sized 8-bit buffers. Identical input
/// returns `f64::INFINITY`.
pub fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    if a.is_empty() {
        return f64::INFINITY;
    }
    let mse: f64 = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

/// Largest absolute per-sample difference.
pub fn max_abs_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psnr_behaves() {
        assert!(psnr(&[1, 2, 3], &[1, 2, 3]).is_infinite());
        let p = psnr(&[100; 100], &[101; 100]);
        assert!((p - 48.13).abs() < 0.01, "{p}");
        assert_eq!(max_abs_diff(&[1, 9], &[4, 9]), 3);
    }
}
