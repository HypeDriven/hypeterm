package com.hypedriven.hypeterm

import android.content.Context
import android.graphics.Rect
import android.os.Bundle
import android.text.InputType
import android.util.AttributeSet
import android.view.GestureDetector
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.ScaleGestureDetector
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.accessibility.AccessibilityNodeInfo
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import kotlin.math.abs
import kotlin.math.roundToInt

/**
 * The terminal viewport.
 *
 * It owns no terminal state: the surface goes to the native render thread, input goes
 * to the native controller, and what it draws is decided there (spec §6.1, §6.2). Its
 * job is the platform's: surface lifecycle, IME, touch and accessibility.
 */
class TerminalView @JvmOverloads constructor(
    context: Context,
    attrs: AttributeSet? = null,
) : SurfaceView(context, attrs), SurfaceHolder.Callback {

    var bridge: NativeBridge? = null
    /** Cell size in device pixels, published by the render thread. */
    var cellWidthPx: Float = 10f
    var cellHeightPx: Float = 20f
    /** Latched modifiers from the extra-key row, consumed by the next key. */
    var latchedModifiers: Int = 0
    var onSelectionChanged: ((hasSelection: Boolean) -> Unit)? = null
    /** The latched modifiers changed, so the extra-key row needs repainting. */
    var onModifiersChanged: (() -> Unit)? = null

    private var scrollRemainder = 0f
    private var selecting = false
    private var selectionStartRow = 0
    private var selectionStartColumn = 0

    /// Focus of the last two-finger gesture, so a drag pans as well as a pinch zooms.
    private var lastFocusX = 0f
    private var lastFocusY = 0f
    private var pinching = false
    /// The connection the IME is currently served by, so input arriving from anywhere
    /// else can make it give up what it is holding first.
    private var inputConnection: TerminalInputConnection? = null
    /// One cancel per gesture: MotionEvent.obtain allocates, and a pinch delivers moves
    /// continuously.
    private var gestureCancelled = false
    /// Where the current gesture started, so a stationary tap can be told from a drag.
    private var downX = 0f
    private var downY = 0f

    private val scaleDetector = ScaleGestureDetector(context, object :
        ScaleGestureDetector.SimpleOnScaleGestureListener() {

        override fun onScaleBegin(detector: ScaleGestureDetector): Boolean {
            pinching = true
            lastFocusX = detector.focusX
            lastFocusY = detector.focusY
            // A two-finger gesture is never a selection; drop one in progress rather
            // than extending it to wherever the fingers land.
            if (selecting) clearSelection()
            return true
        }

        override fun onScale(detector: ScaleGestureDetector): Boolean {
            // The focus moving *is* a two-finger drag, so panning and zooming come
            // from the same gesture without a separate mode.
            bridge?.panBy(detector.focusX - lastFocusX, detector.focusY - lastFocusY)
            lastFocusX = detector.focusX
            lastFocusY = detector.focusY
            bridge?.zoomBy(detector.scaleFactor, detector.focusX, detector.focusY)
            return true
        }

        override fun onScaleEnd(detector: ScaleGestureDetector) {
            pinching = false
        }
    })

    private val gestureDetector = GestureDetector(context, object :
        GestureDetector.SimpleOnGestureListener() {

        override fun onDown(event: MotionEvent): Boolean = true

        override fun onSingleTapUp(event: MotionEvent): Boolean {
            if (selecting) {
                clearSelection()
                return true
            }
            requestFocus()
            showKeyboard()
            // Ahead of the click bytes: a mouse report that overtakes a half-typed word
            // moves the cursor in the remote application before the word arrives.
            flushComposingText()
            // A tap is also a click for an application that asked for mouse reporting.
            // The controller drops it when tracking is off, so the view does not need
            // to know the terminal's mode.
            val cell = bridge?.cellAt(event.x, event.y) ?: return true
            val (row, column) = cell
            bridge?.sendMouse(NativeBridge.MouseButton.LEFT, NativeBridge.MouseAction.PRESS,
                column, row, latchedModifiers)
            bridge?.sendMouse(NativeBridge.MouseButton.LEFT, NativeBridge.MouseAction.RELEASE,
                column, row, latchedModifiers)
            return true
        }

        override fun onScroll(
            first: MotionEvent?, current: MotionEvent, distanceX: Float, distanceY: Float,
        ): Boolean {
            if (selecting) {
                extendSelectionTo(current)
                return true
            }
            // Vertical drag scrolls the scrollback; the accumulator keeps sub-cell
            // movement from being lost, which is what makes slow scrolling feel right.
            if (abs(distanceY) < abs(distanceX)) return false
            scrollRemainder += distanceY / scaledCellHeight()
            val lines = scrollRemainder.toInt()
            if (lines != 0) {
                scrollRemainder -= lines
                bridge?.scroll(lines)
            }
            return true
        }

        override fun onDoubleTap(event: MotionEvent): Boolean {
            // Back to fitting the width, which is the one view a user can always
            // return to when they have zoomed themselves somewhere confusing.
            bridge?.resetView()
            return true
        }

        override fun onLongPress(event: MotionEvent) {
            beginSelection(event)
        }
    })

    init {
        holder.addCallback(this)
        isFocusable = true
        isFocusableInTouchMode = true
        importantForAccessibility = IMPORTANT_FOR_ACCESSIBILITY_YES
    }

    // ------------------------------------------------------------------ surface

    override fun surfaceCreated(holder: SurfaceHolder) {}

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        bridge?.setSurface(holder.surface, width, height)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        // Surface loss stops rendering but keeps terminal state (spec §11).
        bridge?.setSurface(null, 0, 0)
    }

    // ---------------------------------------------------------------------- IME

    override fun onCheckIsTextEditor(): Boolean = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        // A terminal is not a text field: predictions, auto-capitalisation and
        // personalised learning would all corrupt what the user typed, and the last
        // of them would send terminal input to a keyboard's language model.
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or
            InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD or
            InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_ACTION_NONE or
            EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
        outAttrs.initialSelStart = -1
        outAttrs.initialSelEnd = -1
        val connection = TerminalInputConnection(this)
        inputConnection = connection
        return connection
    }

    /**
     * Sends whatever the keyboard is still composing.
     *
     * Composed text has not reached the shell yet, and input that does not come through
     * the IME — the extra-key row, a paste — is posted straight onto the command queue,
     * so without this it overtakes the half-typed word. Tapping Enter after `cd /ho`
     * would run `cd /` and leave `ho` at the next prompt.
     */
    fun flushComposingText() {
        if (inputConnection?.flushPending() != true) return
        // The keyboard still believes it has a composing region, and would send the same
        // text again when it finishes.
        restartInput()
    }

    override fun onDetachedFromWindow() {
        super.onDetachedFromWindow()
        inputConnection = null
    }

    fun showKeyboard() {
        val manager = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        manager.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)
    }

    fun hideKeyboard() {
        val manager = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        manager.hideSoftInputFromWindow(windowToken, 0)
    }

    /// Makes the IME throw away whatever it thinks is composing and start again.
    fun restartInput() {
        val manager = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        manager.restartInput(this)
    }

    // -------------------------------------------------------------- key events

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        if (dispatchKey(keyCode, event)) return true
        return super.onKeyDown(keyCode, event)
    }

    override fun onKeyMultiple(keyCode: Int, repeatCount: Int, event: KeyEvent): Boolean {
        if (keyCode == KeyEvent.KEYCODE_UNKNOWN && event.characters != null) {
            flushComposingText()
            sendTypedText("", event.characters)
            return true
        }
        return super.onKeyMultiple(keyCode, repeatCount, event)
    }

    /**
     * Sends text the user typed, together with whatever modifier is latched.
     *
     * A soft keyboard delivers ordinary letters as *committed text*, never as key
     * events, so text and modifiers arrive by different routes and something has to join
     * them up. That something is `input::PlanTypedText` in the native core, not this
     * class: it is terminal input logic (spec §6.1), and keeping it here is what made it
     * impossible to test anywhere but on a phone with a particular keyboard.
     *
     * [pending] is what the keyboard is still composing and has not sent; [value] is
     * what it now says that composition is.
     */
    fun sendTypedText(pending: String, value: String) {
        val bridge = this.bridge ?: return
        if (value.isEmpty()) return
        if (bridge.sendTypedText(pending, value, latchedModifiers)) consumeLatched()
    }

    fun dispatchKey(keyCode: Int, event: KeyEvent): Boolean {
        val bridge = this.bridge ?: return false
        // A hardware key is posted straight onto the command queue and would otherwise
        // arrive ahead of whatever the on-screen keyboard is still composing. Free when
        // there is nothing pending, and the IME's own path has already flushed by here.
        flushComposingText()
        var modifiers = latchedModifiers
        if (event.isShiftPressed) modifiers = modifiers or NativeBridge.Modifier.SHIFT
        if (event.isAltPressed) modifiers = modifiers or NativeBridge.Modifier.ALT
        if (event.isCtrlPressed) modifiers = modifiers or NativeBridge.Modifier.CTRL
        if (event.isMetaPressed) modifiers = modifiers or NativeBridge.Modifier.META

        val named = namedKeyFor(keyCode)
        if (named != NativeBridge.Key.NONE) {
            bridge.sendKey(named, 0, modifiers, event.repeatCount > 0)
            consumeLatched()
            return true
        }

        // Printable keys: the platform has already applied shift and any dead-key
        // composition, so its unicode value is authoritative (spec §9.2).
        val unicode = if (modifiers and NativeBridge.Modifier.CTRL != 0) {
            // Ctrl+<key> must see the unmodified character, otherwise Ctrl+Shift+C
            // would not map to the same control byte as Ctrl+C.
            event.getUnicodeChar(event.metaState and KeyEvent.META_CTRL_MASK.inv())
        } else {
            event.unicodeChar
        }
        if (unicode != 0) {
            bridge.sendKey(NativeBridge.Key.NONE, unicode, modifiers, event.repeatCount > 0)
            consumeLatched()
            return true
        }
        return false
    }

    /** Sends a named key with the latched modifiers applied, then consumes the latch. */
    fun sendLatchedKey(key: Int) {
        bridge?.sendKey(key, 0, latchedModifiers, false)
        consumeLatched()
    }

    private fun consumeLatched() {
        if (latchedModifiers != 0) {
            latchedModifiers = 0
            // Its own callback, not onSelectionChanged(false): a key consuming the latch
            // says nothing about the selection, and borrowing that callback to repaint
            // the modifier row also took the "Copy selection" action away while the
            // highlight was still on screen.
            onModifiersChanged?.invoke()
        }
    }

    private fun namedKeyFor(keyCode: Int): Int = when (keyCode) {
        KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> NativeBridge.Key.ENTER
        KeyEvent.KEYCODE_TAB -> NativeBridge.Key.TAB
        KeyEvent.KEYCODE_DEL -> NativeBridge.Key.BACKSPACE
        KeyEvent.KEYCODE_FORWARD_DEL -> NativeBridge.Key.DELETE
        KeyEvent.KEYCODE_ESCAPE -> NativeBridge.Key.ESCAPE
        KeyEvent.KEYCODE_INSERT -> NativeBridge.Key.INSERT
        KeyEvent.KEYCODE_MOVE_HOME -> NativeBridge.Key.HOME
        KeyEvent.KEYCODE_MOVE_END -> NativeBridge.Key.END
        KeyEvent.KEYCODE_PAGE_UP -> NativeBridge.Key.PAGE_UP
        KeyEvent.KEYCODE_PAGE_DOWN -> NativeBridge.Key.PAGE_DOWN
        KeyEvent.KEYCODE_DPAD_UP -> NativeBridge.Key.UP
        KeyEvent.KEYCODE_DPAD_DOWN -> NativeBridge.Key.DOWN
        KeyEvent.KEYCODE_DPAD_LEFT -> NativeBridge.Key.LEFT
        KeyEvent.KEYCODE_DPAD_RIGHT -> NativeBridge.Key.RIGHT
        in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 ->
            NativeBridge.Key.F1 + (keyCode - KeyEvent.KEYCODE_F1)
        else -> NativeBridge.Key.NONE
    }

    // ------------------------------------------------------------------- touch

    override fun onTouchEvent(event: MotionEvent): Boolean {
        if (event.actionMasked == MotionEvent.ACTION_DOWN) {
            gestureCancelled = false
            downX = event.x
            downY = event.y
        }
        scaleDetector.onTouchEvent(event)
        // While two fingers are down the gesture belongs to the view, not the
        // terminal: passing it on would scroll the scrollback at the same time.
        if (pinching || event.pointerCount > 1) {
            // GestureDetector keeps its long-press timer running on events it is no
            // longer being given, so a pinch held past the timeout fires a long press
            // and begins a selection under fingers that are zooming. Cancelling it is
            // the only way to stop that; the detector ignores the rest of this gesture
            // either way.
            cancelPendingGesture(event)
            return true
        }
        // Extending a selection cannot go through GestureDetector: it suppresses every
        // move after a long press, so onScroll never fires again for that gesture and a
        // long-press-and-drag would select only the cell it started on.
        //
        // Beyond the touch slop, and no sooner. A stationary tap still reports moves —
        // that is what slop is for — and taking those here would cancel the detector's
        // pending tap, which is the gesture that dismisses a selection: every attempt to
        // tap one away would instead drag it to wherever the user tapped.
        if (selecting && event.actionMasked == MotionEvent.ACTION_MOVE && movedBeyondSlop(event)) {
            // Past that point the detector stops being given the moves, so its long-press
            // timer is still armed and its tap still pending: a drag over an existing
            // selection would re-anchor it partway through and clear it on release.
            cancelPendingGesture(event)
            extendSelectionTo(event)
            return true
        }
        if (gestureDetector.onTouchEvent(event)) return true
        if (event.actionMasked == MotionEvent.ACTION_UP && selecting) {
            onSelectionChanged?.invoke(true)
            return true
        }
        return super.onTouchEvent(event)
    }

    override fun onGenericMotionEvent(event: MotionEvent): Boolean {
        if (event.actionMasked == MotionEvent.ACTION_SCROLL) {
            val vertical = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
            if (vertical != 0f) {
                bridge?.scroll(vertical.roundToInt() * 3)
                return true
            }
        }
        return super.onGenericMotionEvent(event)
    }

    private fun movedBeyondSlop(event: MotionEvent): Boolean {
        val slop = android.view.ViewConfiguration.get(context).scaledTouchSlop
        return abs(event.x - downX) > slop || abs(event.y - downY) > slop
    }

    /// Tells the gesture detector this gesture is over, so a pending long press cannot
    /// still fire after the view has taken the events for itself.
    private fun cancelPendingGesture(event: MotionEvent) {
        if (gestureCancelled) return
        gestureCancelled = true
        val cancel = MotionEvent.obtain(event)
        cancel.action = MotionEvent.ACTION_CANCEL
        gestureDetector.onTouchEvent(cancel)
        cancel.recycle()
    }

    private fun beginSelection(event: MotionEvent) {
        val (row, column) = bridge?.cellAt(event.x, event.y) ?: return
        selecting = true
        selectionStartRow = row
        selectionStartColumn = column
        bridge?.setSelection(row, column, row, column, false)
        performHapticFeedback(android.view.HapticFeedbackConstants.LONG_PRESS)
        onSelectionChanged?.invoke(true)
    }

    private fun extendSelectionTo(event: MotionEvent) {
        val (row, column) = bridge?.cellAt(event.x, event.y) ?: return
        bridge?.setSelection(selectionStartRow, selectionStartColumn, row, column, false)
    }

    fun clearSelection() {
        selecting = false
        bridge?.clearSelection()
        onSelectionChanged?.invoke(false)
    }

    val hasSelection: Boolean get() = selecting

    /// Scrollback movement is measured in cells, so it needs the on-screen height of
    /// one — which the zoom changes. Without this a drag scrolls twice as far when
    /// zoomed in, which feels like the terminal fighting the finger.
    private fun scaledCellHeight(): Float =
        (cellHeightPx * (bridge?.viewScale() ?: 1f)).coerceAtLeast(1f)

    // ----------------------------------------------------------- accessibility

    override fun onInitializeAccessibilityNodeInfo(info: AccessibilityNodeInfo) {
        super.onInitializeAccessibilityNodeInfo(info)
        // The visible grid is exposed as the node's text so a screen reader can read
        // the terminal. High-frequency output does not announce itself: only an
        // explicit accessibility request pulls the current text (spec §13).
        info.className = "android.widget.TextView"
        info.text = bridge?.visibleText() ?: ""
        info.isFocusable = true
        info.isLongClickable = true
    }

    override fun performAccessibilityAction(action: Int, arguments: Bundle?): Boolean {
        if (action == AccessibilityNodeInfo.ACTION_ACCESSIBILITY_FOCUS) {
            invalidate()
        }
        return super.performAccessibilityAction(action, arguments)
    }

    override fun onFocusChanged(gainFocus: Boolean, direction: Int, previouslyFocusedRect: Rect?) {
        super.onFocusChanged(gainFocus, direction, previouslyFocusedRect)
        bridge?.setFocused(gainFocus)
    }
}

/**
 * Terminal-flavoured input connection (spec §9.1).
 *
 * A terminal has no editable buffer for the IME to reason about: the remote shell owns
 * the line. So the connection keeps a scratch editor purely so the IME has somewhere to
 * compose, sends its contents the moment the composition is committed, and empties it
 * again — and a deletion becomes a backspace key rather than an edit of text the IME
 * imagines exists.
 *
 * The scratch editor is why this is a *full* editor rather than the dummy one. In dummy
 * mode `BaseInputConnection` turns committed text back into key events, which is a
 * second, lossier path to the same place; owning the buffer keeps one path.
 */
private class TerminalInputConnection(private val view: TerminalView) :
    BaseInputConnection(view, /* fullEditor = */ true) {

    override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
        if (view.latchedModifiers != 0) return sendWithLatchedModifier(text)
        // super replaces the composing region rather than appending after it, so a
        // keyboard that composes "ls" and then commits "ls" sends the word once.
        super.commitText(text, newCursorPosition)
        flush()
        return true
    }

    /**
     * Handles an IME update that arrived with a modifier latched.
     *
     * A latched modifier means the user is pressing a control key, not writing a word,
     * and a soft keyboard delivers that key as text like any other. Two things follow.
     *
     * It cannot wait for the composition to finish: keyboards that compose ordinary
     * letters — Samsung's does — would hold `Ctrl` `C` until the *next* keystroke, which
     * is the one moment it must not wait, because there is no other way to interrupt a
     * running program from the on-screen keyboard.
     *
     * And only what this update *adds* is the control key. Whatever was already
     * composing is ordinary text the user typed before reaching for the modifier, so it
     * goes first and unmodified. Latching a modifier already flushes, so in practice
     * there is nothing pending — this is what keeps a keyboard that recomposes anyway
     * from sending the word twice. Updates that are not an append never reach here; see
     * [isNewKeypress].
     */
    private fun sendWithLatchedModifier(text: CharSequence?): Boolean {
        val pending = editable?.toString().orEmpty()
        val value = text?.toString().orEmpty()
        // Hand both halves over exactly as the keyboard reported them and let the core
        // decide. It answers "nothing happened" for an update that is an edit rather
        // than a keypress — a backspace, an autocorrect, a swipe — and the composition
        // is then left to be tracked as usual.
        val before = view.latchedModifiers
        view.sendTypedText(pending, value)
        if (view.latchedModifiers == before) {
            return super.setComposingText(text, 1)
        }
        editable?.clear()
        // The keyboard still believes it has a composing region; left alone it would
        // extend it on the next character and send this one a second time.
        view.restartInput()
        return true
    }

    override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
        if (view.latchedModifiers != 0) return sendWithLatchedModifier(text)
        // Intermediate states are not sent (spec §9.1): predictive and multi-stage input
        // methods produce many, and a shell would see every one. They are still tracked,
        // because the composition is exactly what a later commit sends.
        return super.setComposingText(text, newCursorPosition)
    }

    override fun finishComposingText(): Boolean {
        super.finishComposingText()
        // Not a no-op, and this is the whole of the bug it replaces: a keyboard that
        // composes ordinary letters — Samsung's does, whatever the editor asks for —
        // ends a word here and never sends a separate commit. Dropping this drops
        // everything the user typed, silently, with no frame reaching the relay.
        flush()
        return true
    }

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        // The latch belongs to the first key only, exactly as it does for a hardware
        // one; sendLatchedKey consumes it so a repeat does not carry it too.
        repeat(beforeLength.coerceIn(0, 64)) { view.sendLatchedKey(NativeBridge.Key.BACKSPACE) }
        repeat(afterLength.coerceIn(0, 64)) { view.sendLatchedKey(NativeBridge.Key.DELETE) }
        return true
    }

    override fun sendKeyEvent(event: KeyEvent): Boolean {
        if (event.action != KeyEvent.ACTION_DOWN) return true
        // Whatever is still composing was typed before this key and has to reach the
        // shell before it, or Enter would arrive ahead of the line it submits.
        flush()
        return view.dispatchKey(event.keyCode, event)
    }

    override fun performEditorAction(actionCode: Int): Boolean {
        flush()
        view.sendLatchedKey(NativeBridge.Key.ENTER)
        return true
    }

    private fun flush() {
        flushPending()
    }

    /**
     * Sends what the scratch editor holds, empties it so the next commit is clean, and
     * reports whether there was anything to send.
     *
     * Always unmodified: a keypress arriving while a modifier is latched is taken by
     * [sendWithLatchedModifier] before it ever reaches the editor, so what is left here
     * is text the user finished typing.
     */
    fun flushPending(): Boolean {
        val content = editable ?: return false
        if (content.isEmpty()) return false
        val text = content.toString()
        content.clear()
        view.bridge?.sendText(text)
        return true
    }
}
