package dev.secondscrn.client.ui

import android.content.Intent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import dev.secondscrn.client.ClientRepository
import uniffi.secondscrn_client.ClientState
import uniffi.secondscrn_client.HostItem
import uniffi.secondscrn_client.Phase
import uniffi.secondscrn_client.Preset

private const val DEFAULT_PORT = 47800

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HomeScreen(state: ClientState, repo: ClientRepository) {
    var manual by rememberSaveable { mutableStateOf(false) }
    Scaffold(topBar = { TopAppBar(title = { Text("SecondScrn") }) }) { padding ->
        LazyColumn(
            Modifier.fillMaxSize().padding(padding),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(24.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            item { ConnectionBanner(state, repo) }
            item {
                Row(Modifier.widthIn(max = 900.dp).fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                    Text("Computers", style = MaterialTheme.typography.titleLarge, modifier = Modifier.weight(1f))
                    OutlinedButton(onClick = { manual = true }) { Text("Connect manually") }
                }
            }
            if (state.hosts.isEmpty()) {
                item { EmptyHosts() }
            }
            items(state.hosts, key = { it.fingerprint }) { host -> HostCard(host, state, repo) }
            item { SettingsCard(state, repo) }
            item { TroubleshootingCard(state, repo) }
        }
    }
    if (manual) {
        ManualConnectDialog(onDismiss = { manual = false }) { address, port ->
            manual = false
            repo.connect(address, port, null)
        }
    }
}

@Composable
private fun ConnectionBanner(state: ClientState, repo: ClientRepository) {
    val (text, busy) = when (val p = state.phase) {
        is Phase.Connecting -> "Connecting to ${state.hostName ?: "PC"}…" to true
        is Phase.Negotiating -> "Setting up the display…" to true
        is Phase.Reconnecting -> "Reconnecting to ${state.hostName ?: "PC"} (attempt ${p.attempt})…" to true
        is Phase.Failed -> p.message to false
        else -> return
    }
    val failed = state.phase is Phase.Failed
    Card(
        Modifier.widthIn(max = 900.dp).fillMaxWidth(),
        colors = if (failed) CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.errorContainer)
        else CardDefaults.cardColors(),
    ) {
        Row(Modifier.padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
            if (busy) {
                CircularProgressIndicator(Modifier.size(24.dp), strokeWidth = 3.dp)
                Spacer(Modifier.width(16.dp))
            }
            Text(text, Modifier.weight(1f), style = MaterialTheme.typography.bodyLarge)
            val needsPairing = (state.phase as? Phase.Failed)?.needsPairing == true
            val host = state.hosts.firstOrNull { it.name == state.hostName && it.paired }
            if (needsPairing && host != null) {
                Button(onClick = {
                    repo.forget(host.fingerprint)
                    repo.connect(host.address, host.port.toInt(), null)
                }) { Text("Pair again") }
            } else if (failed && host != null) {
                Button(onClick = { repo.connect(host.address, host.port.toInt(), host.fingerprint) }) { Text("Retry") }
            } else if (busy) {
                TextButton(onClick = repo::disconnect) { Text("Cancel") }
            }
        }
    }
}

@Composable
private fun EmptyHosts() {
    Card(Modifier.widthIn(max = 900.dp).fillMaxWidth()) {
        Column(Modifier.padding(24.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text("Looking for PCs on this network…", style = MaterialTheme.typography.titleMedium)
            Text(
                "Start SecondScrn on your Windows PC and make sure both devices are on the same network. " +
                    "If your PC doesn't appear, use “Connect manually” with the address shown in the PC app.",
                style = MaterialTheme.typography.bodyMedium,
            )
        }
    }
}

@Composable
private fun HostCard(host: HostItem, state: ClientState, repo: ClientRepository) {
    var menu by remember { mutableStateOf(false) }
    val busy = state.phase !is Phase.Idle && state.phase !is Phase.Failed
    Card(Modifier.widthIn(max = 900.dp).fillMaxWidth()) {
        Row(Modifier.padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(host.name, style = MaterialTheme.typography.titleMedium)
                val status = buildList {
                    add(if (host.paired) "Paired" else "New — pairing required")
                    add(if (host.discovered) "online" else "last seen at ${host.address}")
                    add("ID ${host.fingerprintShort}")
                }.joinToString(" · ")
                Text(status, style = MaterialTheme.typography.bodyMedium)
            }
            Button(enabled = !busy, onClick = {
                repo.connect(host.address, host.port.toInt(), host.fingerprint.takeIf { host.paired })
            }) { Text(if (host.paired) "Connect" else "Pair") }
            if (host.paired) {
                Box {
                    IconButton(onClick = { menu = true }) { Icon(Icons.Default.MoreVert, contentDescription = "More options") }
                    DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                        DropdownMenuItem(
                            text = { Text(if (host.autoReconnect) "Don't connect automatically" else "Connect automatically") },
                            onClick = {
                                menu = false
                                repo.setAutoReconnect(host.fingerprint, !host.autoReconnect)
                            },
                        )
                        DropdownMenuItem(text = { Text("Forget this PC") }, onClick = {
                            menu = false
                            repo.forget(host.fingerprint)
                        })
                    }
                }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PresetSelector(current: Preset, onSelect: (Preset) -> Unit) {
    val options = listOf(
        Preset.AUTOMATIC to "Automatic",
        Preset.SHARP to "High quality",
        Preset.SMOOTH to "Smooth",
        Preset.LOW_BANDWIDTH to "Weak Wi-Fi",
    )
    SingleChoiceSegmentedButtonRow {
        options.forEachIndexed { i, (preset, label) ->
            SegmentedButton(
                selected = current == preset,
                onClick = { onSelect(preset) },
                shape = SegmentedButtonDefaults.itemShape(i, options.size),
            ) { Text(label) }
        }
    }
}

@Composable
private fun SettingsCard(state: ClientState, repo: ClientRepository) {
    Card(Modifier.widthIn(max = 900.dp).fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text("Settings", style = MaterialTheme.typography.titleMedium)
            Text("Preset", style = MaterialTheme.typography.labelLarge)
            PresetSelector(state.preset, repo::setPreset)
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text("Control the PC with touch and pen", style = MaterialTheme.typography.bodyLarge)
                    Text(
                        "While streaming, the handle at the top edge (or Back) shows the session options.",
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
                Switch(checked = state.touchInput, onCheckedChange = repo::setTouchInput)
            }
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text("Touch as mouse", style = MaterialTheme.typography.bodyLarge)
                    Text(
                        "Off: fingers are real touch on the PC (Windows gestures). On: tap to click, " +
                            "long-press to right-click, two fingers to scroll. A pen always works as a pen.",
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
                Switch(
                    checked = state.touchAsMouse,
                    enabled = state.touchInput,
                    onCheckedChange = repo::setTouchAsMouse,
                )
            }
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text("Lock rotation", style = MaterialTheme.typography.bodyLarge)
                    Text("Keep the current orientation while SecondScrn is open.", style = MaterialTheme.typography.bodySmall)
                }
                Switch(checked = state.rotationLocked, onCheckedChange = repo::setRotationLocked)
            }
        }
    }
}

@Composable
private fun TroubleshootingCard(state: ClientState, repo: ClientRepository) {
    val context = LocalContext.current
    Card(Modifier.widthIn(max = 900.dp).fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text("Troubleshooting", style = MaterialTheme.typography.titleMedium)
            Text("This tablet's ID: ${state.deviceFingerprintShort}", style = MaterialTheme.typography.bodyMedium)
            Text("Video decoders:", style = MaterialTheme.typography.labelLarge)
            if (state.decoders.isEmpty()) Text("Checking…", style = MaterialTheme.typography.bodySmall)
            state.decoders.forEach { Text("• $it", style = MaterialTheme.typography.bodySmall) }
            Spacer(Modifier.height(4.dp))
            OutlinedButton(onClick = {
                val file = repo.exportDiagnostics()
                val uri = FileProvider.getUriForFile(context, "${context.packageName}.files", file)
                val send = Intent(Intent.ACTION_SEND).apply {
                    type = "application/json"
                    putExtra(Intent.EXTRA_STREAM, uri)
                    addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                }
                context.startActivity(Intent.createChooser(send, "Share diagnostics"))
            }) { Text("Share diagnostics") }
            Text(
                "Diagnostics contain connection statistics and logs. Keys, codes and identities are removed.",
                style = MaterialTheme.typography.bodySmall,
            )
        }
    }
}

@Composable
private fun ManualConnectDialog(onDismiss: () -> Unit, onConnect: (String, Int) -> Unit) {
    var address by rememberSaveable { mutableStateOf("") }
    var port by rememberSaveable { mutableStateOf(DEFAULT_PORT.toString()) }
    val portValue = port.toIntOrNull()?.takeIf { it in 1..65535 }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Connect manually") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Text("Enter the address shown in the SecondScrn app on your PC.")
                OutlinedTextField(
                    value = address,
                    onValueChange = { address = it.trim() },
                    label = { Text("PC address") },
                    placeholder = { Text("192.168.1.20") },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                )
                OutlinedTextField(
                    value = port,
                    onValueChange = { port = it.filter(Char::isDigit).take(5) },
                    label = { Text("Port") },
                    singleLine = true,
                    isError = portValue == null,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                )
            }
        },
        confirmButton = {
            Button(enabled = address.isNotBlank() && portValue != null, onClick = { onConnect(address, portValue!!) }) {
                Text("Connect")
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}
