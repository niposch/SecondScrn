//! JNI entry point for the video surface.
//!
//! Kotlin: `dev.secondscrn.client.NativeSurface.setSurface(surface: Surface?)`.

use jni::JNIEnv;
use jni::objects::{JClass, JObject};
use ndk::native_window::NativeWindow;

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_secondscrn_client_NativeSurface_setSurface(
    env: JNIEnv,
    _class: JClass,
    surface: JObject,
) {
    let window = if surface.is_null() {
        None
    } else {
        // Acquires a reference to the ANativeWindow; released when dropped.
        unsafe { NativeWindow::from_surface(env.get_raw() as _, surface.as_raw() as _) }
    };
    crate::decoder::set_surface(window);
}
