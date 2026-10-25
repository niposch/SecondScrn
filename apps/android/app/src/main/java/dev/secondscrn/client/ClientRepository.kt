package dev.secondscrn.client

import android.content.Context
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.util.Log
import java.io.File
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import uniffi.secondscrn_client.ClientInit
import uniffi.secondscrn_client.ClientListener
import uniffi.secondscrn_client.ClientState
import uniffi.secondscrn_client.Phase
import uniffi.secondscrn_client.PointerKind
import uniffi.secondscrn_client.Preset
import uniffi.secondscrn_client.SecondScrnClient

/**
 * Owns the Rust client and adapts it to Android: state as a StateFlow,
 * the Wi-Fi multicast lock for discovery, and the background policy.
 */
class ClientRepository(private val context: Context) {
    private val _state = MutableStateFlow<ClientState?>(null)
    val state: StateFlow<ClientState?> = _state.asStateFlow()

    private val main = Handler(Looper.getMainLooper())
    private var multicastLock: WifiManager.MulticastLock? = null
    private var autoConnectAttempted = false

    val client: SecondScrnClient by lazy {
        // Loads the native library (JNI part) before UniFFI/JNA uses it.
        NativeSurface
        SecondScrnClient(
            ClientInit(
                dataDir = context.filesDir.absolutePath,
                deviceName = deviceName(),
                deviceModel = "${Build.MANUFACTURER} ${Build.MODEL}",
                appVersion = BuildConfig.VERSION_NAME,
            ),
            KeystoreSecretStore(),
            object : ClientListener {
                override fun onStateChanged(state: ClientState) {
                    _state.value = state
                }
            },
        )
    }

    private fun deviceName(): String =
        Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)
            ?.takeIf { it.isNotBlank() } ?: "${Build.MANUFACTURER} ${Build.MODEL}"

    /** The app is visible: browse for PCs and reconnect to the last one. */
    fun onForeground() {
        main.removeCallbacks(backgroundDisconnect)
        acquireMulticastLock()
        client.startDiscovery()
        if (!autoConnectAttempted) {
            autoConnectAttempted = true
            if (client.state().phase is Phase.Idle) client.connectLast()
        }
    }

    /**
     * The app went to the background. The surface is gone, so decoding
     * stops; the connection is kept briefly (the PC keeps its virtual
     * monitor) and closed after a minute to save battery.
     */
    fun onBackground() {
        setLowLatencyWifi(false)
        client.stopDiscovery()
        releaseMulticastLock()
        main.postDelayed(backgroundDisconnect, BACKGROUND_GRACE_MS)
    }

    private val backgroundDisconnect = Runnable {
        Log.i(TAG, "disconnecting after a minute in the background")
        client.disconnect()
        autoConnectAttempted = false
    }

    fun connect(address: String, port: Int, fingerprint: String?) =
        client.connect(address, port.toUShort(), fingerprint)

    fun disconnect() = client.disconnect()
    fun forget(fingerprint: String) = client.forgetHost(fingerprint)
    fun setAutoReconnect(fingerprint: String, enabled: Boolean) = client.setHostAutoReconnect(fingerprint, enabled)
    fun setPreset(preset: Preset) = client.setPreset(preset)
    fun setTouchInput(enabled: Boolean) = client.setTouchInput(enabled)
    fun setDisplay(width: Int, height: Int, refreshHz: Int, dpi: Int) =
        client.setDisplay(width.toUInt(), height.toUInt(), refreshHz.toUInt(), dpi.toUInt())

    fun pointer(kind: PointerKind, x: Float, y: Float, secondary: Boolean = false) = client.pointer(kind, x, y, secondary)
    fun scroll(x: Float, y: Float, dx: Float, dy: Float) = client.scroll(x, y, dx, dy)
    fun requestKeyframe() = client.requestKeyframe()

    /** Writes a redacted diagnostic report and returns the file. */
    fun exportDiagnostics(): File {
        val dir = File(context.cacheDir, "diagnostics").apply { mkdirs() }
        return File(dir, "secondscrn-diagnostics.json").apply { writeText(client.exportDiagnostics()) }
    }

    private fun acquireMulticastLock() {
        if (multicastLock?.isHeld == true) return
        val wifi = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        multicastLock = wifi.createMulticastLock("secondscrn-discovery").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    private fun releaseMulticastLock() {
        multicastLock?.takeIf { it.isHeld }?.release()
    }

    private var wifiLock: WifiManager.WifiLock? = null

    /**
     * While the desktop is shown, keep Wi-Fi out of power-save mode. Power
     * save batches packets and causes tens of milliseconds of extra, jumpy
     * latency, which is very visible when moving the mouse.
     */
    fun setLowLatencyWifi(enabled: Boolean) {
        if (!enabled) {
            wifiLock?.takeIf { it.isHeld }?.release()
            return
        }
        if (wifiLock?.isHeld == true) return
        val wifi = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        val mode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) WifiManager.WIFI_MODE_FULL_LOW_LATENCY
        else @Suppress("DEPRECATION") WifiManager.WIFI_MODE_FULL_HIGH_PERF
        wifiLock = wifi.createWifiLock(mode, "secondscrn-stream").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    companion object {
        private const val TAG = "SecondScrn"
        private const val BACKGROUND_GRACE_MS = 60_000L
    }
}
