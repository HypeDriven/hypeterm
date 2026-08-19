package com.hypedriven.hypeterm

import android.app.Activity
import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.TypedValue
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONArray
import org.json.JSONObject

/**
 * Session discovery (spec §5.1 step 2).
 *
 * A `client` device sees exactly the terminals its owning identity owns; anything else
 * answers 404, so this list is also the authorization answer (relay reconciliation
 * §2.3).
 *
 * The screen says what is happening, always. An empty list has several very different
 * causes — nothing is publishing, the tunnel has not come up, the credential was
 * rejected — and showing "no terminals" for all of them leaves the user with nothing
 * to act on. Errors used to go to a Toast alone, which Android suppresses outright
 * when the user has notifications turned down for the app, so they could vanish
 * entirely.
 */
class TerminalListActivity : Activity() {

    private lateinit var client: Hypeterm
    private lateinit var statusLabel: TextView
    private lateinit var terminals: LinearLayout
    private lateinit var emptyLabel: TextView

    private val handler = Handler(Looper.getMainLooper())
    private var polling = false
    /** Latest message from the controller, shown until something replaces it. */
    private var lastMessage: String = ""
    /** The single-terminal shortcut fires once, not on every refresh. */
    private var autoOpened = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        client = Hypeterm.get(this)
        setContentView(buildLayout())

        if (!client.native.hasCredentials()) {
            startActivity(Intent(this, PairingActivity::class.java))
            finish()
            return
        }
        client.native.start()
        bindClient()
    }

    /**
     * Points this screen at the current native client and takes its callbacks.
     *
     * Re-done on every resume, because the connection settings screen tears the whole
     * native session down and builds a new one when the relay or the tunnel changes. A
     * screen still holding the old one keeps a bridge whose handle is zero: every call
     * becomes a silent no-op, the list stops updating, and the tunnel reports itself as
     * "not included in this build" on a device where it is running perfectly.
     */
    private fun bindClient() {
        client = Hypeterm.get(this)
        // The bridge survives a session restart now, so this is only about (re)taking the
        // callbacks — a second screen may have claimed them while this one was paused.
        client.onTerminalsChanged = { list -> render(list) }
        client.onStatusChanged = { updateStatus() }
        client.onUserMessage = { _, message ->
            lastMessage = message
            updateStatus()
        }
    }

    override fun onResume() {
        super.onResume()
        bindClient()
        polling = true
        // A terminal opened on the desktop should appear here without the user having
        // to think about it.
        poll()
    }

    override fun onPause() {
        polling = false
        handler.removeCallbacksAndMessages(null)
        super.onPause()
    }

    private fun poll() {
        if (!polling) return
        client.native.refreshTerminals()
        updateStatus()
        handler.postDelayed({ poll() }, POLL_INTERVAL_MS)
    }

    private fun buildLayout(): View {
        val density = resources.displayMetrics.density
        val padding = (16 * density).toInt()
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(padding, padding, padding, padding)
            setBackgroundColor(Color.parseColor("#101216"))
        }

        statusLabel = TextView(this).apply {
            setTextColor(Color.parseColor("#9AA4B2"))
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setPadding(0, 0, 0, (12 * density).toInt())
        }
        column.addView(statusLabel)

        emptyLabel = TextView(this).apply {
            text = getString(R.string.no_terminals)
            setTextColor(Color.parseColor("#D8D8D8"))
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        }

        // Only this part is rebuilt when the list changes, so the status line and the
        // buttons keep their place instead of jumping about under the user's thumb.
        // Above the list, not below it: the list has no bound, so anything after it is
        // off the bottom of the screen exactly when there is most to do.
        column.addView(newTerminalButton())

        terminals = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        terminals.addView(emptyLabel)
        column.addView(terminals)

        column.addView(Button(this).apply {
            text = getString(R.string.refresh)
            isAllCaps = false
            setOnClickListener {
                client.native.refreshTerminals()
                updateStatus()
            }
        })
        column.addView(connectionSettingsButton())

        return ScrollView(this).apply {
            addView(column)
            applySystemWindowPadding()
        }
    }

    /**
     * Asks one of your machines to open a terminal (relay spec §4.6).
     *
     * The machine decides what runs and refuses unless its owner turned this on, so
     * the worst this button can do on a machine that has not opted in is show why it
     * said no.
     */
    private fun newTerminalButton(): Button = Button(this).apply {
        text = getString(R.string.new_terminal)
        isAllCaps = false
        setOnClickListener { chooseMachineAndOpen(this) }
    }

    /**
     * Both calls block on a round trip that ends with a process starting on another
     * machine, so neither may touch the main thread. The button is disabled meanwhile:
     * a second tap would be a second shell, and the idempotency key only collapses
     * retries of the *same* request.
     */
    private fun chooseMachineAndOpen(button: Button) {
        button.isEnabled = false
        button.text = getString(R.string.new_terminal_working)
        Thread({
            val devices = runCatching { JSONObject(client.native.listDevices()) }.getOrNull()
            val machines = devices?.optJSONArray("devices") ?: JSONArray()
            var opened: JSONObject? = null
            var failure = devices?.optString("error").orEmpty()

            // Only a machine that can publish can host a terminal; a phone cannot.
            for (index in 0 until machines.length()) {
                val machine = machines.optJSONObject(index) ?: continue
                val role = machine.optString("role")
                if (role != "publisher" && role != "both") continue
                val result = runCatching {
                    JSONObject(
                        client.native.openTerminal(
                            machine.optString("device_id"),
                            android.os.Build.MODEL ?: "phone",
                            0,
                            0,
                        )
                    )
                }.getOrNull() ?: continue
                val error = result.optString("error")
                if (error.isEmpty()) {
                    opened = result
                    break
                }
                // Keep the first machine's reason: it is the one the user most likely
                // meant, and a later machine's "not accepting requests" is less useful.
                if (failure.isEmpty()) failure = error
            }

            val terminalId = opened?.optString("terminal_id").orEmpty()
            handler.post {
                button.isEnabled = true
                button.text = getString(R.string.new_terminal)
                if (terminalId.isNotEmpty()) {
                    client.native.refreshTerminals()
                    open(terminalId)
                } else {
                    val reason = failure.ifEmpty { getString(R.string.new_terminal_none) }
                    android.widget.Toast
                        .makeText(this, reason, android.widget.Toast.LENGTH_LONG)
                        .show()
                }
            }
        }, "open-terminal").start()
    }

    /** Reaches the relay URL, pairing and Tailscale controls after the first run. */
    private fun connectionSettingsButton(): Button = Button(this).apply {
        text = getString(R.string.connection_settings)
        isAllCaps = false
        setOnClickListener {
            startActivity(Intent(this@TerminalListActivity, PairingActivity::class.java))
        }
    }

    /**
     * Describes the connection in one line.
     *
     * The tunnel comes first when it is switched on: while it is not carrying traffic
     * nothing else can be true, and "authenticating" would be a misleading thing to
     * show someone whose tailnet has not come up.
     */
    private fun updateStatus() {
        val tunnel = runCatching { JSONObject(client.native.tunnelStatus()) }.getOrNull()
        val tunnelText = tunnel?.let { describeTunnel(it) }
        if (tunnelText != null) {
            statusLabel.text = tunnelText
            return
        }

        val status = client.status
        val state = status.optString("state")
        val error = status.optString("error_message")
        statusLabel.text = when (state) {
            "attached", "attaching", "discovering" -> getString(R.string.list_connected)
            "authenticating" -> getString(R.string.status_authenticating)
            "reconnecting" -> getString(R.string.status_reconnecting)
            "pairing_required" -> getString(R.string.status_pairing_required)
            "failed" -> getString(
                R.string.status_failed,
                error.ifEmpty { lastMessage.ifEmpty { getString(R.string.list_unknown_error) } },
            )
            else -> lastMessage.ifEmpty { getString(R.string.list_connecting) }
        }
    }

    /** A line about the tunnel, or null when it is not what the user is waiting on. */
    private fun describeTunnel(tunnel: JSONObject): String? {
        val settings = Hypeterm.Settings.load(this)
        if (!settings.tunnelEnabled) return null
        if (!tunnel.optBoolean("available")) {
            return getString(R.string.tunnel_status_unavailable)
        }
        if (tunnel.optBoolean("running")) return null
        if (tunnel.optString("auth_url").isNotEmpty()) {
            return getString(R.string.list_tunnel_needs_login)
        }
        if (!tunnel.optBoolean("started")) return getString(R.string.list_tunnel_stopped)
        return getString(R.string.list_tunnel_starting)
    }

    private fun render(list: JSONArray) {
        terminals.removeAllViews()
        if (list.length() == 0) {
            terminals.addView(emptyLabel)
        }
        for (index in 0 until list.length()) {
            val terminal = list.optJSONObject(index) ?: continue
            val id = terminal.optString("terminal_id")
            val label = terminal.optString("label").ifEmpty { id }
            val readOnly = !terminal.optBoolean("accepts_input", false)
            val row = Button(this).apply {
                text = if (readOnly) {
                    getString(R.string.terminal_row_read_only, label,
                        terminal.optInt("columns"), terminal.optInt("rows"))
                } else {
                    getString(R.string.terminal_row, label, terminal.optInt("columns"),
                        terminal.optInt("rows"))
                }
                isAllCaps = false
                contentDescription = text
                minHeight = (48 * resources.displayMetrics.density).toInt()
                setOnClickListener { open(id) }
            }
            terminals.addView(row)
        }
        updateStatus()

        // A single session attaches directly (spec §5.1) — but only the first time.
        // Doing it on every refresh means leaving that terminal drops the user
        // straight back into it, with no way to reach this screen.
        if (list.length() == 1 && !autoOpened) {
            autoOpened = true
            list.optJSONObject(0)?.optString("terminal_id")?.let { open(it) }
        }
    }

    private fun open(terminalId: String) {
        if (terminalId.isEmpty()) return
        startActivity(Intent(this, TerminalActivity::class.java).apply {
            putExtra(TerminalActivity.EXTRA_TERMINAL_ID, terminalId)
        })
    }

    private companion object {
        const val POLL_INTERVAL_MS = 4000L
    }
}
