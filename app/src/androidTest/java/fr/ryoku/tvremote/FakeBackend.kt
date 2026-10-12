package fr.ryoku.tvremote

import android.content.Context
import fr.ryoku.tvremote.core.AppInfo
import fr.ryoku.tvremote.core.Connection
import fr.ryoku.tvremote.core.CoreEvent
import fr.ryoku.tvremote.core.DeviceSummary
import fr.ryoku.tvremote.core.EventListener
import fr.ryoku.tvremote.core.FoundTv
import fr.ryoku.tvremote.core.KeyPress
import fr.ryoku.tvremote.core.RemoteCoreInterface
import fr.ryoku.tvremote.core.RemoteException
import fr.ryoku.tvremote.core.RemoteKey
import fr.ryoku.tvremote.core.ScanResult
import fr.ryoku.tvremote.core.TvPower
import fr.ryoku.tvremote.core.TvRemote
import fr.ryoku.tvremote.core.TvRemoteInterface
import fr.ryoku.tvremote.core.TvSummary
import fr.ryoku.tvremote.core.YouTubeResult
import java.util.Collections

/** Everything the UI did to the fake TV, in order. */
sealed class Call {
    data class Key(val key: RemoteKey) : Call()
    data class KeyAction(val key: RemoteKey, val action: KeyPress) : Call()
    data class Text(val text: String) : Call()
    data class Move(val dx: Int, val dy: Int) : Call()
    object Click : Call()
    data class Launch(val appId: String) : Call()
    data class LaunchId(val appId: String) : Call()
    data class YouTube(val url: String) : Call()
    object Netflix : Call()
    object PowerOff : Call()
    data class PowerOn(val timeoutMs: UInt) : Call()
    data class SetVolume(val volume: Int) : Call()
    data class SetMuted(val muted: Boolean) : Call()
    object Connect : Call()
    object Disconnect : Call()
}

/** An in-memory TV: records calls, answers from its fields, fails on demand. */
class FakeTv(private val id: String, private val name: String, private val host: String) : TvRemoteInterface {
    val calls: MutableList<Call> = Collections.synchronizedList(mutableListOf())

    @Volatile var power: TvPower = TvPower.ON
    @Volatile var volume: Int = 10
    @Volatile var muteState: Boolean = false
    @Volatile var apps: List<AppInfo> = listOf(
        AppInfo("3201907018807", "Netflix", 2, null),
        AppInfo("111299001912", "YouTube", 2, null),
        AppInfo("org.tizen.browser", "Browser", 4, null),
    )

    /** When set, every command throws it. */
    @Volatile var failure: RemoteException? = null

    @Volatile var appsFailure: RemoteException? = null

    fun keys(): List<RemoteKey> = synchronized(calls) { calls.filterIsInstance<Call.Key>().map { it.key } }

    fun has(call: Call): Boolean = synchronized(calls) { calls.contains(call) }

    private fun record(c: Call) {
        failure?.let { throw it }
        calls.add(c)
    }

    override fun connect() {
        calls.add(Call.Connect)
    }

    override fun disconnect() {
        calls.add(Call.Disconnect)
    }

    override fun connection(): Connection = Connection.Connected

    override fun deviceInfo(): DeviceSummary =
        DeviceSummary(name, "QE00TEST", "3840x2160", "wireless", "00:00:5e:00:53:01", host, "Tizen", power)

    override fun holdKey(key: RemoteKey, holdMs: UInt) = record(Call.Key(key))

    override fun host(): String = host

    override fun id(): String = id

    override fun installedApps(): List<AppInfo> {
        appsFailure?.let { throw it }
        return apps
    }

    override fun launchApp(app: AppInfo) = record(Call.Launch(app.appId))

    override fun launchAppId(appId: String) = record(Call.LaunchId(appId))

    override fun mouseClick() = record(Call.Click)

    override fun mouseMove(dx: Int, dy: Int) = record(Call.Move(dx, dy))

    override fun muted(): Boolean = muteState

    override fun name(): String = name

    override fun netflix() = record(Call.Netflix)

    override fun powerOff() {
        record(Call.PowerOff)
        power = TvPower.STANDBY
    }

    override fun powerOn(timeoutMs: UInt) {
        record(Call.PowerOn(timeoutMs))
        power = TvPower.ON
    }

    override fun powerState(): TvPower = power

    override fun sendKey(key: RemoteKey) = record(Call.Key(key))

    override fun sendKeyAction(key: RemoteKey, action: KeyPress) = record(Call.KeyAction(key, action))

    override fun sendKeyCode(code: String) = record(Call.Text("code:$code"))

    override fun sendText(text: String) = record(Call.Text(text))

    override fun setMuted(muted: Boolean) {
        record(Call.SetMuted(muted))
        this.muteState = muted
    }

    override fun setVolume(volume: UByte) {
        record(Call.SetVolume(volume.toInt()))
        this.volume = volume.toInt()
    }

    override fun volume(): UByte = volume.toUByte()

    override fun youtube(idOrUrl: String): YouTubeResult {
        record(Call.YouTube(idOrUrl))
        return YouTubeResult.APP_ONLY
    }
}

/** An in-memory TV list with the core's rules that the UI relies on. */
class FakeCore : RemoteCoreInterface {
    private val lock = Any()
    private val list = mutableListOf<TvSummary>()
    val tvs = mutableMapOf<String, FakeTv>()
    @Volatile var eventListener: EventListener? = null
    @Volatile var polling = false
    @Volatile var found: List<FoundTv> = emptyList()
    @Volatile var scans = 0
    val renamed = Collections.synchronizedList(mutableListOf<Pair<String, String>>())
    val removed = Collections.synchronizedList(mutableListOf<String>())
    val addedHosts = Collections.synchronizedList(mutableListOf<Pair<String, String?>>())

    fun addTv(name: String, host: String, paired: Boolean = true, mac: String? = "00:00:5e:00:53:01"): FakeTv =
        synchronized(lock) {
            val id = "tv${list.size + 1}"
            list.add(TvSummary(id, name, host, "QE00TEST", mac, paired, list.none { it.active }))
            FakeTv(id, name, host).also { tvs[id] = it }
        }

    fun emit(tvId: String, event: CoreEvent) {
        eventListener?.onEvent(tvId, event)
    }

    private fun changed() = emit("", CoreEvent.TvsChanged)

    override fun activeId(): String? = synchronized(lock) { list.firstOrNull { it.active }?.id }

    override fun addFound(key: String): String {
        val f = found.firstOrNull { it.key == key } ?: throw RemoteException.Invalid("gone")
        val id = addTv(f.name, f.ip ?: "", paired = false, mac = f.mac).id()
        changed()
        return id
    }

    override fun addHost(host: String, name: String?): String {
        addedHosts.add(host to name)
        val id = addTv(name ?: "Samsung TV ($host)", host, paired = false, mac = null).id()
        changed()
        return id
    }

    override fun clearListener() {
        eventListener = null
    }

    override fun remove(id: String) {
        synchronized(lock) {
            list.removeAll { it.id == id }
            tvs.remove(id)
        }
        removed.add(id)
        changed()
    }

    override fun rename(id: String, name: String) {
        synchronized(lock) {
            val i = list.indexOfFirst { it.id == id }
            if (i >= 0) list[i] = list[i].copy(name = name)
        }
        renamed.add(id to name)
        changed()
    }

    override fun scan(timeoutMs: UInt): ScanResult {
        scans++
        return ScanResult(found, null)
    }

    override fun setActive(id: String) {
        synchronized(lock) {
            for (i in list.indices) list[i] = list[i].copy(active = list[i].id == id)
        }
        changed()
    }

    override fun setListener(listener: EventListener) {
        eventListener = listener
    }

    override fun startStatusPolling(intervalMs: ULong) {
        polling = true
    }

    override fun stopStatusPolling() {
        polling = false
    }

    /** Not used by the app (it goes through [Backend.tv]); the generated type is final. */
    override fun tv(id: String): TvRemote = throw UnsupportedOperationException("use FakeBackend.tv")

    override fun tvs(): List<TvSummary> = synchronized(lock) { list.toList() }

}

class FakeBackend(override val core: FakeCore = FakeCore(), var lan: Boolean = true) : Backend {
    override fun tv(id: String): TvRemoteInterface = core.tvs[id] ?: throw RemoteException.Invalid("no TV $id")

    override fun onLan(context: Context): Boolean = lan

    override fun updateWolBroadcasts(context: Context) {}

    override fun <T> withMulticast(context: Context, block: () -> T): T = block()
}
