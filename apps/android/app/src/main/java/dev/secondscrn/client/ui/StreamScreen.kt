package dev.secondscrn.client.ui

import android.view.SurfaceHolder
import android.view.SurfaceView
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
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
import uniffi.secondscrn_client.Phase
import uniffi.secondscrn_client.PointerKind

@Composable
fun StreamScreen(state: ClientState, repo: ClientRepository) {
    val stream = state.stream ?: return
    var overlay by rememberSaveable { mutableStateOf(false) }
    var hint by rememberSaveable { mutableStateOf(true) }
    val touchEnabled by rememberUpdatedState(state.touchInput)
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
            update = { it.holder.setFixedSize(stream.width.toInt(), stream.height.toInt()) },
        )

        StatusBadge(state)

        AnimatedVisibility(hint && !overlay, enter = fadeIn(), exit = fadeOut(), modifier = Modifier.align(Alignment.BottomCenter)) {
            Surface(shape = RoundedCornerShape(20.dp), color = Color(0xCC000000), modifier = Modifier.padding(24.dp)) {
                Text(
                    "Three-finger tap or Back for options",
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

@Composable
private fun SessionOverlay(state: ClientState, repo: ClientRepository, onClose: () -> Unit) {
    Surface(
        shape = RoundedCornerShape(bottomStart = 16.dp, bottomEnd = 16.dp),
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
            Row(verticalAlignment = Alignment.CenterVertically) {
                FilterChip(
                    selected = state.touchInput,
                    onClick = { repo.setTouchInput(!state.touchInput) },
                    label = { Text(if (state.touchInput) "Touch controls the PC" else "Touch input off (view only)") },
                )
            }
        }
    }
}

@Composable
fun StreamStats(state: ClientState) {
    val s = state.stream ?: return
    val m = state.metrics
    val rtt = m.rttMs?.let { "%.0f ms".format(it) } ?: "–"
    val loss = m.lossPercent?.let { "%.1f %%".format(it) } ?: "–"
    Text(
        "${s.codec} ${s.width}×${s.height} · %.0f fps · %.1f Mbit/s · RTT $rtt · loss $loss · ${m.health.name.lowercase()}"
            .format(m.renderFps, m.bitrateMbps),
        style = MaterialTheme.typography.bodyMedium,
    )
}
