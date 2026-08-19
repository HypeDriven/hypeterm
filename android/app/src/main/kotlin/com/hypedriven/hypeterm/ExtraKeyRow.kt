package com.hypedriven.hypeterm

import android.content.Context
import android.graphics.Color
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.HorizontalScrollView
import android.widget.LinearLayout

/**
 * The compact extra-key row (spec §5.2): the keys a soft keyboard cannot produce.
 *
 * `Ctrl` and `Alt` latch for exactly one following key and show that state, because a
 * modifier you cannot see the state of is worse than no modifier at all (spec §9.2).
 * Every button carries a content description and a 48dp touch target (spec §13).
 */
class ExtraKeyRow(context: Context) : HorizontalScrollView(context) {

    private val row = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
    }

    private var ctrlButton: Button? = null
    private var altButton: Button? = null

    var bridge: NativeBridge? = null
    var terminalView: TerminalView? = null

    init {
        isHorizontalScrollBarEnabled = false
        addView(row, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT))

        ctrlButton = addModifier("Ctrl", "Control modifier", NativeBridge.Modifier.CTRL)
        altButton = addModifier("Alt", "Alt modifier", NativeBridge.Modifier.ALT)
        // Interrupt, end-of-file and suspend, as whole keys rather than a latch plus a
        // letter. The latch has to travel through the IME to meet the character the soft
        // keyboard produces, and a keyboard is free to deliver that character by a route
        // that carries no modifier with it; these do not depend on any of that. Ctrl+C in
        // particular is the only way to stop a running program, so it should not be the
        // thing that rests on the most fragile path in the app.
        addControl("^C", "Control C, interrupt", 'c')
        addControl("^D", "Control D, end of file", 'd')
        addControl("^Z", "Control Z, suspend", 'z')
        addKey("Esc", "Escape", NativeBridge.Key.ESCAPE)
        addKey("Tab", "Tab", NativeBridge.Key.TAB)
        addKey("←", "Left arrow", NativeBridge.Key.LEFT)
        addKey("↓", "Down arrow", NativeBridge.Key.DOWN)
        addKey("↑", "Up arrow", NativeBridge.Key.UP)
        addKey("→", "Right arrow", NativeBridge.Key.RIGHT)
        addKey("Home", "Home", NativeBridge.Key.HOME)
        addKey("End", "End", NativeBridge.Key.END)
        addKey("PgUp", "Page up", NativeBridge.Key.PAGE_UP)
        addKey("PgDn", "Page down", NativeBridge.Key.PAGE_DOWN)
        addKey("Enter", "Enter", NativeBridge.Key.ENTER)
    }

    /** Repaints latch state; called when a key consumes the latched modifiers. */
    fun refreshModifierState() {
        val latched = terminalView?.latchedModifiers ?: 0
        ctrlButton?.updateLatched(latched and NativeBridge.Modifier.CTRL != 0)
        altButton?.updateLatched(latched and NativeBridge.Modifier.ALT != 0)
    }

    private fun addKey(label: String, description: String, key: Int): Button {
        val button = makeButton(label, description)
        button.setOnClickListener {
            val view = terminalView
            // Tapping a button here does not move focus, so the keyboard is never told to
            // finish and whatever it is still composing has not reached the shell. This
            // key would otherwise overtake it — Enter after `cd /ho` running `cd /`.
            view?.flushComposingText()
            bridge?.sendKey(key, 0, view?.latchedModifiers ?: 0, false)
            view?.latchedModifiers = 0
            refreshModifierState()
        }
        row.addView(button)
        return button
    }

    /**
     * A control character sent as one keypress: no latch, no IME, no composition.
     *
     * The character and the modifier travel together to the encoder, which is the same
     * pair `Integration.ControlKeysAndFunctionKeysArrive` asserts arrives as a single
     * control byte.
     */
    private fun addControl(label: String, description: String, letter: Char): Button {
        val button = makeButton(label, description)
        button.setOnClickListener {
            val view = terminalView
            view?.flushComposingText()
            bridge?.sendKey(NativeBridge.Key.NONE, letter.code,
                NativeBridge.Modifier.CTRL, false)
            view?.latchedModifiers = 0
            refreshModifierState()
        }
        row.addView(button)
        return button
    }

    private fun addModifier(label: String, description: String, modifier: Int): Button {
        val button = makeButton(label, description)
        button.setOnClickListener {
            val view = terminalView ?: return@setOnClickListener
            // Anything half-typed belongs to the line the user is writing, not to the
            // control key they are about to press, so it goes now — which also leaves the
            // keyboard with nothing composing when the next character arrives.
            view.flushComposingText()
            view.latchedModifiers = view.latchedModifiers xor modifier
            refreshModifierState()
        }
        row.addView(button)
        return button
    }

    private fun makeButton(label: String, description: String): Button {
        val density = resources.displayMetrics.density
        return Button(context).apply {
            text = label
            contentDescription = description
            isAllCaps = false
            minWidth = (48 * density).toInt()
            minHeight = (48 * density).toInt()
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setPadding((10 * density).toInt(), 0, (10 * density).toInt(), 0)
            updateLatched(false)
        }
    }

    private fun View.updateLatched(latched: Boolean) {
        // State is shown by both colour and the accessibility "selected" flag, never
        // by colour alone (spec §13).
        setBackgroundColor(if (latched) Color.parseColor("#3A5C8C") else Color.parseColor("#22262E"))
        isSelected = latched
        if (this is Button) setTextColor(if (latched) Color.WHITE else Color.parseColor("#D8D8D8"))
    }
}
