package dev.secondscrn.client

import android.view.Surface

/**
 * Hands the video surface to the Rust decoder. This is the only hand-written
 * JNI call; everything else goes through the generated UniFFI bindings.
 */
object NativeSurface {
    init {
        System.loadLibrary("secondscrn_client")
    }

    @JvmStatic
    external fun setSurface(surface: Surface?)
}
