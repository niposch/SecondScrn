package dev.secondscrn.client

import android.content.pm.ActivityInfo
import android.content.res.Configuration
import android.os.Build
import android.os.Bundle
import android.util.DisplayMetrics
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.repeatOnLifecycle
import kotlinx.coroutines.launch
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import dev.secondscrn.client.ui.SecondScrnRoot
import dev.secondscrn.client.ui.SecondScrnTheme
import kotlin.math.roundToInt

class MainActivity : ComponentActivity() {
    private val repo get() = (application as SecondScrnApp).repository

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        reportDisplay()
        lifecycleScope.launch {
            repeatOnLifecycle(Lifecycle.State.STARTED) {
                repo.state.collect { s -> if (s != null) applyRotationLock(s.rotationLocked) }
            }
        }
        setContent {
            SecondScrnTheme {
                SecondScrnRoot(repo = repo, onStreamingChanged = ::setStreamingMode)
            }
        }
    }

    override fun onStart() {
        super.onStart()
        repo.onForeground()
    }

    override fun onStop() {
        super.onStop()
        repo.onBackground()
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        // Rotation or window resize: the PC re-configures the virtual monitor.
        reportDisplay()
    }

    /** Locks the screen in its current orientation, or follows the sensor again. */
    private fun applyRotationLock(locked: Boolean) {
        val wanted = if (locked) ActivityInfo.SCREEN_ORIENTATION_LOCKED else ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
        if (requestedOrientation != wanted) requestedOrientation = wanted
    }

    /** Tells the PC how large this screen is, in physical pixels. */
    private fun reportDisplay() {
        val (w, h) = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val b = windowManager.currentWindowMetrics.bounds
            b.width() to b.height()
        } else {
            val m = DisplayMetrics()
            @Suppress("DEPRECATION")
            windowManager.defaultDisplay.getRealMetrics(m)
            m.widthPixels to m.heightPixels
        }
        @Suppress("DEPRECATION")
        val refresh = (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) display else windowManager.defaultDisplay)
            ?.refreshRate?.roundToInt() ?: 60
        repo.setDisplay(w, h, refresh.coerceIn(30, 240), resources.displayMetrics.densityDpi)
    }

    /** Full-screen, screen-on presentation while the desktop is shown. */
    private fun setStreamingMode(streaming: Boolean) {
        repo.setLowLatencyWifi(streaming)
        val controller = WindowCompat.getInsetsController(window, window.decorView)
        if (streaming) {
            controller.systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            controller.hide(WindowInsetsCompat.Type.systemBars())
            window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        } else {
            controller.show(WindowInsetsCompat.Type.systemBars())
            window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        }
    }
}
