package fr.ryoku.tvremote.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.ExitToApp
import androidx.compose.material.icons.automirrored.filled.List
import androidx.compose.material.icons.filled.Home
import androidx.compose.material.icons.filled.Info
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.filled.Menu
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowLeft
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import fr.ryoku.tvremote.MainViewModel
import fr.ryoku.tvremote.core.RemoteKey
import fr.ryoku.tvremote.core.TvPower
import fr.ryoku.tvremote.logic.UiState
import fr.ryoku.tvremote.logic.statusLine

@Composable
fun RemoteScreen(state: UiState, vm: MainViewModel, onOpenTvs: () -> Unit, onKeyboard: () -> Unit) {
    var numbers by remember { mutableStateOf(false) }
    var volumeDialog by remember { mutableStateOf(false) }
    val key: (RemoteKey) -> Unit = vm::key

    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp, vertical = 12.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        Header(state, onOpenTvs = onOpenTvs, onPower = vm::power)

        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            val m = Modifier.weight(1f).height(56.dp)
            KeyButton({ key(RemoteKey.SOURCE) }, m, label = "Source", icon = Glyphs.Input)
            KeyButton({ key(RemoteKey.HOME) }, m, label = "Home", icon = Icons.Filled.Home)
            KeyButton({ key(RemoteKey.MENU) }, m, label = "Settings", icon = Icons.Filled.Settings)
            KeyButton(onKeyboard, m, label = "Text", icon = Glyphs.Keyboard)
        }

        DPad(key)

        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            val m = Modifier.weight(1f).height(52.dp)
            KeyButton({ key(RemoteKey.RETURN) }, m, label = "Back", icon = Icons.AutoMirrored.Filled.ArrowBack)
            KeyButton({ key(RemoteKey.INFO) }, m, label = "Info", icon = Icons.Filled.Info)
            KeyButton({ key(RemoteKey.TOOLS) }, m, label = "Options", icon = Icons.Filled.Menu)
            KeyButton({ key(RemoteKey.EXIT) }, m, label = "Exit", icon = Icons.AutoMirrored.Filled.ExitToApp)
        }

        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Rocker("VOL", up = { key(RemoteKey.VOLUME_UP) }, down = { key(RemoteKey.VOLUME_DOWN) })
            Column(
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                val muted = state.muted == true
                KeyButton(
                    vm::toggleMute,
                    Modifier.size(width = 96.dp, height = 48.dp),
                    label = if (muted) "Muted" else "Mute",
                    icon = if (muted) Glyphs.VolumeOff else Glyphs.VolumeUp,
                    container = if (muted) MaterialTheme.colorScheme.errorContainer else MaterialTheme.colorScheme.secondaryContainer,
                )
                Text(
                    state.volume?.let { "Volume $it" } ?: "Volume –",
                    style = MaterialTheme.typography.labelMedium,
                    modifier = Modifier
                        .clickable(enabled = state.volume != null) { volumeDialog = true }
                        .padding(6.dp),
                )
                KeyButton({ numbers = true }, Modifier.size(width = 96.dp, height = 48.dp), label = "123", icon = Glyphs.Dialpad)
            }
            Rocker("CH", up = { key(RemoteKey.CHANNEL_UP) }, down = { key(RemoteKey.CHANNEL_DOWN) })
        }

        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            val m = Modifier.weight(1f).height(48.dp)
            KeyButton({ key(RemoteKey.REWIND) }, m, icon = Glyphs.Rewind, description = "Rewind")
            KeyButton({ key(RemoteKey.PLAY) }, m, icon = Glyphs.Play, description = "Play")
            KeyButton({ key(RemoteKey.PAUSE) }, m, icon = Glyphs.Pause, description = "Pause")
            KeyButton({ key(RemoteKey.STOP) }, m, icon = Glyphs.Stop, description = "Stop")
            KeyButton({ key(RemoteKey.FAST_FORWARD) }, m, icon = Glyphs.FastForward, description = "Fast forward")
        }
        Spacer(Modifier.height(8.dp))
    }

    if (numbers) NumberPad(key, onDismiss = { numbers = false })
    if (volumeDialog) VolumeDialog(state.volume ?: 0, onSet = vm::setVolume, onDismiss = { volumeDialog = false })
}

@Composable
private fun Header(state: UiState, onOpenTvs: () -> Unit, onPower: () -> Unit) {
    val tv = state.active
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Column(
            Modifier
                .weight(1f)
                .clickable(onClick = onOpenTvs)
                .padding(vertical = 4.dp),
        ) {
            Text(
                tv?.name ?: "No TV",
                style = MaterialTheme.typography.titleLarge,
                fontWeight = FontWeight.SemiBold,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                statusLine(tv, state.connection, state.power),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 2,
            )
        }
        val on = state.power == TvPower.ON
        Box(contentAlignment = Alignment.Center) {
            KeyButton(
                onPower,
                Modifier.size(60.dp),
                icon = Glyphs.Power,
                description = if (on) "Turn the TV off" else "Turn the TV on",
                shape = CircleShape,
                container = if (on) Color(0xFFC62828) else MaterialTheme.colorScheme.tertiaryContainer,
                content = if (on) Color.White else MaterialTheme.colorScheme.onTertiaryContainer,
                enabled = !state.powerBusy && tv != null,
            )
            if (state.powerBusy) CircularProgressIndicator(Modifier.size(64.dp), strokeWidth = 3.dp)
        }
    }
}

@Composable
private fun DPad(key: (RemoteKey) -> Unit) {
    Box(
        Modifier
            .fillMaxWidth(0.82f)
            .aspectRatio(1f)
            .background(MaterialTheme.colorScheme.surfaceContainerHigh, CircleShape),
        contentAlignment = Alignment.Center,
    ) {
        val arrow = Modifier.size(84.dp)
        val shape = RoundedCornerShape(28.dp)
        val bg = MaterialTheme.colorScheme.surfaceContainerHigh
        val fg = MaterialTheme.colorScheme.onSurface
        KeyButton({ key(RemoteKey.UP) }, arrow.align(Alignment.TopCenter), icon = Icons.Filled.KeyboardArrowUp, description = "Up", repeat = true, shape = shape, container = bg, content = fg)
        KeyButton({ key(RemoteKey.DOWN) }, arrow.align(Alignment.BottomCenter), icon = Icons.Filled.KeyboardArrowDown, description = "Down", repeat = true, shape = shape, container = bg, content = fg)
        KeyButton({ key(RemoteKey.LEFT) }, arrow.align(Alignment.CenterStart), icon = Icons.AutoMirrored.Filled.KeyboardArrowLeft, description = "Left", repeat = true, shape = shape, container = bg, content = fg)
        KeyButton({ key(RemoteKey.RIGHT) }, arrow.align(Alignment.CenterEnd), icon = Icons.AutoMirrored.Filled.KeyboardArrowRight, description = "Right", repeat = true, shape = shape, container = bg, content = fg)
        KeyButton(
            { key(RemoteKey.ENTER) },
            Modifier.fillMaxSize(0.42f),
            label = "OK",
            shape = CircleShape,
            container = MaterialTheme.colorScheme.primaryContainer,
            content = MaterialTheme.colorScheme.onPrimaryContainer,
        )
    }
}

/** A vertical +/− pill (volume, channel); holding repeats. */
@Composable
private fun Rocker(label: String, up: () -> Unit, down: () -> Unit) {
    Column(
        Modifier
            .width(76.dp)
            .background(MaterialTheme.colorScheme.secondaryContainer, RoundedCornerShape(38.dp)),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        val shape = RoundedCornerShape(38.dp)
        KeyButton(up, Modifier.size(76.dp), label = "+", description = "$label up", repeat = true, shape = shape)
        Text(label, style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSecondaryContainer)
        KeyButton(down, Modifier.size(76.dp), label = "−", description = "$label down", repeat = true, shape = shape)
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun NumberPad(key: (RemoteKey) -> Unit, onDismiss: () -> Unit) {
    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 24.dp)
                .padding(bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            val digits = listOf(
                listOf(RemoteKey.NUM1, RemoteKey.NUM2, RemoteKey.NUM3),
                listOf(RemoteKey.NUM4, RemoteKey.NUM5, RemoteKey.NUM6),
                listOf(RemoteKey.NUM7, RemoteKey.NUM8, RemoteKey.NUM9),
            )
            for (row in digits) {
                Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    for (k in row) {
                        KeyButton({ key(k) }, Modifier.weight(1f).height(56.dp), label = k.name.removePrefix("NUM"))
                    }
                }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                val m = Modifier.weight(1f).height(56.dp)
                KeyButton({ key(RemoteKey.PREVIOUS_CHANNEL) }, m, label = "Prev ch")
                KeyButton({ key(RemoteKey.NUM0) }, m, label = "0")
                KeyButton({ key(RemoteKey.CHANNEL_LIST) }, m, label = "Ch list", icon = Icons.AutoMirrored.Filled.List)
            }
            Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                val m = Modifier.weight(1f).height(48.dp)
                KeyButton({ key(RemoteKey.GUIDE) }, m, label = "Guide")
                KeyButton({ key(RemoteKey.POWER_OFF) }, m, label = "Power off")
            }
            Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                val m = Modifier.weight(1f).height(40.dp)
                KeyButton({ key(RemoteKey.RED) }, m, description = "Red", container = Color(0xFFD32F2F), shape = RoundedCornerShape(12.dp))
                KeyButton({ key(RemoteKey.GREEN) }, m, description = "Green", container = Color(0xFF388E3C), shape = RoundedCornerShape(12.dp))
                KeyButton({ key(RemoteKey.YELLOW) }, m, description = "Yellow", container = Color(0xFFFBC02D), shape = RoundedCornerShape(12.dp))
                KeyButton({ key(RemoteKey.BLUE) }, m, description = "Blue", container = Color(0xFF1976D2), shape = RoundedCornerShape(12.dp))
            }
        }
    }
}

@Composable
private fun VolumeDialog(initial: Int, onSet: (Int) -> Unit, onDismiss: () -> Unit) {
    var value by remember { mutableFloatStateOf(initial.toFloat()) }
    AlertDialog(
        onDismissRequest = onDismiss,
        confirmButton = { TextButton(onClick = onDismiss) { Text("Done") } },
        title = { Text("Volume ${value.toInt()}") },
        text = {
            Slider(
                value = value,
                onValueChange = {
                    value = it
                    onSet(it.toInt())
                },
                valueRange = 0f..100f,
            )
        },
    )
}
