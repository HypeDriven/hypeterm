package com.hypedriven.hypeterm

import android.view.Surface

/**
 * The whole JVM→native surface (spec §6.1).
 *
 * Every method here forwards to the C++ core. Nothing in this package parses terminal
 * output, speaks the relay protocol, or draws: the JVM layer exists for the platform
 * APIs that have no usable native equivalent — IME composition, the Keystore, the
 * clipboard, accessibility, connectivity and font rasterization.
 */
class NativeBridge internal constructor(@Volatile private var handle: Long) {

    /** Key codes mirroring `tmirror::input::Key`. Kept in the same order. */
    object Key {
        const val NONE = 0
        const val ENTER = 1
        const val TAB = 2
        const val BACKSPACE = 3
        const val ESCAPE = 4
        const val DELETE = 5
        const val INSERT = 6
        const val HOME = 7
        const val END = 8
        const val PAGE_UP = 9
        const val PAGE_DOWN = 10
        const val UP = 11
        const val DOWN = 12
        const val LEFT = 13
        const val RIGHT = 14
        const val F1 = 15
        const val F2 = 16
        const val F3 = 17
        const val F4 = 18
        const val F5 = 19
        const val F6 = 20
        const val F7 = 21
        const val F8 = 22
        const val F9 = 23
        const val F10 = 24
        const val F11 = 25
        const val F12 = 26
    }

    /** Modifier bits mirroring `tmirror::input::KeyModifier`. */
    object Modifier {
        const val NONE = 0
        const val SHIFT = 1
        const val ALT = 2
        const val CTRL = 4
        const val META = 8
    }

    object MouseButton {
        const val LEFT = 0
        const val MIDDLE = 1
        const val RIGHT = 2
        const val WHEEL_UP = 64
        const val WHEEL_DOWN = 65
    }

    object MouseAction {
        const val PRESS = 0
        const val RELEASE = 1
        const val MOVE = 2
    }

    val isValid: Boolean get() = handle != 0L

    fun start(): String = if (handle == 0L) "no session" else nativeStart(handle)

    /** Sets the relay URL. Only valid before [start]; returns "" on success. */
    fun setServerUrl(url: String): String =
        if (handle == 0L) "no session" else nativeSetServerUrl(handle, url)

    fun destroy() {
        if (handle != 0L) {
            nativeDestroy(handle)
            handle = 0
        }
    }

    /**
     * Takes over a freshly created native session, ending the previous one.
     *
     * The point is that nobody else has to know. Changing the relay or the tunnel means
     * building a new native session, and before this every screen holding a bridge was
     * left holding a dead one: calls became silent no-ops, the terminal list froze, and
     * the tunnel reported itself absent on a device where it was running. Swapping the
     * handle inside the object every holder already has makes a restart invisible.
     *
     * Call from the main thread with no native call in flight — the old session is torn
     * down here, and a call that overlapped it would be using freed memory.
     */
    @Synchronized
    fun adopt(replacement: Long) {
        val previous = handle
        handle = replacement
        if (previous != 0L) nativeDestroy(previous)
    }

    fun setSurface(surface: Surface?, width: Int, height: Int) {
        if (handle != 0L) nativeSetSurface(handle, surface, width, height)
    }

    fun setFontSize(fontSizePx: Float, density: Float) {
        if (handle != 0L) nativeSetFontSize(handle, fontSizePx, density)
    }

    /** Default colours and the minimum contrast ratio (spec §13). */
    fun setColors(foregroundArgb: Int, backgroundArgb: Int, minimumContrast: Float) {
        if (handle != 0L) nativeSetColors(handle, foregroundArgb, backgroundArgb, minimumContrast)
    }

    fun refreshTerminals() {
        if (handle != 0L) nativeRefreshTerminals(handle)
    }

    fun attach(terminalId: String) {
        if (handle != 0L) nativeAttach(handle, terminalId)
    }

    fun detach() {
        if (handle != 0L) nativeDetach(handle)
    }

    fun sendKey(key: Int, unicode: Int, modifiers: Int, repeat: Boolean) {
        if (handle != 0L) nativeSendKey(handle, key, unicode, modifiers, repeat)
    }

    fun sendText(text: String) {
        if (handle != 0L && text.isNotEmpty()) nativeSendText(handle, text)
    }

    /**
     * One delivery of typed text, with whatever modifier is latched.
     *
     * How it divides — which character becomes a modified keypress, and which text goes
     * before and after it — is decided natively by `input::PlanTypedText`, because it is
     * terminal input logic and belongs in core (spec §6.1). Returns true when the latch
     * was spent, which is the only part of the answer the JVM layer needs.
     */
    fun sendTypedText(pending: String, value: String, modifiers: Int): Boolean =
        handle != 0L && nativeSendTypedText(handle, pending, value, modifiers)

    fun paste(text: String) {
        if (handle != 0L && text.isNotEmpty()) nativePaste(handle, text)
    }

    fun sendMouse(button: Int, action: Int, column: Int, row: Int, modifiers: Int) {
        if (handle != 0L) nativeSendMouse(handle, button, action, column, row, modifiers)
    }

    fun scroll(lines: Int) {
        if (handle != 0L) nativeScroll(handle, lines)
    }

    fun scrollToBottom() {
        if (handle != 0L) nativeScrollToBottom(handle)
    }

    fun setSelection(startRow: Int, startColumn: Int, endRow: Int, endColumn: Int,
                     rectangular: Boolean) {
        if (handle != 0L) {
            nativeSetSelection(handle, startRow, startColumn, endRow, endColumn, rectangular)
        }
    }

    fun clearSelection() {
        if (handle != 0L) nativeClearSelection(handle)
    }

    fun selectedText(): String = if (handle == 0L) "" else nativeSelectedText(handle)

    fun visibleText(): String = if (handle == 0L) "" else nativeVisibleText(handle)

    fun setFocused(focused: Boolean) {
        if (handle != 0L) nativeSetFocused(handle, focused)
    }

    fun setPaused(paused: Boolean) {
        if (handle != 0L) nativeSetPaused(handle, paused)
    }

    fun setNetworkAvailable(available: Boolean) {
        if (handle != 0L) nativeSetNetworkAvailable(handle, available)
    }

    // ------------------------------------------------------------------ the view
    //
    // The terminal is drawn at whatever size the publisher is running at; these move
    // the window onto it rather than changing it (spec §10.4).

    /** Multiplies the zoom about a point on the surface. */
    fun zoomBy(factor: Float, focusX: Float, focusY: Float) {
        if (handle != 0L) nativeZoomBy(handle, factor, focusX, focusY)
    }

    fun panBy(dx: Float, dy: Float) {
        if (handle != 0L) nativePanBy(handle, dx, dy)
    }

    /** Fits the terminal's width to the screen, and follows the output again. */
    fun resetView() {
        if (handle != 0L) nativeResetView(handle)
    }

    /**
     * Returns to the newest output and keeps up with it.
     *
     * Covers both ways of being somewhere else: a scrollback position up in the history
     * and a view panned or zoomed away from where output arrives. Returns false when the
     * command queue refused it, in which case nothing changed and the controller has
     * already told the user (spec §6.2).
     *
     * There is no matching "stop following": a pinch, a two-finger drag, a scroll into
     * the history or a selection all end it, which is what the user meant by making one.
     */
    fun followLatest(): Boolean = handle != 0L && nativeFollowLatest(handle)

    /** Stops following. Only a change of intent, so unlike [followLatest] it cannot fail. */
    fun stopFollowing() {
        if (handle != 0L) nativeStopFollowing(handle)
    }

    /** Terminal pixels per surface pixel, so the caller can reason in screen distance. */
    fun viewScale(): Float = if (handle == 0L) 1f else nativeViewScale(handle)

    /**
     * The cell under a point on the surface, or null outside the terminal.
     *
     * Native, because the answer depends on the zoom, the pan and the cell metrics at
     * once — and a mapping split across the JNI boundary is how selection ends up a
     * cell out.
     */
    fun cellAt(x: Float, y: Float): Pair<Int, Int>? {
        if (handle == 0L) return null
        val packed = nativeCellAt(handle, x, y)
        if (packed < 0) return null
        val row = (packed shr 32).toInt()
        val column = (packed and 0xFFFFFFFFL).toInt()
        return row to column
    }

    fun status(): String = if (handle == 0L) "{}" else nativeStatus(handle)

    fun beginPairing(): String = if (handle == 0L) "{}" else nativeBeginPairing(handle)

    fun completePairing(identityId: String, deviceId: String): String =
        if (handle == 0L) "no session" else nativeCompletePairing(handle, identityId, deviceId)

    /**
     * Finishes pairing from a code produced by the owner's machine.
     *
     * Blocking — several HTTP round trips. Call it off the main thread.
     */
    fun completePairingWithCode(code: String): String =
        if (handle == 0L) "{\"error\":\"no session\"}"
        else nativeCompletePairingWithCode(handle, code)

    /**
     * Asks a machine to open a terminal, and returns it as JSON (relay spec §4.6).
     *
     * Blocking — the far machine has to start a process. Call it off the main thread.
     * The request carries only a label and a size: what runs is the other machine's
     * decision, and it refuses outright unless its owner turned this on.
     */
    fun openTerminal(deviceId: String, label: String, columns: Int, rows: Int): String =
        if (handle == 0L) "{\"error\":\"no session\"}"
        else nativeOpenTerminal(handle, deviceId, label, columns, rows)

    /** The machines this identity owns. Blocking; call it off the main thread. */
    fun listDevices(): String =
        if (handle == 0L) "{\"devices\":[]}" else nativeListDevices(handle)

    fun hasCredentials(): Boolean = handle != 0L && nativeHasCredentials(handle)

    fun forgetCredentials() {
        if (handle != 0L) nativeForgetCredentials(handle)
    }

    /**
     * The embedded Tailscale node (spec §7.4).
     *
     * Returns a JSON document: `available` (the node is part of this build),
     * `started`, `running`, `backend_state`, `auth_url`, `hostname`, `addresses`,
     * `peers`, `no_log_upload` and `last_error`. It never contains the auth key.
     */
    fun tunnelStatus(): String = if (handle == 0L) "{}" else nativeTunnelStatus(handle)

    /** Starts the node. An empty [authKey] means an interactive browser login. */
    fun tunnelStart(authKey: String): String =
        if (handle == 0L) "no session" else nativeTunnelStart(handle, authKey)

    fun tunnelStop() {
        if (handle != 0L) nativeTunnelStop(handle)
    }

    /** Stops the node and forgets its key, so the next start needs authorising again. */
    fun tunnelLogout(): String = if (handle == 0L) "no session" else nativeTunnelLogout(handle)

    private external fun nativeStart(handle: Long): String
    private external fun nativeDestroy(handle: Long)
    private external fun nativeSetSurface(handle: Long, surface: Surface?, width: Int, height: Int)
    private external fun nativeSetFontSize(handle: Long, fontSizePx: Float, density: Float)
    private external fun nativeSetServerUrl(handle: Long, url: String): String
    private external fun nativeSetColors(handle: Long, foregroundArgb: Int,
                                         backgroundArgb: Int, minimumContrast: Float)
    private external fun nativeRefreshTerminals(handle: Long)
    private external fun nativeAttach(handle: Long, terminalId: String)
    private external fun nativeDetach(handle: Long)
    private external fun nativeSendKey(handle: Long, key: Int, unicode: Int, modifiers: Int,
                                       repeat: Boolean)
    private external fun nativeSendText(handle: Long, text: String)
    private external fun nativeSendTypedText(handle: Long, pending: String, value: String,
                                             modifiers: Int): Boolean
    private external fun nativePaste(handle: Long, text: String)
    private external fun nativeSendMouse(handle: Long, button: Int, action: Int, column: Int,
                                         row: Int, modifiers: Int)
    private external fun nativeScroll(handle: Long, lines: Int)
    private external fun nativeScrollToBottom(handle: Long)
    private external fun nativeSetSelection(handle: Long, startRow: Int, startColumn: Int,
                                            endRow: Int, endColumn: Int, rectangular: Boolean)
    private external fun nativeClearSelection(handle: Long)
    private external fun nativeSelectedText(handle: Long): String
    private external fun nativeVisibleText(handle: Long): String
    private external fun nativeSetFocused(handle: Long, focused: Boolean)
    private external fun nativeSetPaused(handle: Long, paused: Boolean)
    private external fun nativeSetNetworkAvailable(handle: Long, available: Boolean)
    private external fun nativeZoomBy(handle: Long, factor: Float, focusX: Float, focusY: Float)
    private external fun nativePanBy(handle: Long, dx: Float, dy: Float)
    private external fun nativeResetView(handle: Long)
    private external fun nativeFollowLatest(handle: Long): Boolean
    private external fun nativeStopFollowing(handle: Long)
    private external fun nativeViewScale(handle: Long): Float
    private external fun nativeCellAt(handle: Long, x: Float, y: Float): Long
    private external fun nativeStatus(handle: Long): String
    private external fun nativeBeginPairing(handle: Long): String
    private external fun nativeCompletePairing(handle: Long, identityId: String,
                                               deviceId: String): String
    private external fun nativeCompletePairingWithCode(handle: Long, code: String): String
    private external fun nativeOpenTerminal(handle: Long, deviceId: String, label: String,
                                           columns: Int, rows: Int): String
    private external fun nativeListDevices(handle: Long): String
    private external fun nativeHasCredentials(handle: Long): Boolean
    private external fun nativeForgetCredentials(handle: Long)
    private external fun nativeTunnelStatus(handle: Long): String
    private external fun nativeTunnelStart(handle: Long, authKey: String): String
    private external fun nativeTunnelStop(handle: Long)
    private external fun nativeTunnelLogout(handle: Long): String

    companion object {
        init {
            System.loadLibrary("hypeterm")
        }

        /// Builds a native session and returns its handle, so a caller can swap one into
        /// an existing bridge rather than replacing the bridge itself.
        fun newHandle(
            configJson: String,
            preferencesPath: String,
            secureStore: KeystoreSecureStore,
            rasterizer: GlyphRasterizer,
            callbacks: NativeCallbacks,
            fontSizePx: Float,
            density: Float,
        ): Long = nativeCreate(configJson, preferencesPath, secureStore, rasterizer,
            callbacks, fontSizePx, density)

        @JvmStatic
        private external fun nativeCreate(
            configJson: String,
            preferencesPath: String,
            secureStore: Any,
            rasterizer: Any,
            callbacks: Any,
            fontSizePx: Float,
            density: Float,
        ): Long
    }
}

/**
 * Callbacks from the native controller. They arrive on the native network thread, so
 * every implementation must marshal to the main thread before touching the UI.
 */
interface NativeCallbacks {
    fun onStatus(statusJson: String)
    fun onTerminals(terminalsJson: String)
    fun onTitle(title: String)
    fun onMessage(kind: String, message: String)
    fun onBell()
    fun onClipboardWrite(text: String)

    /**
     * Whether the view is keeping up with the newest output.
     *
     * Arrives on the native *render* thread, not the network thread, and only when the
     * answer changes — an idle terminal reports nothing.
     */
    fun onFollowOutputChanged(following: Boolean)
}
