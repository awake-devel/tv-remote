package fr.ryoku.tvremote

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import fr.ryoku.tvremote.logic.UiState
import fr.ryoku.tvremote.logic.youTubeLink
import fr.ryoku.tvremote.ui.AppsScreen
import fr.ryoku.tvremote.ui.Centered
import fr.ryoku.tvremote.ui.Glyphs
import fr.ryoku.tvremote.ui.RemoteScreen
import fr.ryoku.tvremote.ui.TouchpadScreen
import fr.ryoku.tvremote.ui.TvsScreen

/** Android 17: local network protection (ACCESS_LOCAL_NETWORK). */
private const val LOCAL_NETWORK_SDK = 37

private enum class Tab(val label: String, val icon: ImageVector) {
    Remote("Remote", Glyphs.Tv),
    Touchpad("Touchpad", Glyphs.Mouse),
    Apps("Apps", Glyphs.Apps),
    Tvs("TVs", Glyphs.Input),
}

class MainActivity : ComponentActivity() {
    private val vm: MainViewModel by viewModels()
    private var askedLan = false

    private val lanPermission = registerForActivityResult(ActivityResultContracts.RequestPermission()) {
        vm.onLanPermission(it)
    }

    private fun lanGranted(): Boolean =
        Backends.lanGranted?.invoke(this) ?: (
            Build.VERSION.SDK_INT < LOCAL_NETWORK_SDK ||
                checkSelfPermission(Manifest.permission.ACCESS_LOCAL_NETWORK) == PackageManager.PERMISSION_GRANTED
            )

    /** Android 17+ asks before an app may reach devices on the local network (the TV). */
    fun requestLan() {
        if (lanGranted()) vm.onLanPermission(true) else lanPermission.launch(Manifest.permission.ACCESS_LOCAL_NETWORK)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        if (savedInstanceState == null) handleShare(intent)
        setContent {
            AppTheme {
                val state by vm.state.collectAsStateWithLifecycle()
                Root(state, vm, onRequestLan = ::requestLan)
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleShare(intent)
    }

    override fun onStart() {
        super.onStart()
        val granted = lanGranted()
        vm.onLanPermission(granted)
        if (!granted && !askedLan) {
            askedLan = true
            requestLan()
        }
        vm.onForeground()
    }

    override fun onStop() {
        vm.onBackground()
        super.onStop()
    }

    /** "Share → Play on TV" with a YouTube link. */
    private fun handleShare(intent: Intent?) {
        if (intent?.action != Intent.ACTION_SEND) return
        val text = intent.getStringExtra(Intent.EXTRA_TEXT) ?: return
        youTubeLink(text)?.let(vm::playYouTube)
    }
}

@Composable
private fun Root(state: UiState, vm: MainViewModel, onRequestLan: () -> Unit) {
    var tab by rememberSaveable { mutableStateOf(Tab.Remote) }
    var typing by remember { mutableStateOf(false) }
    val snackbar = remember { SnackbarHostState() }

    LaunchedEffect(Unit) { vm.messages.collect { snackbar.showSnackbar(it) } }
    // First run: go where a TV can be added.
    LaunchedEffect(state.ready, state.tvs.isEmpty()) {
        if (state.ready && state.tvs.isEmpty()) tab = Tab.Tvs
    }
    // The TV opened its keyboard: offer text entry.
    LaunchedEffect(state.keyboardShown) { if (state.keyboardShown) typing = true }

    Scaffold(
        snackbarHost = { SnackbarHost(snackbar) },
        bottomBar = {
            NavigationBar {
                Tab.entries.forEach { t ->
                    NavigationBarItem(
                        selected = tab == t,
                        onClick = { tab = t },
                        icon = { Icon(t.icon, contentDescription = null) },
                        label = { Text(t.label) },
                    )
                }
            }
        },
    ) { padding ->
        Box(Modifier.padding(padding)) {
            when {
                !state.ready -> Centered("Starting…")
                state.fatal != null -> Centered(state.fatal)
                else -> when (tab) {
                    Tab.Remote -> RemoteScreen(state, vm, onOpenTvs = { tab = Tab.Tvs }, onKeyboard = { typing = true })
                    Tab.Touchpad -> TouchpadScreen(vm, onKeyboard = { typing = true })
                    Tab.Apps -> AppsScreen(state, vm)
                    Tab.Tvs -> TvsScreen(state, vm, onRequestLan)
                }
            }
        }
    }

    if (typing) {
        TextEntryDialog(
            fromTv = state.keyboardShown,
            onSend = vm::sendText,
            onDismiss = {
                typing = false
                vm.keyboardDismissed()
            },
        )
    }
}

/** Text for the TV's focused field (search boxes, logins). */
@Composable
private fun TextEntryDialog(fromTv: Boolean, onSend: (String) -> Unit, onDismiss: () -> Unit) {
    var text by remember { mutableStateOf("") }
    val focus = remember { FocusRequester() }
    LaunchedEffect(Unit) { focus.requestFocus() }
    val send = {
        onSend(text)
        onDismiss()
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (fromTv) "The TV asks for text" else "Type on the TV") },
        text = {
            OutlinedTextField(
                text,
                { text = it },
                singleLine = true,
                placeholder = { Text("Sent to the TV's text field") },
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Send),
                keyboardActions = KeyboardActions(onSend = { send() }),
                modifier = Modifier.focusRequester(focus),
            )
        },
        confirmButton = { TextButton(onClick = send, enabled = text.isNotEmpty()) { Text("Send") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun AppTheme(content: @Composable () -> Unit) {
    val dark = isSystemInDarkTheme()
    val context = LocalContext.current
    val colors = when {
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.S ->
            if (dark) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        dark -> darkColorScheme()
        else -> lightColorScheme()
    }
    MaterialTheme(colorScheme = colors, content = content)
}
