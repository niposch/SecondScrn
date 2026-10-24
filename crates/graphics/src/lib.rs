//! Platform-independent media helpers.
//!
//! * [`color`]: the RGB ⇄ Y'CbCr reference used by the GPU converter (its
//!   shader constants come from here) and by tests.
//! * [`pattern`]: deterministic test images for colour/sharpness validation.
//! * [`bitstream`]: Annex-B NAL helpers (keyframe detection, parameter sets).
//! * [`psnr`]: image comparison.

pub mod bitstream;
pub mod color;
pub mod pattern;
pub mod psnr;
