package dev.secondscrn.client.ui

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.secondscrn.client.ClientRepository
import uniffi.secondscrn_client.Phase

@Composable
fun SecondScrnRoot(repo: ClientRepository, onStreamingChanged: (Boolean) -> Unit) {
    val state by repo.state.collectAsStateWithLifecycle()
    val s = state
    if (s == null) {
        Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) { CircularProgressIndicator() }
        LaunchedEffect(Unit) { repo.client } // starts the engine
        return
    }
    // Keep showing the desktop (last frame) while reconnecting.
    val showStream = s.stream != null && when (s.phase) {
        is Phase.Streaming, is Phase.Interrupted, is Phase.Reconnecting, is Phase.Negotiating -> true
        else -> false
    }
    LaunchedEffect(showStream) { onStreamingChanged(showStream) }
    when {
        s.phase is Phase.Pairing -> PairingScreen(s, onCancel = repo::disconnect)
        showStream -> StreamScreen(s, repo)
        else -> HomeScreen(s, repo)
    }
}
