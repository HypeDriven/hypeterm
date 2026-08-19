package com.hypedriven.hypeterm

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest

/**
 * Tells the native controller when the device has usable connectivity (spec §11).
 *
 * A network change may trigger a reconnect, but the controller decides that: this only
 * reports what the platform says, so a handover between Wi-Fi and mobile never creates
 * two concurrent attachments.
 */
class ConnectivityWatcher(context: Context, private val onChanged: (Boolean) -> Unit) {

    private val manager =
        context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager

    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) = report()
        override fun onLost(network: Network) = report()
        override fun onCapabilitiesChanged(network: Network, capabilities: NetworkCapabilities) =
            report()
    }

    private var registered = false

    fun start() {
        if (registered) return
        val request = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .build()
        manager.registerNetworkCallback(request, callback)
        registered = true
        report()
    }

    fun stop() {
        if (!registered) return
        runCatching { manager.unregisterNetworkCallback(callback) }
        registered = false
    }

    private fun report() {
        val capabilities = manager.getNetworkCapabilities(manager.activeNetwork)
        val available = capabilities != null &&
            capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) &&
            capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED)
        onChanged(available)
    }
}
