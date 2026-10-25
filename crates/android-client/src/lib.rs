//! Android client for SecondScrn.
//!
//! * [`api`]: the UniFFI interface used by the Kotlin/Compose app.
//! * `decoder`: NDK MediaCodec decoding straight onto the app's Surface.
//! * `jni`: the one hand-written JNI entry point, which hands the
//!   `android.view.Surface` to Rust (UniFFI cannot pass Java objects).

uniffi::setup_scaffolding!();

pub mod api;
#[cfg(target_os = "android")]
mod decoder;
#[cfg(target_os = "android")]
mod jni_surface;
mod logging;
pub mod platform;
#[cfg(target_os = "android")]
mod upscale;

/// Snapdragon GSR can run on this device (Adreno GPU, OpenGL ES 3.1).
pub(crate) fn super_resolution_supported() -> bool {
    #[cfg(target_os = "android")]
    {
        upscale::supported()
    }
    #[cfg(not(target_os = "android"))]
    {
        false
    }
}

pub(crate) fn set_super_resolution(enabled: bool) {
    #[cfg(target_os = "android")]
    upscale::set_enabled(enabled);
    #[cfg(not(target_os = "android"))]
    let _ = enabled;
}
