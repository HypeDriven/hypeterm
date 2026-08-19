package com.hypedriven.hypeterm

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.graphics.Color
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.util.TypedValue
import android.view.View
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import org.json.JSONObject

/**
 * The embedded Tailscale node's controls (spec §7.4).
 *
 * The node is not a VPN: it runs in user space and carries only this app's connections
 * to the relay, so no `VpnService` consent dialog appears and nothing else on the
 * device is rerouted. Switching it on or off rebuilds the native session, because
 * whether connections go through the tunnel is fixed when the controller is created —
 * a running session must never quietly change which path its traffic takes.
 *
 * The auth key is a credential: it is sealed by the Keystore-backed store, never
 * written to the settings file, and never logged.
 */
class TunnelPanel(
    private val activity: Activity,
    private val onSettingsChanged: (Hypeterm.Settings) -> Unit,
) {

    private val handler = Handler(Looper.getMainLooper())
    private val secureStore = KeystoreSecureStore(activity)

    private lateinit var enableBox: CheckBox
    private lateinit var cleartextBox: CheckBox
    private lateinit var authKeyField: EditText
    private lateinit var statusView: TextView
    private lateinit var connectButton: Button
    private lateinit var loginButton: Button
    private lateinit var forgetButton: Button

    private var polling = false
    /// Set while a start, stop or logout is in flight, so the one-second status poll
    /// does not re-enable the button under the user's finger.
    @Volatile private var busy = false
    /// How many tunnel operations are actually running, so a completion that was posted
    /// to a cleared handler can be told from one still in progress.
    private val operations = java.util.concurrent.atomic.AtomicInteger(0)

    fun build(settings: Hypeterm.Settings): View {
        val density = activity.resources.displayMetrics.density
        val column = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(0, (16 * density).toInt(), 0, 0)
        }

        column.addView(label(activity.getString(R.string.tunnel_section)).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
        })
        column.addView(label(activity.getString(R.string.tunnel_explanation)))

        enableBox = CheckBox(activity).apply {
            // 48dp, like every other control (spec §13).
            minHeight = (48 * density).toInt()
            text = activity.getString(R.string.tunnel_enable)
            setTextColor(TEXT)
            isChecked = settings.tunnelEnabled
            setOnClickListener { applyEnabled(isChecked) }
        }
        column.addView(enableBox)

        cleartextBox = CheckBox(activity).apply {
            // 48dp, like every other control (spec §13).
            minHeight = (48 * density).toInt()
            text = activity.getString(R.string.tunnel_allow_cleartext)
            setTextColor(TEXT)
            isChecked = settings.tunnelAllowCleartext
            setOnClickListener { applyCleartext(isChecked) }
        }
        column.addView(cleartextBox)

        authKeyField = EditText(activity).apply {
            hint = activity.getString(R.string.tunnel_auth_key_hint)
            contentDescription = activity.getString(R.string.tunnel_auth_key_hint)
            // A password field: no suggestions, no keyboard learning, not shown in the
            // clear. It is a credential (spec §12).
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
        }
        column.addView(authKeyField)

        connectButton = button(R.string.tunnel_connect) { toggleConnection() }
        column.addView(connectButton)

        loginButton = button(R.string.tunnel_open_login) { openLoginPage() }
        loginButton.visibility = View.GONE
        column.addView(loginButton)

        forgetButton = button(R.string.tunnel_sign_out) { forget() }
        column.addView(forgetButton)

        statusView = label("")
        column.addView(statusView)

        refresh()
        return column
    }

    fun onResume() {
        polling = true
        clearStaleBusy()
        poll()
    }

    fun onPause() {
        polling = false
        handler.removeCallbacksAndMessages(null)
    }

    /// Re-enables the controls when nothing is actually in flight.
    ///
    /// A start, stop or logout that finishes while this screen is paused posts its
    /// completion to a handler whose callbacks have just been cleared, so `busy` stays
    /// set and every tunnel control is disabled for the life of the process.
    private fun clearStaleBusy() {
        if (busy && operations.get() == 0) busy = false
    }

    // ---------------------------------------------------------------- internals

    private fun client(): Hypeterm = Hypeterm.get(activity)

    private fun applyEnabled(enabled: Boolean) {
        val updated = Hypeterm.Settings.load(activity).copy(tunnelEnabled = enabled)
        updated.save(activity)
        Toast.makeText(activity, R.string.tunnel_restart_needed, Toast.LENGTH_SHORT).show()
        onSettingsChanged(updated)
        refresh()
    }

    private fun applyCleartext(allow: Boolean) {
        val updated = Hypeterm.Settings.load(activity).copy(tunnelAllowCleartext = allow)
        updated.save(activity)
        onSettingsChanged(updated)
    }

    private fun toggleConnection() {
        if (busy) return
        if (tunnelStatus().optBoolean("started")) {
            inBackground {
                client().native.tunnelStop()
                ""
            }
            return
        }

        // A key typed now replaces any stored one; an empty field reuses what is
        // stored, so the user does not have to paste it again on every reconnect.
        val typed = authKeyField.text.toString().trim()
        if (typed.isNotEmpty()) {
            secureStore.put(AUTH_KEY, typed.toByteArray(Charsets.UTF_8))
            authKeyField.setText("")
        }
        inBackground {
            val stored = secureStore.get(AUTH_KEY)?.toString(Charsets.UTF_8).orEmpty()
            client().native.tunnelStart(stored)
        }
    }

    private fun forget() {
        if (busy) return
        inBackground {
            val error = client().native.tunnelLogout()
            secureStore.remove(AUTH_KEY)
            error
        }
    }

    /**
     * Runs a tunnel operation off the main thread and reports its error, if any.
     *
     * Starting a WireGuard node, stopping one, and logging out all take long enough to
     * stall a frame — logout has a ten-second timeout of its own — so none of them may
     * run on the UI thread.
     */
    private fun inBackground(operation: () -> String) {
        busy = true
        operations.incrementAndGet()
        connectButton.isEnabled = false
        Thread({
            val error = operation()
            operations.decrementAndGet()
            handler.post {
                busy = false
                if (error.isNotEmpty()) {
                    statusView.text =
                        activity.getString(R.string.tunnel_status_error, error)
                }
                refresh()
            }
        }, "tunnel-operation").start()
    }

    private fun openLoginPage() {
        val url = tunnelStatus().optString("auth_url")
        if (url.isEmpty()) return
        try {
            activity.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)))
        } catch (error: ActivityNotFoundException) {
            // No browser: show the URL so it can be copied to another device.
            statusView.text = url
            statusView.setTextIsSelectable(true)
        }
    }

    private fun tunnelStatus(): JSONObject =
        runCatching { JSONObject(client().native.tunnelStatus()) }.getOrNull() ?: JSONObject()

    private fun poll() {
        if (!polling) return
        refresh()
        handler.postDelayed({ poll() }, POLL_INTERVAL_MS)
    }

    private fun refresh() {
        val status = tunnelStatus()
        val enabled = enableBox.isChecked
        val available = status.optBoolean("available")
        val started = status.optBoolean("started")
        val running = status.optBoolean("running")
        val authUrl = status.optString("auth_url")
        val lastError = status.optString("last_error")

        authKeyField.isEnabled = enabled && available && !started && !busy
        connectButton.isEnabled = enabled && available && !busy
        forgetButton.isEnabled = enabled && available && !busy
        connectButton.text = activity.getString(
            if (started) R.string.tunnel_disconnect else R.string.tunnel_connect
        )
        loginButton.visibility = if (authUrl.isNotEmpty()) View.VISIBLE else View.GONE

        statusView.text = when {
            !enabled -> activity.getString(R.string.tunnel_status_disabled)
            !available -> activity.getString(R.string.tunnel_status_unavailable)
            running -> {
                val addresses = status.optJSONArray("addresses")
                val address = if (addresses != null && addresses.length() > 0) {
                    addresses.optString(0)
                } else {
                    ""
                }
                activity.getString(
                    R.string.tunnel_status_running,
                    status.optString("hostname").ifEmpty { "this device" },
                    address,
                )
            }
            authUrl.isNotEmpty() -> activity.getString(R.string.tunnel_status_waiting)
            started -> activity.getString(
                R.string.tunnel_status_starting,
                status.optString("backend_state").ifEmpty { "starting" },
            )
            lastError.isNotEmpty() ->
                activity.getString(R.string.tunnel_status_error, lastError)
            else -> activity.getString(R.string.tunnel_status_stopped)
        }
    }

    private fun label(text: String): TextView = TextView(activity).apply {
        this.text = text
        setTextColor(TEXT)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        setPadding(0, 16, 0, 16)
    }

    private fun button(textId: Int, onClick: () -> Unit): Button = Button(activity).apply {
        text = activity.getString(textId)
        isAllCaps = false
        setOnClickListener { onClick() }
    }

    private companion object {
        val AUTH_KEY = Hypeterm.TUNNEL_AUTH_KEY
        const val POLL_INTERVAL_MS = 1000L
        val TEXT = Color.parseColor("#D8D8D8")
    }
}
