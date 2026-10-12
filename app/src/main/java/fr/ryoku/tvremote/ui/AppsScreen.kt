package fr.ryoku.tvremote.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.TextButton
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import fr.ryoku.tvremote.MainViewModel
import fr.ryoku.tvremote.logic.UiState

@Composable
fun AppsScreen(state: UiState, vm: MainViewModel) {
    val haptic = rememberHaptic()
    var addingId by remember { mutableStateOf(false) }
    if (addingId) {
        AddAppIdDialog(onAdd = { vm.addAppId(it); addingId = false }, onDismiss = { addingId = false })
    }
    LaunchedEffect(state.activeId) {
        if (state.activeId != null && state.apps.isEmpty()) vm.loadApps()
    }
    Column(Modifier.fillMaxSize().padding(horizontal = 16.dp)) {
        Row(
            Modifier.fillMaxWidth().padding(vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("Apps", style = MaterialTheme.typography.titleLarge, modifier = Modifier.weight(1f))
            IconButton(onClick = vm::loadApps, enabled = !state.appsLoading && state.activeId != null) {
                Icon(Icons.Filled.Refresh, contentDescription = "Reload the list")
            }
        }
        Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            FilledTonalButton(onClick = { haptic(); vm.youTubeApp() }, Modifier.weight(1f)) { Text("YouTube") }
            FilledTonalButton(onClick = { haptic(); vm.netflix() }, Modifier.weight(1f)) { Text("Netflix") }
        }
        Text(
            "Apps the TV confirms from a list of known app ids: the TV gives no full list, so a " +
                "few (mostly system apps) can be missing. Tip: share a YouTube link to “Play on TV”.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(vertical = 8.dp),
        )
        TextButton(onClick = { addingId = true }, enabled = state.activeId != null) { Text("Missing an app? Add it by id") }
        when {
            state.activeId == null -> Centered("Add a TV first (TVs tab).")
            state.appsLoading && state.apps.isEmpty() -> Box(Modifier.fillMaxSize(), Alignment.Center) { CircularProgressIndicator() }
            state.appsError != null && state.apps.isEmpty() -> Column(
                Modifier.fillMaxSize(),
                verticalArrangement = Arrangement.Center,
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Text(
                    "Could not list the apps: ${state.appsError}\nThe TV must be on.",
                    textAlign = TextAlign.Center,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Button(onClick = vm::loadApps, Modifier.padding(top = 12.dp)) { Text("Try again") }
            }
            else -> LazyVerticalGrid(
                columns = GridCells.Adaptive(150.dp),
                contentPadding = PaddingValues(bottom = 16.dp),
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                items(state.apps, key = { it.appId }) { app ->
                    ElevatedCard(onClick = { haptic(); vm.launch(app) }, Modifier.fillMaxWidth().height(72.dp)) {
                        Box(Modifier.fillMaxSize().padding(12.dp), contentAlignment = Alignment.CenterStart) {
                            Text(app.name, maxLines = 2, overflow = TextOverflow.Ellipsis, style = MaterialTheme.typography.titleSmall)
                        }
                    }
                }
            }
        }
    }
}

@Composable
fun Centered(text: String) {
    Box(Modifier.fillMaxSize().padding(24.dp), contentAlignment = Alignment.Center) {
        Text(text, textAlign = TextAlign.Center, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

@Composable
private fun AddAppIdDialog(onAdd: (String) -> Unit, onDismiss: () -> Unit) {
    var id by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Add an app by id") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    "The Tizen app id, e.g. 3201907018807 for Netflix (shown in the app's page on apps.samsung.com).",
                    style = MaterialTheme.typography.bodySmall,
                )
                OutlinedTextField(id, { id = it.trim() }, label = { Text("App id") }, singleLine = true)
            }
        },
        confirmButton = { TextButton(onClick = { onAdd(id) }, enabled = id.isNotBlank()) { Text("Add") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}
