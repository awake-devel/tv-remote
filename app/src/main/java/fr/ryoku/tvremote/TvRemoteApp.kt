package fr.ryoku.tvremote

import android.app.Application
import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkRequest
import android.net.NetworkCapabilities
import android.net.wifi.WifiManager
import android.os.Build
import fr.ryoku.tvremote.core.RemoteCore
import fr.ryoku.tvremote.logic.ipv4Broadcast
import java.io.File
import java.net.Inet4Address

class TvRemoteApp : Application() {
    override fun onCreate() {
        super.onCreate()
        Net.pinToWifi(this)
    }

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
    /** The Wi-Fi network the process is bound to, if any. */
    @Volatile
    var wifi: Network? = null
        private set

    /**
     * Send every socket of this process (the Rust core's too) over Wi-Fi. Without this,
     * Android routes them over the default network, which can be mobile data while Wi-Fi
     * is up (Wi-Fi without validated internet, adaptive connectivity…): the TV is then
     * unreachable. Internet is not required: the app only talks to the LAN.
     */
    fun pinToWifi(context: Context) {
        val cm = context.getSystemService(ConnectivityManager::class.java) ?: return
        val request = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .removeCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .build()
        cm.registerNetworkCallback(
            request,
            object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) {
                    wifi = network
                    cm.bindProcessToNetwork(network)
                }

                override fun onLost(network: Network) {
                    if (wifi == network) {
                        wifi = null
                        cm.bindProcessToNetwork(null)
                    }
                }
            },
        )
    }

    private fun lanNetwork(cm: ConnectivityManager): Network? {
        wifi?.let { return it }
        val active = cm.activeNetwork ?: return null
        val caps = cm.getNetworkCapabilities(active) ?: return null
        val local = caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) ||
            caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET)
        return if (local) active else null
    }

    /** Broadcast address(es) of the Wi-Fi (or Ethernet) network the phone is on. */
    fun lanBroadcasts(context: Context): List<String> {
        val cm = context.getSystemService(ConnectivityManager::class.java) ?: return emptyList()
        val props = cm.getLinkProperties(lanNetwork(cm) ?: return emptyList()) ?: return emptyList()
        return props.linkAddresses
            .filter { it.address is Inet4Address }
            .mapNotNull { ipv4Broadcast(it.address.address, it.prefixLength) }
    }

    /** True when the phone is on Wi-Fi or Ethernet (where the TV can be). */
    fun onLan(context: Context): Boolean {
        val cm = context.getSystemService(ConnectivityManager::class.java) ?: return false
        return lanNetwork(cm) != null
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
