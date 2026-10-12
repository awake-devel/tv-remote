package fr.ryoku.tvremote.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.Search
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import fr.ryoku.tvremote.MainViewModel
import fr.ryoku.tvremote.core.FoundTv
import fr.ryoku.tvremote.core.TvPower
import fr.ryoku.tvremote.core.TvSummary
import fr.ryoku.tvremote.logic.UiState

@Composable
fun TvsScreen(state: UiState, vm: MainViewModel) {
    var adding by remember { mutableStateOf(false) }
    var renaming by remember { mutableStateOf<TvSummary?>(null) }
    var removing by remember { mutableStateOf<TvSummary?>(null) }

    LazyColumn(
        Modifier.fillMaxSize().padding(horizontal = 16.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        item {
            Text("Your TVs", style = MaterialTheme.typography.titleLarge, modifier = Modifier.padding(top = 8.dp))
        }
        if (state.tvs.isEmpty()) {
            item {
                Text(
                    "No TV yet. Turn the TV on, connect the phone to the same Wi-Fi, then search.",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        items(state.tvs, key = { it.id }) { tv ->
            TvCard(
                tv,
                power = state.powers[tv.id],
                onSelect = { vm.select(tv.id) },
                onPair = { vm.pair(tv.id) },
                onRename = { renaming = tv },
                onRemove = { removing = tv },
            )
        }
        item {
            Row(
                Modifier.fillMaxWidth().padding(top = 8.dp),
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Button(onClick = vm::scan, enabled = !state.scanning, modifier = Modifier.weight(1f)) {
                    if (state.scanning) {
                        CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
                    } else {
                        Icon(Icons.Filled.Search, contentDescription = null)
                    }
                    Spacer(Modifier.size(8.dp))
                    Text(if (state.scanning) "Searching…" else "Search the network")
                }
                OutlinedButton(onClick = { adding = true }) {
                    Icon(Icons.Filled.Add, contentDescription = null)
                    Spacer(Modifier.size(4.dp))
                    Text("By IP")
                }
            }
        }
        state.scanError?.let { err ->
            item { Text(err, color = MaterialTheme.colorScheme.error) }
        }
        val fresh = state.found.filter { it.knownId == null }
        if (state.scanned && !state.scanning) {
            if (fresh.isEmpty()) {
                item {
                    Text(
                        if (state.found.isEmpty()) "No Samsung TV answered. Is it on, on this Wi-Fi?" else "No other TV found.",
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            } else {
                item { Text("Found", style = MaterialTheme.typography.titleMedium) }
            }
        }
        items(fresh, key = { "found:" + it.key }) { found -> FoundCard(found, onAdd = { vm.addFound(found.key) }) }
        item { Spacer(Modifier.size(24.dp)) }
    }

    if (adding) AddByIpDialog(onAdd = { ip, name -> vm.addHost(ip, name); adding = false }, onDismiss = { adding = false })
    renaming?.let { tv ->
        RenameDialog(tv, onRename = { vm.rename(tv.id, it); renaming = null }, onDismiss = { renaming = null })
    }
    removing?.let { tv ->
        AlertDialog(
            onDismissRequest = { removing = null },
            title = { Text("Remove ${tv.name}?") },
            text = { Text("Its pairing is forgotten: the TV will ask again next time.") },
            confirmButton = { TextButton(onClick = { vm.remove(tv.id); removing = null }) { Text("Remove") } },
            dismissButton = { TextButton(onClick = { removing = null }) { Text("Cancel") } },
        )
    }
}

@Composable
private fun TvCard(
    tv: TvSummary,
    power: TvPower?,
    onSelect: () -> Unit,
    onPair: () -> Unit,
    onRename: () -> Unit,
    onRemove: () -> Unit,
) {
    ElevatedCard(onClick = onSelect, modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                if (tv.active) {
                    Icon(
                        Icons.Filled.CheckCircle,
                        contentDescription = "Active",
                        tint = MaterialTheme.colorScheme.primary,
                        modifier = Modifier.padding(end = 8.dp),
                    )
                }
                Text(tv.name, style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
                IconButton(onClick = onRename) { Icon(Icons.Filled.Edit, contentDescription = "Rename") }
                IconButton(onClick = onRemove) { Icon(Icons.Filled.Delete, contentDescription = "Remove") }
            }
            val details = listOfNotNull(
                tv.host.ifEmpty { null },
                tv.model,
                when (power) {
                    TvPower.ON -> "on"
                    TvPower.STANDBY -> "standby"
                    TvPower.OFF -> "off"
                    else -> null
                },
            ).joinToString(" · ")
            Text(details, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Text(
                buildString {
                    append(if (tv.paired) "Paired" else "Not paired")
                    append(if (tv.mac != null) " · power on by Wi-Fi ready" else " · MAC unknown (power on needs it)")
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            if (!tv.paired) {
                FilledTonalButton(onClick = onPair, modifier = Modifier.padding(top = 6.dp)) {
                    Text("Pair: a prompt appears on the TV")
                }
            }
        }
    }
}

@Composable
private fun FoundCard(tv: FoundTv, onAdd: () -> Unit) {
    ElevatedCard(Modifier.fillMaxWidth()) {
        Row(Modifier.padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(tv.name, style = MaterialTheme.typography.titleMedium)
                Text(
                    listOfNotNull(tv.ip, tv.model).joinToString(" · "),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Button(onClick = onAdd) { Text("Add") }
        }
    }
}

@Composable
private fun AddByIpDialog(onAdd: (String, String?) -> Unit, onDismiss: () -> Unit) {
    var ip by remember { mutableStateOf("") }
    var name by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Add a TV by address") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    ip,
                    { ip = it.trim() },
                    label = { Text("IP address") },
                    placeholder = { Text("192.168.1.20") },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                )
                OutlinedTextField(name, { name = it }, label = { Text("Name (optional)") }, singleLine = true)
            }
        },
        confirmButton = { TextButton(onClick = { onAdd(ip, name) }, enabled = ip.isNotBlank()) { Text("Add") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun RenameDialog(tv: TvSummary, onRename: (String) -> Unit, onDismiss: () -> Unit) {
    var name by remember { mutableStateOf(tv.name) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Rename") },
        text = { OutlinedTextField(name, { name = it }, singleLine = true) },
        confirmButton = { TextButton(onClick = { onRename(name) }, enabled = name.isNotBlank()) { Text("Save") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}
