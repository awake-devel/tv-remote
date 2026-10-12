package fr.ryoku.tvremote.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Home
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import fr.ryoku.tvremote.MainViewModel
import fr.ryoku.tvremote.core.RemoteKey

/** Drag to move the TV's pointer, tap to click (works where the TV shows a pointer, e.g. the browser). */
@Composable
fun TouchpadScreen(vm: MainViewModel, onKeyboard: () -> Unit) {
    val haptic = rememberHaptic()
    Column(
        Modifier
            .fillMaxSize()
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Box(
            Modifier
                .weight(1f)
                .fillMaxWidth()
                .background(MaterialTheme.colorScheme.surfaceContainerHigh, RoundedCornerShape(28.dp))
                .pointerInput(Unit) {
                    detectTapGestures(onTap = {
                        haptic()
                        vm.pointerClick()
                    })
                }
                .pointerInput(Unit) {
                    detectDragGestures { change, drag ->
                        change.consume()
                        vm.pointerMove(drag.x / density, drag.y / density)
                    }
                },
            contentAlignment = Alignment.Center,
        ) {
            Text(
                "Drag to move the pointer\nTap to click",
                textAlign = TextAlign.Center,
                style = MaterialTheme.typography.bodyLarge,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            val m = Modifier.weight(1f).height(56.dp)
            KeyButton({ vm.key(RemoteKey.RETURN) }, m, label = "Back", icon = Icons.AutoMirrored.Filled.ArrowBack)
            KeyButton({ vm.key(RemoteKey.HOME) }, m, label = "Home", icon = Icons.Filled.Home)
            KeyButton(onKeyboard, m, label = "Text", icon = Glyphs.Keyboard)
        }
    }
}
