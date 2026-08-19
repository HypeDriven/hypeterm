package com.hypedriven.hypeterm

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.os.SystemClock
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import android.view.accessibility.AccessibilityNodeInfo
import android.widget.Button
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import org.json.JSONObject

/**
 * The terminal screen (spec §5.2).
 *
 * Layout: a connection-state line that does not cover the grid, the terminal viewport,
 * and the extra-key row. Everything below the platform layer is native.
 */
class TerminalActivity : Activity() {

    private lateinit var client: Hypeterm
    private lateinit var terminalView: TerminalView
    private lateinit var statusLine: TextView
    private lateinit var extraKeys: ExtraKeyRow
    private lateinit var copyButton: Button
    private lateinit var followButton: Button
    private lateinit var followToggle: Button
    /// What the follow control is *meant* to be showing. Not read back off the view: a
    /// fade leaves `visibility` stale for its whole duration, and an update that lands
    /// inside one would be dropped.
    private var followButtonShown = false
    private var connectivity: ConnectivityWatcher? = null
    private var lastMessage: String? = null
    private var lastMessageAt = 0L
    private var terminalId: String? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val settings = Hypeterm.Settings.load(this)
        client = Hypeterm.get(this, settings)

        if (settings.secureWindow) {
            // Screen-capture prevention is a deployment policy (spec §12).
            window.setFlags(WindowManager.LayoutParams.FLAG_SECURE,
                WindowManager.LayoutParams.FLAG_SECURE)
        }

        terminalId = intent.getStringExtra(EXTRA_TERMINAL_ID)
        setContentView(buildLayout(settings))
        wireCallbacks()

        client.native.setColors(settings.foregroundColor, settings.backgroundColor,
            settings.minimumContrast)
        val error = client.native.start()
        if (error.isNotEmpty()) {
            statusLine.text = getString(R.string.status_failed, error)
        }
        if (!client.native.hasCredentials()) {
            startActivity(Intent(this, PairingActivity::class.java))
            finish()
            return
        }
        terminalId?.let { client.native.attach(it) }
    }

    private fun buildLayout(settings: Hypeterm.Settings): View {
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.parseColor("#101216"))
        }

        statusLine = TextView(this).apply {
            setTextColor(Color.parseColor("#D8D8D8"))
            setBackgroundColor(Color.parseColor("#181C22"))
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setPadding(16, 8, 16, 8)
            gravity = Gravity.CENTER_VERTICAL
            // Connection state is text, not a coloured dot (spec §13).
            text = getString(R.string.status_connecting)
        }
        root.addView(statusLine, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT))

        terminalView = TerminalView(this).apply {
            bridge = client.native
            contentDescription = getString(R.string.terminal_content_description)
        }
        // The terminal and the follow control share a frame so the control can float
        // over the grid. TerminalView must never gain setZOrderOnTop: its surface would
        // then composite over its own siblings and swallow the control with no other
        // symptom.
        val terminalArea = FrameLayout(this)
        terminalArea.addView(terminalView, FrameLayout.LayoutParams(
            FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT))
        followButton = buildFollowButton()
        val inset = (12 * resources.displayMetrics.density).toInt()
        terminalArea.addView(followButton, FrameLayout.LayoutParams(
            FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT,
        ).apply {
            // Bottom-right: the corner a prompt and a left-aligned log line reach last.
            gravity = Gravity.BOTTOM or Gravity.END
            setMargins(inset, inset, inset, inset)
        })
        // The root's padding covers top and bottom; an edge-pinned child also needs the
        // side inset, or in landscape it sits under the gesture bar or a cutout.
        followButton.applyHorizontalSystemWindowMargin(inset)
        root.addView(terminalArea, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))

        // Session actions (spec §5.2): copy, an intentional paste, and explicit
        // reconnect and leave actions. Scrollable, so a narrow screen never squeezes a
        // button below its touch target.
        val actions = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        // First in the row, and the row scrolls: a control the user is meant to be able
        // to find must not be the one that scrolls off a narrow phone.
        followToggle = buildFollowToggle()
        actions.addView(followToggle)
        copyButton = action(R.string.copy_selection) { copySelection() }.apply {
            visibility = View.GONE
        }
        actions.addView(copyButton)
        actions.addView(action(R.string.paste) { pasteFromClipboard() })
        actions.addView(action(R.string.reconnect) { reconnect() })
        actions.addView(action(R.string.leave) { leaveSession() })
        actions.addView(action(R.string.font_smaller) { adjustFontSize(-1f) })
        actions.addView(action(R.string.font_larger) { adjustFontSize(1f) })
        val actionScroller = android.widget.HorizontalScrollView(this).apply {
            isHorizontalScrollBarEnabled = false
            addView(actions)
        }
        root.addView(actionScroller, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT))

        extraKeys = ExtraKeyRow(this).apply {
            bridge = client.native
            terminalView = this@TerminalActivity.terminalView
        }
        root.addView(extraKeys, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT))

        // Nothing follows the output until the render thread says otherwise, so the
        // control starts in whatever state the process-wide client is already in.
        applyFollowingState(client.followingOutput, animate = false)

        terminalView.onSelectionChanged = { hasSelection ->
            copyButton.visibility = if (hasSelection) View.VISIBLE else View.GONE
        }
        terminalView.onModifiersChanged = { extraKeys.refreshModifierState() }
        // Cell metrics come from the same measurement the render thread uses, so touch
        // coordinates and drawn cells agree.
        val scaledDensity = resources.displayMetrics.scaledDensity
        val measured = GlyphRasterizer().measure(settings.fontSizeSp * scaledDensity,
            resources.displayMetrics.density)
        terminalView.cellWidthPx = measured[0]
        terminalView.cellHeightPx = measured[1]
        root.applySystemWindowPadding()
        return root
    }

    /**
     * The "back to the latest output" control (spec §5.2).
     *
     * Present only while the view is *not* following, so its presence is the state and
     * there is no second control to keep in agreement with it. Turning following off
     * needs no button: a pinch, a two-finger drag, a scroll into the history or a
     * selection all do it, which is what the user meant by making one.
     */
    private fun buildFollowButton(): Button {
        val density = resources.displayMetrics.density
        return Button(this).apply {
            text = getString(R.string.follow_latest)
            contentDescription = getString(R.string.follow_latest_description)
            isAllCaps = false
            minWidth = (48 * density).toInt()
            minHeight = (48 * density).toInt()
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setPadding((16 * density).toInt(), 0, (16 * density).toInt(), 0)
            setTextColor(Color.WHITE)
            // Opaque, so it never lowers the contrast of the grid it sits over, and
            // rounded by hand: the client carries no support library to take a shape
            // from. Setting a background also drops the theme's pressed state, so the
            // pressed colour is spelled out rather than lost.
            background = android.graphics.drawable.StateListDrawable().apply {
                addState(intArrayOf(android.R.attr.state_pressed), pillShape("#4A72AB"))
                addState(IntArray(0), pillShape("#3A5C8C"))
            }
            visibility = View.GONE
            setOnClickListener {
                // Deliberately not hidden here. The control disappears when a frame
                // reports the session really is at the latest output, so a request the
                // command queue refused stays visible instead of pretending (spec §6.2).
                client.native.followLatest()
            }
        }
    }

    /**
     * The always-visible half of following the output (spec §5.2).
     *
     * The floating pill only appears once the view has stopped following, so while the
     * feature is doing its job there is nothing on screen to tell you it exists — or to
     * turn it off deliberately. This says which state you are in, all the time, and
     * toggles it. State is shown by label and by the accessibility selected flag as well
     * as by colour, never by colour alone (spec §13).
     */
    private fun buildFollowToggle(): Button = action(R.string.follow_output) {
        if (client.followingOutput) client.native.stopFollowing() else client.native.followLatest()
    }

    private fun paintFollowToggle(following: Boolean) {
        followToggle.text = getString(
            if (following) R.string.follow_output_on else R.string.follow_output_off
        )
        followToggle.contentDescription = getString(
            if (following) R.string.follow_output_on_description
            else R.string.follow_output_off_description
        )
        followToggle.isSelected = following
        followToggle.setBackgroundColor(
            Color.parseColor(if (following) "#3A5C8C" else "#22262E")
        )
        followToggle.setTextColor(if (following) Color.WHITE else Color.parseColor("#D8D8D8"))
    }

    private fun pillShape(color: String): GradientDrawable = GradientDrawable().apply {
        shape = GradientDrawable.RECTANGLE
        cornerRadius = 24 * resources.displayMetrics.density
        setColor(Color.parseColor(color))
    }

    private fun applyFollowingState(following: Boolean, animate: Boolean) {
        paintFollowToggle(following)
        val wantVisible = !following
        if (followButtonShown == wantVisible) return
        followButtonShown = wantVisible
        // Drop any fade still in flight, and with it its end action. Following can flip
        // twice inside 120 ms — double-tap to fit, then drag — and a stale end action
        // would hide a control the user needs, with no later edge to bring it back.
        followButton.animate().cancel()
        if (wantVisible) {
            followButton.alpha = if (animate) 0f else 1f
            followButton.visibility = View.VISIBLE
            // A short fade, because a control that pops into existence over a repainting
            // surface reads as terminal output. ViewPropertyAnimator honours the
            // system's animation scale, so "animations off" needs no code path here.
            if (animate) followButton.animate().alpha(1f).setDuration(120).start()
            // A screen reader cannot see a control appear, and this is the one fact it
            // has no other way to learn. Edge-triggered, so it can never fire per frame
            // of output (spec §13).
            followButton.announceForAccessibility(
                getString(R.string.follow_paused_announcement))
            return
        }
        // A view that is going away can neither announce anything nor keep accessibility
        // focus, so both move to the terminal before it does.
        if (followButton.isAccessibilityFocused) {
            terminalView.performAccessibilityAction(
                AccessibilityNodeInfo.ACTION_ACCESSIBILITY_FOCUS, null)
        }
        terminalView.announceForAccessibility(
            getString(R.string.follow_resumed_announcement))
        if (animate) {
            followButton.animate().alpha(0f).setDuration(120)
                .withEndAction { followButton.visibility = View.GONE }.start()
        } else {
            followButton.visibility = View.GONE
        }
    }

    private fun action(labelId: Int, onClick: () -> Unit): Button = Button(this).apply {
        text = getString(labelId)
        contentDescription = getString(labelId)
        isAllCaps = false
        minHeight = (48 * resources.displayMetrics.density).toInt()
        setOnClickListener { onClick() }
    }

    /** Explicit reconnect: detach and attach again, which also bumps the generation. */
    private fun reconnect() {
        val id = terminalId ?: return
        client.native.detach()
        client.native.attach(id)
    }

    private fun leaveSession() {
        client.native.detach()
        finish()
    }

    private fun adjustFontSize(deltaSp: Float) {
        val settings = Hypeterm.Settings.load(this)
        val updated = (settings.fontSizeSp + deltaSp).coerceIn(8f, 32f)
        settings.copy(fontSizeSp = updated).save(this)
        val metrics = resources.displayMetrics
        client.native.setFontSize(updated * metrics.scaledDensity, metrics.density)
        // Touch mapping follows the same measurement the renderer uses.
        val measured = GlyphRasterizer().measure(updated * metrics.scaledDensity, metrics.density)
        terminalView.cellWidthPx = measured[0]
        terminalView.cellHeightPx = measured[1]
    }

    private fun wireCallbacks() {
        client.onStatusChanged = { status -> renderStatus(status) }
        client.onTitleChanged = { title -> title.takeIf { it.isNotEmpty() }?.let { setTitle(it) } }
        client.onUserMessage = { kind, message -> showMessage(kind, message) }
        client.onBellRang = {
            terminalView.performHapticFeedback(
                android.view.HapticFeedbackConstants.KEYBOARD_TAP)
        }
        client.onClipboardWriteRequested = { text -> writeClipboard(text) }
        client.onFollowingOutputChanged = { following ->
            applyFollowingState(following, animate = true)
        }
        connectivity = ConnectivityWatcher(this) { available ->
            client.native.setNetworkAvailable(available)
        }
    }

    private fun renderStatus(status: JSONObject) {
        val state = status.optString("state")
        val label = status.optString("terminal_label")
        val readOnly = !status.optBoolean("input_available", false)
        val text = when (state) {
            "attached" -> if (readOnly) {
                getString(R.string.status_attached_read_only, label)
            } else {
                getString(R.string.status_attached, label)
            }
            "reconnecting" -> getString(R.string.status_reconnecting)
            "authenticating" -> getString(R.string.status_authenticating)
            "attaching" -> getString(R.string.status_connecting)
            "terminal_closed" -> getString(R.string.status_terminal_closed)
            "failed" -> getString(R.string.status_failed, status.optString("error_message"))
            "pairing_required" -> getString(R.string.status_pairing_required)
            else -> state
        }
        statusLine.text = text
        // Screen readers get the state as text, and the announcement is not tied to
        // terminal output (spec §13).
        statusLine.contentDescription = text
    }

    private fun showMessage(kind: String, message: String) {
        // The controller reports every input frame it drops, which is what spec §15 asks
        // for — but while a session is read-only or disconnected that is once per
        // keystroke. Typing a command then queues a toast per character, each sitting for
        // seconds over the keys the user is still pressing, and interrupts a screen
        // reader just as often. Repeating a reason the user has just been given adds
        // nothing to it, so an unchanged message is shown again only after a quiet gap.
        val now = SystemClock.uptimeMillis()
        if (message == lastMessage && now - lastMessageAt < REPEAT_MESSAGE_WINDOW_MS) return
        lastMessage = message
        lastMessageAt = now
        Toast.makeText(this, message, Toast.LENGTH_SHORT).show()
        statusLine.announceForAccessibility(message)
    }

    private fun copySelection() {
        val text = client.native.selectedText()
        if (text.isEmpty()) return
        writeClipboard(text)
        terminalView.clearSelection()
        Toast.makeText(this, R.string.copied, Toast.LENGTH_SHORT).show()
    }

    private fun writeClipboard(text: String) {
        val manager = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        manager.setPrimaryClip(ClipData.newPlainText("terminal", text))
    }

    /** Paste needs an intentional action; it is never triggered by remote output. */
    fun pasteFromClipboard() {
        val manager = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        val clip = manager.primaryClip ?: return
        if (clip.itemCount == 0) return
        val text = clip.getItemAt(0).coerceToText(this).toString()
        if (text.isEmpty()) return
        // Ahead of the paste, or it lands in the middle of a word the keyboard is still
        // holding and the two arrive out of order.
        terminalView.flushComposingText()
        client.native.paste(text)
    }

    override fun onStart() {
        super.onStart()
        connectivity?.start()
        client.native.setPaused(false)
    }

    override fun onResume() {
        super.onResume()
        terminalView.requestFocus()
        client.native.setFocused(true)
    }

    override fun onPause() {
        super.onPause()
        client.native.setFocused(false)
    }

    override fun onStop() {
        super.onStop()
        // Backgrounding follows the documented policy: the controller keeps the
        // connection while that is useful and then detaches (spec §11).
        client.native.setPaused(true)
        connectivity?.stop()
    }

    override fun onDestroy() {
        super.onDestroy()
        if (isFinishing) client.native.setSurface(null, 0, 0)
    }

    companion object {
        const val EXTRA_TERMINAL_ID = "terminal_id"
        /// Long enough to collapse a typed command's worth of refusals into one notice,
        /// short enough that a user who keeps typing is reminded rather than ignored.
        private const val REPEAT_MESSAGE_WINDOW_MS = 3000L
    }
}
