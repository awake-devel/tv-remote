package fr.ryoku.tvremote.ui

import android.view.HapticFeedbackConstants
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.waitForUpOrCancellation
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.onClick
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

/** A key tick on the phone, like a hardware remote's click. */
@Composable
fun rememberHaptic(): () -> Unit {
    val view = LocalView.current
    return remember(view) { { view.performHapticFeedback(HapticFeedbackConstants.VIRTUAL_KEY) } }
}

/**
 * A remote button: fires on touch down (feels immediate), with a haptic tick. With
 * [repeat], holding it repeats the action (volume, channel, D-pad).
 */
@Composable
fun KeyButton(
    onPress: () -> Unit,
    modifier: Modifier = Modifier,
    label: String? = null,
    icon: ImageVector? = null,
    description: String = label ?: "",
    repeat: Boolean = false,
    shape: Shape = MaterialTheme.shapes.large,
    container: Color = MaterialTheme.colorScheme.secondaryContainer,
    content: Color = MaterialTheme.colorScheme.onSecondaryContainer,
    enabled: Boolean = true,
    iconSize: Dp = 24.dp,
    labelSize: TextUnit = TextUnit.Unspecified,
) {
    val haptic = rememberHaptic()
    val action by rememberUpdatedState(onPress)
    val scope = rememberCoroutineScope()
    var pressed by remember { mutableStateOf(false) }
    Surface(
        shape = shape,
        color = if (pressed) MaterialTheme.colorScheme.primary else container,
        contentColor = if (pressed) MaterialTheme.colorScheme.onPrimary else content,
        // Disabled: dimmed (no TV yet, or a power command is running).
        modifier = modifier
            .alpha(if (enabled) 1f else 0.45f)
            .semantics {
                role = Role.Button
                contentDescription = description
                onClick { action(); true }
            }
            .pointerInput(enabled, repeat) {
                if (!enabled) return@pointerInput
                awaitEachGesture {
                    awaitFirstDown(requireUnconsumed = false)
                    pressed = true
                    haptic()
                    action()
                    var job: Job? = null
                    if (repeat) {
                        job = scope.launch {
                            delay(400)
                            while (isActive) {
                                action()
                                delay(130)
                            }
                        }
                    }
                    waitForUpOrCancellation()
                    job?.cancel()
                    pressed = false
                }
            },
    ) {
        Box(contentAlignment = Alignment.Center) {
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                if (icon != null) Icon(icon, contentDescription = null, modifier = Modifier.size(iconSize))
                if (label != null) {
                    Text(
                        label,
                        fontSize = if (labelSize != TextUnit.Unspecified) labelSize else if (icon != null) 11.sp else 15.sp,
                        fontWeight = FontWeight.Medium,
                        maxLines = 1,
                    )
                }
            }
        }
    }
}
