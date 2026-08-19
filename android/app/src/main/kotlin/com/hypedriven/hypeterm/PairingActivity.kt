package com.hypedriven.hypeterm

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import android.text.InputType
import android.util.TypedValue
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import org.json.JSONObject

/**
 * Pairing (relay reconciliation §2.2), which replaces the sign-in screen the client
 * specification originally assumed.
 *
 * There are no usernames and no passwords. The ordinary path is a pairing code, printed
 * by `hypeterm-publish pair-code` on the machine that holds the identity key: it lends
 * this device that identity's authority for a few minutes and carries the relay address
 * with it. The device still generates its own key and signs its own registration
 * challenge, so the private half never leaves it — the code only authorises the request.
 * Nothing else has to be filled in, which is why this screen leads with it.
 *
 * The fields below that are the older two-step flow, where the owner registers a key
 * they were shown. The production relay refuses it, because it requires the device to
 * sign for itself; the development relay allows it. It is kept for that, and because
 * this screen doubles as connection settings.
 */
class PairingActivity : Activity() {

    private lateinit var client: Hypeterm
    private lateinit var tunnelPanel: TunnelPanel
    /// The raw key as the relay would see it. The view holds a labelled, wrapped version
    /// for reading; pasting that into a terminal would paste the label with it.
    private lateinit var pairingCodeField: EditText
    private lateinit var pairButton: Button

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val settings = Hypeterm.Settings.load(this)
        client = Hypeterm.get(this, settings)
        tunnelPanel = TunnelPanel(this) { restartSession(it) }
        setContentView(buildLayout(settings))
        startClient(settings)
    }

    override fun onResume() {
        super.onResume()
        tunnelPanel.onResume()
    }

    override fun onPause() {
        tunnelPanel.onPause()
        super.onPause()
    }

    private fun startClient(settings: Hypeterm.Settings) {
        if (settings.serverUrl.isNotEmpty()) client.native.setServerUrl(settings.serverUrl)
        client.native.start()
    }

    /**
     * Rebuilds the native session so a tunnel change takes effect.
     *
     * Whether connections go through the tunnel is decided when the controller is
     * constructed, deliberately: a live session must not change the path its traffic
     * takes underneath itself.
     */
    private fun restartSession(settings: Hypeterm.Settings) {
        // In place, so every screen holding this client keeps working. Tearing the
        // singleton down and building another left the list and terminal screens bound
        // to a bridge whose handle was zero — every call a silent no-op.
        client.restart(this, settings)
        startClient(settings)
    }

    private fun buildLayout(settings: Hypeterm.Settings): View {
        val density = resources.displayMetrics.density
        val padding = (16 * density).toInt()

        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(padding, padding, padding, padding)
            setBackgroundColor(Color.parseColor("#101216"))
        }

        // The code flow first, and on its own. It is the one that works against the real
        // relay, and it needs neither a generated key nor a typed URL: the native side
        // makes the key if there is none, and the code carries the relay address. Leading
        // with the manual flow told people to do two things by hand that this does for
        // them, and described a path the production relay refuses.
        column.addView(label(getString(R.string.pairing_explanation)))

        pairingCodeField = EditText(this).apply {
            hint = getString(R.string.pairing_code_hint)
            // The code is a credential for the few minutes it lives, and long enough
            // that a keyboard's suggestions would only get in the way.
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        }
        column.addView(pairingCodeField)

        pairButton = Button(this).apply {
            text = getString(R.string.pair_with_code)
            isAllCaps = false
            setOnClickListener { pairWithCode() }
        }
        column.addView(pairButton)

        // Which relay this device is paired to. Shown, not editable: pairing sets it
        // from the code, and the controller refuses to change it while it is running —
        // an editable field could only ever persist a value the running session ignored.
        // Pasting a code from a different relay is how you move.
        column.addView(heading(getString(R.string.pairing_relay_heading)))
        column.addView(label(settings.serverUrl.ifEmpty {
            getString(R.string.pairing_relay_unset)
        }).apply { typeface = android.graphics.Typeface.MONOSPACE })

        column.addView(tunnelPanel.build(settings))

        return ScrollView(this).apply {
            addView(column)
            applySystemWindowPadding()
        }
    }

    /// Marks where the ordinary path ends and the manual one begins, so nobody reads the
    /// manual instructions as the next step of the one above.
    private fun heading(text: String): TextView = TextView(this).apply {
        this.text = text
        setTextColor(Color.WHITE)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
        typeface = android.graphics.Typeface.DEFAULT_BOLD
        setPadding(0, 40, 0, 8)
        // A visual heading is a heading to a screen reader too (spec §13).
        if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.P) {
            isAccessibilityHeading = true
        }
    }

    private fun label(text: String): TextView = TextView(this).apply {
        this.text = text
        setTextColor(Color.parseColor("#D8D8D8"))
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        setPadding(0, 16, 0, 16)
    }



    /**
     * Pairs from a code: the device signs its own registration challenge, and the code
     * only lends it the owner's authority to make the request.
     *
     * Off the main thread — it is several HTTP round trips over a tunnel that may
     * still be waking up.
     */
    private fun pairWithCode() {
        val code = pairingCodeField.text.toString().trim()
        if (code.isEmpty()) {
            Toast.makeText(this, R.string.pairing_code_required, Toast.LENGTH_SHORT).show()
            return
        }
        pairButton.isEnabled = false
        pairButton.text = getString(R.string.pairing_in_progress)
        Thread({
            val result = runCatching {
                JSONObject(client.native.completePairingWithCode(code))
            }.getOrNull()
            runOnUiThread {
                pairButton.isEnabled = true
                pairButton.text = getString(R.string.pair_with_code)
                val relayUrl = result?.optString("server_url").orEmpty()
                if (relayUrl.isEmpty()) {
                    val message = result?.optString("error").orEmpty()
                    Toast.makeText(this, message.ifEmpty { "pairing failed" },
                        Toast.LENGTH_LONG).show()
                    return@runOnUiThread
                }
                // The relay address arrived with the code, so it becomes the one this
                // client uses; the session is rebuilt against it before anything
                // tries to connect.
                pairingCodeField.setText("")
                val settings = Hypeterm.Settings.load(this).copy(serverUrl = relayUrl)
                settings.save(this)
                restartSession(settings)
                startActivity(Intent(this, TerminalListActivity::class.java))
                finish()
            }
        }, "pairing").start()
    }

}
