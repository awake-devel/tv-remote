package fr.ryoku.tvremote

import android.app.Application
import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.net.wifi.WifiManager
import android.os.Build
import fr.ryoku.tvremote.core.RemoteCore
import fr.ryoku.tvremote.logic.ipv4Broadcast
import java.io.File
import java.net.Inet4Address

class TvRemoteApp : Application() {
    /** Opened on first use, off the main thread (it reads the config file). */
    val core: RemoteCore by lazy {
        RemoteCore(
            File(filesDir, "config.json").absolutePath,
            File(cacheDir, "core").absolutePath,
            // Shown in the TV's pairing prompt and its list of connected devices.
            "TV Remote (${Build.MODEL})",
        )
    }
}

/** Network helpers that need Android APIs. */
object Net {
    /** Broadcast address(es) of the Wi-Fi (or Ethernet) network the phone is on. */
    fun lanBroadcasts(context: Context): List<String> {
        val cm = context.getSystemService(ConnectivityManager::class.java) ?: return emptyList()
        val network = cm.activeNetwork ?: return emptyList()
        val caps = cm.getNetworkCapabilities(network)
        val local = caps != null && (
            caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) ||
                caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET)
            )
        if (!local) return emptyList()
        val props = cm.getLinkProperties(network) ?: return emptyList()
        return props.linkAddresses
            .filter { it.address is Inet4Address }
            .mapNotNull { ipv4Broadcast(it.address.address, it.prefixLength) }
    }

    /** True when the phone is on Wi-Fi or Ethernet (where the TV can be). */
    fun onLan(context: Context): Boolean {
        val cm = context.getSystemService(ConnectivityManager::class.java) ?: return false
        val caps = cm.getNetworkCapabilities(cm.activeNetwork ?: return false) ?: return false
        return caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) ||
            caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET)
    }

    /** Run [block] holding a Wi-Fi multicast lock (SSDP answers are multicast-adjacent). */
    fun <T> withMulticast(context: Context, block: () -> T): T {
        val wifi = context.applicationContext.getSystemService(WifiManager::class.java)
        val lock = wifi?.createMulticastLock("tvremote-ssdp")?.apply {
            setReferenceCounted(false)
            acquire()
        }
        try {
            return block()
        } finally {
            lock?.release()
        }
    }
}
