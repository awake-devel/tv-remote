package fr.ryoku.tvremote

import android.app.Application
import android.content.Context
import fr.ryoku.tvremote.core.RemoteCoreInterface
import fr.ryoku.tvremote.core.TvRemoteInterface
import fr.ryoku.tvremote.core.setWolBroadcasts

/**
 * What the view model needs from the outside world: the Rust core and a few network
 * facts. Production uses [RealBackend]; instrumented tests install an in-memory fake
 * through [Backends] so they never touch a real TV.
 */
interface Backend {
    val core: RemoteCoreInterface

    /** The handle of TV [id] (the generated `RemoteCoreInterface.tv` returns the concrete class). */
    fun tv(id: String): TvRemoteInterface

    fun onLan(context: Context): Boolean

    /** Pass the LAN broadcast addresses to the core's Wake-on-LAN. */
    fun updateWolBroadcasts(context: Context)

    fun <T> withMulticast(context: Context, block: () -> T): T
}

class RealBackend(private val app: TvRemoteApp) : Backend {
    override val core = app.core

    override fun tv(id: String): TvRemoteInterface = app.core.tv(id)

    override fun onLan(context: Context) = Net.onLan(context)

    override fun updateWolBroadcasts(context: Context) = setWolBroadcasts(Net.lanBroadcasts(context))

    override fun <T> withMulticast(context: Context, block: () -> T): T = Net.withMulticast(context, block)
}

/** Test seams. Set before the activity starts; production never changes them. */
object Backends {
    @Volatile
    var factory: (Application) -> Backend = { RealBackend(it as TvRemoteApp) }

    /** Overrides the local network permission check (Android 17+) when set. */
    @Volatile
    var lanGranted: ((Context) -> Boolean)? = null

    fun reset() {
        factory = { RealBackend(it as TvRemoteApp) }
        lanGranted = null
    }
}
