package dev.secondscrn.client.ui

import android.os.Build
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import dev.secondscrn.client.ClientRepository
import dev.secondscrn.client.NativeSurface
import kotlinx.coroutines.delay
import uniffi.secondscrn_client.ClientState
import uniffi.secondscrn_client.PenKind
import uniffi.secondscrn_client.Phase
import uniffi.secondscrn_client.PointerKind
import uniffi.secondscrn_client.TouchPoint

@Composable
fun StreamScreen(state: ClientState, repo: ClientRepository) {
    val stream = state.stream ?: return
    var overlay by rememberSaveable { mutableStateOf(false) }
    var hint by rememberSaveable { mutableStateOf(true) }
    val touchEnabled by rememberUpdatedState(state.touchInput)
    val asMouse by rememberUpdatedState(state.touchAsMouse)
    val maxFps by rememberUpdatedState(stream.maxFps.toInt())
    BackHandler { overlay = !overlay }
    LaunchedEffect(Unit) {
        delay(4000)
        hint = false
    }

    Box(Modifier.fillMaxSize().background(Color.Black), contentAlignment = Alignment.Center) {
        val aspect = stream.width.toFloat() / stream.height.toFloat()
        AndroidView(
            modifier = Modifier.aspectRatio(aspect, matchHeightConstraintsFirst = aspect < 1f).fillMaxSize(),
            factory = { ctx ->
                SurfaceView(ctx).apply {
                    holder.addCallback(object : SurfaceHolder.Callback {
                        override fun surfaceCreated(h: SurfaceHolder) {}
                        override fun surfaceChanged(h: SurfaceHolder, format: Int, w: Int, hh: Int) {
                            NativeSurface.setSurface(h.surface)
                            requestFrameRate(h, maxFps)
                            repo.requestKeyframe()
                        }
                        override fun surfaceDestroyed(h: SurfaceHolder) = NativeSurface.setSurface(null)
                    })
                    val sink = object : InputSink {
                        override fun pointer(kind: PointerKind, x: Float, y: Float, secondary: Boolean) {
                            if (touchEnabled) repo.pointer(kind, x, y, secondary)
                        }
                        override fun scroll(x: Float, y: Float, dx: Float, dy: Float) {
                            if (touchEnabled) repo.scroll(x, y, dx, dy)
                        }
                        override fun touch(points: List<TouchPoint>) {
                            if (touchEnabled) repo.touch(points)
                        }
                        override fun pen(
                            kind: PenKind, x: Float, y: Float, pressure: Float,
                            tiltX: Float, tiltY: Float, barrel: Boolean, eraser: Boolean,
                        ) {
                            if (touchEnabled) repo.pen(kind, x, y, pressure, tiltX, tiltY, barrel, eraser)
                        }
                        override val touchAsMouse get() = asMouse
                        override fun toggleOverlay() {
                            overlay = !overlay
                        }
                    }
                    val input = TouchInput(this, sink)
                    setOnTouchListener(input)
                    setOnGenericMotionListener(input)
                }
            },
            // Decode at exactly the stream resolution; the compositor maps it
            // 1:1 when the PC honoured the tablet's size.
            update = {
                it.holder.setFixedSize(stream.width.toInt(), stream.height.toInt())
                requestFrameRate(it.holder, stream.maxFps.toInt())
            },
        )

        StatusBadge(state)

        // Opens the options without taking a gesture away from the PC.
        if (!overlay) {
            Box(
                Modifier.align(Alignment.TopCenter)
                    .size(width = 72.dp, height = 28.dp)
                    .clickable(onClickLabel = "Show session options") { overlay = true },
                contentAlignment = Alignment.TopCenter,
            ) {
                Box(
                    Modifier.padding(top = 6.dp).size(width = 48.dp, height = 5.dp)
                        .background(Color(0x99FFFFFF), RoundedCornerShape(3.dp))
                        .border(0.5.dp, Color(0x66000000), RoundedCornerShape(3.dp)),
                )
            }
        }

        AnimatedVisibility(hint && !overlay, enter = fadeIn(), exit = fadeOut(), modifier = Modifier.align(Alignment.BottomCenter)) {
            Surface(shape = RoundedCornerShape(20.dp), color = Color(0xCC000000), modifier = Modifier.padding(24.dp)) {
                Text(
                    if (state.touchAsMouse) "Three-finger tap, Back or the handle at the top for options" else "Back or the handle at the top for options",
                    color = Color.White,
                    modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
                )
            }
        }

        AnimatedVisibility(overlay, enter = fadeIn(), exit = fadeOut(), modifier = Modifier.align(Alignment.TopCenter)) {
            SessionOverlay(state, repo, onClose = { overlay = false })
        }
    }

    DisposableEffect(Unit) { onDispose { NativeSurface.setSurface(null) } }
}

/**
 * Asks the display to run at the stream's frame rate. Without it Android keeps
 * many tablets at 60 Hz for apps, whatever the PC sends.
 */
private fun requestFrameRate(holder: SurfaceHolder, fps: Int) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return
    val surface = holder.surface ?: return
    if (!surface.isValid) return
    runCatching { surface.setFrameRate(fps.toFloat(), Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE) }
}

@Composable
private fun StatusBadge(state: ClientState) {
    val text = when (val p = state.phase) {
        is Phase.Interrupted -> "Connection interrupted — waiting…"
        is Phase.Reconnecting -> "Reconnecting (attempt ${p.attempt})…"
        is Phase.Negotiating -> "Starting stream…"
        else -> null
    } ?: return
    Box(Modifier.fillMaxSize().safeDrawingPadding(), contentAlignment = Alignment.TopCenter) {
        Surface(shape = RoundedCornerShape(20.dp), color = Color(0xCC8A5A00), modifier = Modifier.padding(16.dp)) {
            Text(text, color = Color.White, modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp))
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun SessionOverlay(state: ClientState, repo: ClientRepository, onClose: () -> Unit) {
    // Translucent: the desktop stays visible behind the options.
    Surface(
        shape = RoundedCornerShape(bottomStart = 16.dp, bottomEnd = 16.dp),
        color = MaterialTheme.colorScheme.surface.copy(alpha = 0.72f),
        tonalElevation = 6.dp,
        modifier = Modifier.fillMaxWidth().safeDrawingPadding(),
    ) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                Column(Modifier.weight(1f)) {
                    Text(state.hostName ?: "PC", style = MaterialTheme.typography.titleLarge)
                    StreamStats(state)
                }
                OutlinedButton(onClick = onClose) { Text("Hide") }
                Button(onClick = repo::disconnect) { Text("Disconnect") }
            }
            Text("Quality", style = MaterialTheme.typography.labelLarge)
            PresetSelector(state.preset, repo::setPreset)
            Text("Resolution", style = MaterialTheme.typography.labelLarge)
            ResolutionSelector(state, repo)
            Text("Frame rate", style = MaterialTheme.typography.labelLarge)
            FrameRateSelector(state, repo)
            FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                FilterChip(
                    selected = state.touchInput,
                    onClick = { repo.setTouchInput(!state.touchInput) },
                    label = { Text(if (state.touchInput) "Touch controls the PC" else "Touch input off (view only)") },
                )
                FilterChip(
                    selected = state.touchAsMouse,
                    enabled = state.touchInput,
                    onClick = { repo.setTouchAsMouse(!state.touchAsMouse) },
                    label = { Text("Touch as mouse") },
                )
                FilterChip(
                    selected = state.rotationLocked,
                    onClick = { repo.setRotationLocked(!state.rotationLocked) },
                    label = { Text(if (state.rotationLocked) "Rotation locked" else "Lock rotation") },
                )
            }
        }
    }
}

/** Stream sizes for this screen; lower ones save bandwidth on weak Wi-Fi. */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun ResolutionSelector(state: ClientState, repo: ClientRepository) {
    val current = state.stream?.scalePercent?.toInt()
    FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        state.resolutions.forEach { r ->
            val pct = r.scalePercent.toInt()
            FilterChip(
                selected = current == pct,
                onClick = { repo.setResolution(pct) },
                label = { Text("${r.width}×${r.height}" + if (pct == 100) " (native)" else " ($pct %)") },
            )
        }
        if (current == 0) {
            val s = state.stream
            FilterChip(selected = true, onClick = {}, label = { Text("${s?.width}×${s?.height} (set on PC)") })
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun FrameRateSelector(state: ClientState, repo: ClientRepository) {
    val current = state.stream?.maxFps?.toInt()
    FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        state.frameRates.forEach { r ->
            FilterChip(
                selected = current == r.toInt(),
                onClick = { repo.setFrameRate(r.toInt()) },
                label = { Text("$r fps") },
            )
        }
    }
}

@Composable
fun StreamStats(state: ClientState) {
    val s = state.stream ?: return
    val m = state.metrics
    // Format each value on its own: composed text may contain '%'.
    val parts = listOf(
        "${s.codec} ${s.width}×${s.height}",
        "%.0f fps".format(m.renderFps),
        "%.1f Mbit/s".format(m.bitrateMbps),
        "RTT " + (m.rttMs?.let { "%.0f ms".format(it) } ?: "–"),
        "loss " + (m.lossPercent?.let { "%.1f".format(it) + " %" } ?: "–"),
        m.health.name.lowercase(),
    )
    Text(parts.joinToString(" · "), style = MaterialTheme.typography.bodyMedium)
}
