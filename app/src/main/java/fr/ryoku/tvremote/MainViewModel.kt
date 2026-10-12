package fr.ryoku.tvremote

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import fr.ryoku.tvremote.core.AppInfo
import fr.ryoku.tvremote.core.CoreEvent
import fr.ryoku.tvremote.core.EventListener
import fr.ryoku.tvremote.core.KeyPress
import fr.ryoku.tvremote.core.RemoteCore
import fr.ryoku.tvremote.core.RemoteException
import fr.ryoku.tvremote.core.RemoteKey
import fr.ryoku.tvremote.core.TvPower
import fr.ryoku.tvremote.core.TvRemote
import fr.ryoku.tvremote.core.YouTubeResult
import fr.ryoku.tvremote.core.setWolBroadcasts
import fr.ryoku.tvremote.logic.PointerAccumulator
import fr.ryoku.tvremote.logic.UiState
import fr.ryoku.tvremote.logic.on
import fr.ryoku.tvremote.logic.withTvs
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

/** Glue between the screens and the Rust core. Every core call runs off the main thread. */
@OptIn(ExperimentalCoroutinesApi::class)
class MainViewModel(app: Application) : AndroidViewModel(app) {
    private val _state = MutableStateFlow(UiState())
    val state: StateFlow<UiState> = _state.asStateFlow()

    private val _messages = Channel<String>(Channel.BUFFERED)
    /** One-off messages for a snackbar. */
    val messages = _messages.receiveAsFlow()

    /** Keys, text and pointer moves go out one at a time, in order. */
    private val commands = Dispatchers.IO.limitedParallelism(1)
    private val io = Dispatchers.IO

    @Volatile private var core: RemoteCore? = null
    @Volatile private var tv: TvRemote? = null
    @Volatile private var foreground = false
    private var lastErrorAt = 0L
    private var volumeJob: Job? = null
    private val pointer = PointerAccumulator()
    private var pointerJob: Job? = null

    private val listener = object : EventListener {
        override fun onEvent(tvId: String, event: CoreEvent) {
            _state.update { it.on(tvId, event) }
            when (event) {
                is CoreEvent.TvsChanged -> viewModelScope.launch(io) { reloadTvs() }
                is CoreEvent.Paired -> viewModelScope.launch(io) { reloadTvs() }
                is CoreEvent.Power -> if (event.power == TvPower.ON && tvId == _state.value.activeId) {
                    viewModelScope.launch(io) { refreshDetails() }
                }
                else -> {}
            }
        }
    }

    init {
        viewModelScope.launch(io) {
            try {
                val c = getApplication<TvRemoteApp>().core
                core = c
                c.setListener(listener)
                reloadTvs()
                _state.update { it.copy(ready = true) }
                if (foreground) startForegroundWork()
            } catch (e: Exception) {
                _state.update { it.copy(ready = true, fatal = "Cannot open the settings: ${e.text()}") }
            }
        }
    }

    // ----- lifecycle --------------------------------------------------------------------

    fun onForeground() {
        foreground = true
        viewModelScope.launch(io) { if (core != null) startForegroundWork() }
    }

    fun onBackground() {
        foreground = false
        viewModelScope.launch(io) {
            core?.stopStatusPolling()
            tv?.disconnect()
        }
    }

    private fun startForegroundWork() {
        val c = core ?: return
        setWolBroadcasts(Net.lanBroadcasts(getApplication()))
        c.startStatusPolling(5_000uL)
        val t = tv ?: return
        // Only reconnect on our own when paired: an unpaired first connection shows the
        // TV's approval prompt, which should follow a deliberate action.
        if (_state.value.active?.paired == true) t.connect()
        refreshDetails()
    }

    override fun onCleared() {
        core?.clearListener()
        core?.stopStatusPolling()
        tv?.disconnect()
        tv?.close()
    }

    // ----- TV list ----------------------------------------------------------------------

    private fun reloadTvs() {
        val c = core ?: return
        val list = c.tvs()
        val activeId = list.firstOrNull { it.active }?.id
        if (tv?.id() != activeId) {
            tv?.disconnect()
            tv?.close()
            tv = activeId?.let { runCatching { c.tv(it) }.getOrNull() }
        }
        _state.update { it.withTvs(list) }
    }

    private fun refreshDetails() {
        val t = tv ?: return
        val id = t.id()
        val power = t.powerState()
        _state.update { it.copy(powers = it.powers + (id to power)) }
        if (power == TvPower.ON) {
            runCatching { t.deviceInfo() } // fills a missing MAC (Wake-on-LAN)
            refreshVolume()
        }
    }

    private fun refreshVolume() {
        val t = tv ?: return
        val v = runCatching { t.volume().toInt() }.getOrNull()
        val m = runCatching { t.muted() }.getOrNull()
        _state.update { it.copy(volume = v ?: it.volume, muted = m ?: it.muted) }
    }

    fun select(id: String) = launchIo("Cannot switch TV") {
        core?.setActive(id)
        reloadTvs()
        if (foreground) startForegroundWork()
    }

    fun rename(id: String, name: String) = launchIo("Cannot rename") { core?.rename(id, name) }

    fun remove(id: String) = launchIo("Cannot remove") {
        core?.remove(id)
        reloadTvs()
    }

    /** Connect now; on a new TV this makes it show its approval prompt. */
    fun pair(id: String) = launchIo("Cannot connect") {
        if (_state.value.activeId != id) {
            core?.setActive(id)
            reloadTvs()
        }
        tv?.connect()
    }

    fun scan() {
        if (_state.value.scanning) return
        val c = core ?: return
        _state.update { it.copy(scanning = true, scanError = null) }
        viewModelScope.launch(io) {
            val app = getApplication<Application>()
            if (!Net.onLan(app)) {
                _state.update {
                    it.copy(scanning = false, scanned = true, found = emptyList(), scanError = "Connect the phone to the TV's Wi-Fi")
                }
                return@launch
            }
            val result = Net.withMulticast(app) { c.scan(4_000u) }
            _state.update {
                it.copy(scanning = false, scanned = true, found = result.tvs, scanError = result.error)
            }
            reloadTvs()
        }
    }

    fun addFound(key: String) = launchIo("Cannot add the TV") {
        val id = core?.addFound(key) ?: return@launchIo
        afterAdd(id)
    }

    fun addHost(host: String, name: String?) = launchIo("Cannot add the TV") {
        val id = core?.addHost(host, name?.takeIf { it.isNotBlank() }) ?: return@launchIo
        afterAdd(id)
    }

    private fun afterAdd(id: String) {
        core?.setActive(id)
        reloadTvs()
        _state.update { s -> s.copy(found = s.found.map { if (it.knownId == null && it.ip == s.active?.host) it.copy(knownId = id) else it }) }
        if (foreground) startForegroundWork()
    }

    // ----- remote -----------------------------------------------------------------------

    fun key(k: RemoteKey) = command { it.sendKey(k) }.also { if (k.isVolume()) scheduleVolumeRefresh() }

    fun keyDown(k: RemoteKey) = command { it.sendKeyAction(k, KeyPress.PRESS) }

    fun keyUp(k: RemoteKey) = command { it.sendKeyAction(k, KeyPress.RELEASE) }

    fun sendText(text: String) {
        if (text.isEmpty()) return
        command { it.sendText(text) }
    }

    fun keyboardDismissed() = _state.update { it.copy(keyboardShown = false) }

    fun setVolume(v: Int) {
        _state.update { it.copy(volume = v) }
        volumeJob?.cancel()
        volumeJob = viewModelScope.launch(commands) {
            delay(150) // coalesce slider drags
            val t = tv ?: return@launch
            runCatching { t.setVolume(v.coerceIn(0, 100).toUByte()) }.onFailure { report("Volume", it) }
        }
    }

    fun toggleMute() {
        val muted = _state.value.muted
        if (muted == null) {
            key(RemoteKey.MUTE)
            return
        }
        _state.update { it.copy(muted = !muted) }
        command { it.setMuted(!muted) }
    }

    private fun scheduleVolumeRefresh() {
        volumeJob?.cancel()
        volumeJob = viewModelScope.launch(io) {
            delay(500)
            refreshVolume()
        }
    }

    /** On → off; off or standby → Wake-on-LAN (never followed by a power key: see core). */
    fun power() {
        val s = _state.value
        if (s.powerBusy) return
        val t = tv ?: return message("Add a TV first")
        _state.update { it.copy(powerBusy = true) }
        viewModelScope.launch(io) {
            try {
                val now = t.powerState()
                if (now == TvPower.ON) {
                    t.powerOff()
                    _state.update { it.copy(powers = it.powers + (t.id() to TvPower.STANDBY)) }
                } else {
                    setWolBroadcasts(Net.lanBroadcasts(getApplication()))
                    t.powerOn(25_000u)
                    _state.update { it.copy(powers = it.powers + (t.id() to TvPower.ON)) }
                    if (_state.value.active?.paired == true) t.connect()
                    refreshDetails()
                }
            } catch (e: RemoteException.Invalid) {
                message(
                    if (e.text().contains("MAC", ignoreCase = true)) {
                        "The TV's MAC address is unknown: scan once while the TV is on to enable power on"
                    } else {
                        e.text()
                    },
                )
            } catch (e: Exception) {
                message("Power: ${e.text()}")
            } finally {
                _state.update { it.copy(powerBusy = false) }
            }
        }
    }

    // ----- touchpad ---------------------------------------------------------------------

    fun pointerMove(dx: Float, dy: Float) {
        pointer.add(dx, dy)
        if (pointerJob?.isActive == true) return
        pointerJob = viewModelScope.launch(commands) {
            // Flush every ~30 ms while moves keep coming; the TV can't take more.
            while (isActive) {
                val step = pointer.take() ?: break
                val t = tv ?: break
                runCatching { t.mouseMove(step.first, step.second) }.onFailure { report("Pointer", it) }
                delay(30)
            }
        }
    }

    fun pointerClick() {
        pointer.reset()
        command { it.mouseClick() }
    }

    // ----- apps -------------------------------------------------------------------------

    fun loadApps() {
        val t = tv ?: return
        if (_state.value.appsLoading) return
        _state.update { it.copy(appsLoading = true, appsError = null) }
        viewModelScope.launch(io) {
            try {
                val apps = t.installedApps().sortedBy { it.name.lowercase() }
                _state.update { it.copy(apps = apps, appsLoading = false) }
            } catch (e: Exception) {
                _state.update { it.copy(appsLoading = false, appsError = e.text()) }
            }
        }
    }

    fun launch(app: AppInfo) = command { it.launchApp(app) }

    fun youTubeApp() = command { it.launchAppId(YOUTUBE_APP_ID) }

    fun netflix() = command { it.netflix() }

    /** A shared YouTube link: open the video on the TV. */
    fun playYouTube(url: String) {
        command {
            when (it.youtube(url)) {
                YouTubeResult.VIDEO -> message("Opening the video on the TV")
                YouTubeResult.APP_ONLY -> message("The TV refused the link: opened YouTube instead")
            }
        }
    }

    // ----- helpers ----------------------------------------------------------------------

    private fun command(block: (TvRemote) -> Unit) {
        val t = tv ?: return message("Add a TV first (TVs tab)")
        viewModelScope.launch(commands) {
            try {
                block(t)
            } catch (e: Exception) {
                report("", e)
            }
        }
    }

    private fun launchIo(what: String, block: suspend () -> Unit) {
        viewModelScope.launch(io) {
            try {
                block()
            } catch (e: Exception) {
                message("$what: ${e.text()}")
            }
        }
    }

    /** A failed command; at most one message per 3 s (a held key can fail many times). */
    private fun report(what: String, e: Throwable) {
        val now = System.currentTimeMillis()
        if (now - lastErrorAt < 3_000) return
        lastErrorAt = now
        val text = when (e) {
            is RemoteException.NotAuthorized -> "Not allowed: accept this phone on the TV"
            is RemoteException.Unreachable -> "TV unreachable: is it on and on the same Wi-Fi?"
            else -> e.text()
        }
        message(if (what.isEmpty()) text else "$what: $text")
    }

    private fun message(text: String) {
        _messages.trySend(text)
    }

    companion object {
        const val YOUTUBE_APP_ID = "111299001912"
    }
}

private fun RemoteKey.isVolume() =
    this == RemoteKey.VOLUME_UP || this == RemoteKey.VOLUME_DOWN || this == RemoteKey.MUTE

/** The message of a core exception (flat errors carry the core's text). */
fun Throwable.text(): String = message?.takeIf { it.isNotBlank() } ?: javaClass.simpleName
