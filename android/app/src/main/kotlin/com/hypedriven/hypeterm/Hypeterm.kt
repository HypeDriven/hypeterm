package com.hypedriven.hypeterm

import android.content.Context
import android.os.Handler
import android.os.Looper
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.security.KeyStore
import java.security.cert.X509Certificate
import android.util.Base64

/**
 * Process-wide client instance.
 *
 * One native controller and one render thread per process: the specification's model
 * is a single attached session (spec §4, non-goals: multiple visible panes), and
 * sharing one controller keeps the reconnect and generation bookkeeping in one place.
 */
class Hypeterm private constructor(
    private val bridge: NativeBridge,
    private val mainHandler: Handler,
) : NativeCallbacks {

    /** Latest status, as reported by the controller. */
    @Volatile var status: JSONObject = JSONObject()
        private set

    var onStatusChanged: ((JSONObject) -> Unit)? = null
    var onTerminalsChanged: ((JSONArray) -> Unit)? = null
    var onTitleChanged: ((String) -> Unit)? = null
    var onUserMessage: ((kind: String, message: String) -> Unit)? = null
    var onBellRang: (() -> Unit)? = null
    var onClipboardWriteRequested: ((String) -> Unit)? = null
    var onFollowingOutputChanged: ((Boolean) -> Unit)? = null

    /**
     * Whether the view is keeping up with the newest output.
     *
     * Cached here rather than pulled, because it changes as an asynchronous consequence
     * of a scroll command: asking right after one returns the old answer. A screen that
     * opens mid-session reads this for its initial state.
     */
    @Volatile var followingOutput: Boolean = true
        private set

    val native: NativeBridge get() = bridge

    // -- NativeCallbacks: all of these arrive on the native network thread ------

    override fun onStatus(statusJson: String) {
        val parsed = runCatching { JSONObject(statusJson) }.getOrNull() ?: return
        status = parsed
        mainHandler.post { onStatusChanged?.invoke(parsed) }
    }

    override fun onTerminals(terminalsJson: String) {
        val parsed = runCatching { JSONObject(terminalsJson).getJSONArray("terminals") }
            .getOrNull() ?: return
        mainHandler.post { onTerminalsChanged?.invoke(parsed) }
    }

    override fun onTitle(title: String) {
        mainHandler.post { onTitleChanged?.invoke(title) }
    }

    override fun onMessage(kind: String, message: String) {
        mainHandler.post { onUserMessage?.invoke(kind, message) }
    }

    override fun onBell() {
        mainHandler.post { onBellRang?.invoke() }
    }

    override fun onClipboardWrite(text: String) {
        mainHandler.post { onClipboardWriteRequested?.invoke(text) }
    }

    override fun onFollowOutputChanged(following: Boolean) {
        followingOutput = following
        mainHandler.post { onFollowingOutputChanged?.invoke(following) }
    }

    fun shutdown() {
        bridge.destroy()
        instance = null
    }

    /**
     * Rebuilds the native session in place, keeping this object and its bridge.
     *
     * Changing the relay or the tunnel needs a new native session, but every screen
     * already holds this instance and its [NativeBridge]. Swapping the handle inside
     * them means nobody has to notice, and nobody is left holding a dead one.
     */
    fun restart(context: Context, settings: Settings) {
        bridge.adopt(newHandle(context.applicationContext, settings))
        startTunnelIfEnabled(context.applicationContext, settings)
    }

    companion object {
        @Volatile private var instance: Hypeterm? = null

        fun get(context: Context, settings: Settings = Settings.load(context)): Hypeterm {
            instance?.let { return it }
            synchronized(this) {
                instance?.let { return it }
                val created = create(context.applicationContext, settings)
                instance = created
                return created
            }
        }

        private fun create(context: Context, settings: Settings): Hypeterm {
            val bridge = NativeBridge(newHandle(context, settings))
            startTunnelIfEnabled(context, settings)
            return Hypeterm(bridge, Handler(Looper.getMainLooper()))
        }

        /// Builds a native session and returns its handle.
        ///
        /// The callbacks forward to whatever `instance` is at the time they fire, not to
        /// the object that happened to be current when the session was built, so a
        /// restart does not orphan them.
        private fun newHandle(context: Context, settings: Settings): Long {
            val metrics = context.resources.displayMetrics
            val fontSizePx = settings.fontSizeSp * metrics.scaledDensity
            val secureStore = KeystoreSecureStore(context)
            return NativeBridge.newHandle(
                configJson = buildConfig(context, settings).toString(),
                preferencesPath = File(context.filesDir, "preferences.json").absolutePath,
                secureStore = secureStore,
                rasterizer = GlyphRasterizer(),
                callbacks = object : NativeCallbacks {
                    override fun onStatus(statusJson: String) =
                        instance?.onStatus(statusJson) ?: Unit
                    override fun onTerminals(terminalsJson: String) =
                        instance?.onTerminals(terminalsJson) ?: Unit
                    override fun onTitle(title: String) = instance?.onTitle(title) ?: Unit
                    override fun onMessage(kind: String, message: String) =
                        instance?.onMessage(kind, message) ?: Unit
                    override fun onBell() = instance?.onBell() ?: Unit
                    override fun onClipboardWrite(text: String) =
                        instance?.onClipboardWrite(text) ?: Unit
                    override fun onFollowOutputChanged(following: Boolean) =
                        instance?.onFollowOutputChanged(following) ?: Unit
                },
                fontSizePx = fontSizePx,
                density = metrics.density,
            )
        }

        /// Enabling the tunnel means the relay is only reachable through it, so the node
        /// comes up with the session rather than waiting for a visit to the settings
        /// screen. The auth key is read from Keystore-sealed storage and handed straight
        /// to the node; it is never held anywhere else.
        ///
        /// Off the main thread: bringing a WireGuard node up takes the better part of a
        /// second, and this runs from an activity's onCreate.
        private fun startTunnelIfEnabled(context: Context, settings: Settings) {
            if (!settings.tunnelEnabled) return
            val secureStore = KeystoreSecureStore(context)
            Thread({
                val authKey = secureStore.get(TUNNEL_AUTH_KEY)?.toString(Charsets.UTF_8).orEmpty()
                instance?.native?.tunnelStart(authKey)
            }, "tunnel-start").start()
        }

        /** Where the Tailscale auth key is sealed. Shared with [TunnelPanel]. */
        const val TUNNEL_AUTH_KEY = "tailscale_auth_key"


        private fun buildConfig(context: Context, settings: Settings): JSONObject {
            val config = JSONObject()
            config.put("server_url", settings.serverUrl)
            config.put("device_name", android.os.Build.MODEL ?: "Android device")
            config.put("scrollback_lines", settings.scrollbackLines)
            config.put("allow_clipboard_write", settings.allowRemoteClipboardWrite)
            config.put("secure_window", settings.secureWindow)
            config.put("detach_when_backgrounded", settings.detachWhenBackgrounded)
            config.put("trust_anchors_pem", JSONArray(systemTrustAnchors()))

            // The tunnel's state directory holds the node key, so it goes in
            // app-private storage and nowhere else.
            config.put("tunnel", JSONObject().apply {
                put("enabled", settings.tunnelEnabled)
                put("state_dir", File(context.filesDir, "tailscale").absolutePath)
                put("hostname", settings.tunnelHostname.ifEmpty { defaultNodeName() })
                put("control_url", settings.tunnelControlUrl)
                // Inside the tunnel the relay may legitimately be plain HTTP: the
                // WireGuard tunnel already authenticates and encrypts the peer, and a
                // tailnet address has no public certificate to present. It stays off
                // by default all the same (spec §7.4).
                put("allow_cleartext", settings.tunnelAllowCleartext)
            })
            return config
        }

        /** A tailnet node name: lower case, digits and hyphens only. */
        private fun defaultNodeName(): String {
            val model = (android.os.Build.MODEL ?: "android").lowercase()
            val cleaned = model.map { if (it.isLetterOrDigit()) it else '-' }
                .joinToString("")
                .trim('-')
                .take(24)
            return if (cleaned.isEmpty()) "hypeterm" else "hypeterm-$cleaned"
        }

        /**
         * Exports the platform's trust anchors as PEM.
         *
         * OpenSSL cannot read Android's trust store, so the anchors are handed to it
         * explicitly. This *adds* anchors; verification itself is never disabled
         * (spec §7.4). A deployment that pins certificates supplies its own list here
         * instead.
         */
        private fun systemTrustAnchors(): List<String> {
            return runCatching {
                val keyStore = KeyStore.getInstance("AndroidCAStore").apply { load(null) }
                val anchors = mutableListOf<String>()
                val aliases = keyStore.aliases()
                while (aliases.hasMoreElements()) {
                    val certificate = keyStore.getCertificate(aliases.nextElement())
                    if (certificate is X509Certificate) {
                        val encoded = Base64.encodeToString(certificate.encoded, Base64.NO_WRAP)
                        anchors.add(
                            "-----BEGIN CERTIFICATE-----\n" +
                                encoded.chunked(64).joinToString("\n") +
                                "\n-----END CERTIFICATE-----\n"
                        )
                    }
                }
                anchors.toList()
            }.getOrElse { emptyList() }
        }
    }

    /** User-visible preferences (spec §13: font size and contrast are configurable). */
    data class Settings(
        val serverUrl: String,
        val fontSizeSp: Float = 14f,
        val scrollbackLines: Int = 10000,
        val allowRemoteClipboardWrite: Boolean = false,
        val secureWindow: Boolean = false,
        val detachWhenBackgrounded: Boolean = true,
        /// Reach the relay through the embedded Tailscale node instead of the
        /// ordinary network stack (spec §7.4).
        val tunnelEnabled: Boolean = false,
        val tunnelHostname: String = "",
        val tunnelControlUrl: String = "",
        val tunnelAllowCleartext: Boolean = false,
        val foregroundColor: Int = 0xD8D8D8.toInt(),
        val backgroundColor: Int = 0x101216,
        /// 1.0 disables the floor; 4.5 is the WCAG AA ratio for body text.
        val minimumContrast: Float = 1.0f,
    ) {
        fun save(context: Context) {
            context.getSharedPreferences(NAME, Context.MODE_PRIVATE).edit()
                .putString("server_url", serverUrl)
                .putFloat("font_size_sp", fontSizeSp)
                .putInt("scrollback_lines", scrollbackLines)
                .putBoolean("allow_remote_clipboard_write", allowRemoteClipboardWrite)
                .putBoolean("secure_window", secureWindow)
                .putBoolean("detach_when_backgrounded", detachWhenBackgrounded)
                .putBoolean("tunnel_enabled", tunnelEnabled)
                .putString("tunnel_hostname", tunnelHostname)
                .putString("tunnel_control_url", tunnelControlUrl)
                .putBoolean("tunnel_allow_cleartext", tunnelAllowCleartext)
                .putInt("foreground_color", foregroundColor)
                .putInt("background_color", backgroundColor)
                .putFloat("minimum_contrast", minimumContrast)
                .apply()
        }

        companion object {
            private const val NAME = "hypeterm_settings"

            fun load(context: Context): Settings {
                val preferences = context.getSharedPreferences(NAME, Context.MODE_PRIVATE)
                return Settings(
                    serverUrl = preferences.getString("server_url", "") ?: "",
                    fontSizeSp = preferences.getFloat("font_size_sp", 14f),
                    scrollbackLines = preferences.getInt("scrollback_lines", 10000),
                    allowRemoteClipboardWrite =
                        preferences.getBoolean("allow_remote_clipboard_write", false),
                    secureWindow = preferences.getBoolean("secure_window", false),
                    detachWhenBackgrounded =
                        preferences.getBoolean("detach_when_backgrounded", true),
                    tunnelEnabled = preferences.getBoolean("tunnel_enabled", false),
                    tunnelHostname = preferences.getString("tunnel_hostname", "") ?: "",
                    tunnelControlUrl = preferences.getString("tunnel_control_url", "") ?: "",
                    tunnelAllowCleartext =
                        preferences.getBoolean("tunnel_allow_cleartext", false),
                    foregroundColor = preferences.getInt("foreground_color", 0xD8D8D8.toInt()),
                    backgroundColor = preferences.getInt("background_color", 0x101216),
                    minimumContrast = preferences.getFloat("minimum_contrast", 1.0f),
                )
            }
        }
    }
}
