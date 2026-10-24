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
