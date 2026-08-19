package com.hypedriven.hypeterm

import android.os.Build
import android.view.View
import android.view.WindowInsets

/**
 * Keeps content out from under the system bars and the keyboard.
 *
 * Targeting SDK 35 means the window is edge-to-edge: the app draws behind the status
 * and navigation bars unless it says otherwise. For a terminal that matters twice over
 * — the connection-state line must not sit under the clock (spec §5.2 asks for an
 * indicator that does not obscure the terminal), and the grid must not sit under the
 * soft keyboard, or half the visible rows are unreadable while typing.
 *
 * The bottom inset takes whichever is larger, the navigation bar or the IME, so the
 * layout is correct with the keyboard both up and down.
 */
fun View.applySystemWindowPadding() {
    setOnApplyWindowInsetsListener { view, insets ->
        val top: Int
        val bottom: Int
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val bars = insets.getInsets(WindowInsets.Type.systemBars())
            val ime = insets.getInsets(WindowInsets.Type.ime())
            top = bars.top
            bottom = maxOf(bars.bottom, ime.bottom)
        } else {
            @Suppress("DEPRECATION")
            top = insets.systemWindowInsetTop
            @Suppress("DEPRECATION")
            bottom = insets.systemWindowInsetBottom
        }
        view.setPadding(view.paddingLeft, top, view.paddingRight, bottom)
        insets
    }
    requestApplyInsets()
}

/**
 * Keeps a control pinned to an edge clear of the horizontal system bars and any display
 * cutout.
 *
 * [applySystemWindowPadding] handles top and bottom for the whole screen, which is what
 * a full-width column needs. A floating child pinned to a side needs the side inset too,
 * or in landscape it lands under the gesture bar or behind a cutout.
 */
fun View.applyHorizontalSystemWindowMargin(baseMarginPx: Int) {
    setOnApplyWindowInsetsListener { view, insets ->
        val left: Int
        val right: Int
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val bars = insets.getInsets(
                WindowInsets.Type.systemBars() or WindowInsets.Type.displayCutout())
            left = bars.left
            right = bars.right
        } else {
            @Suppress("DEPRECATION")
            left = insets.systemWindowInsetLeft
            @Suppress("DEPRECATION")
            right = insets.systemWindowInsetRight
        }
        val params = view.layoutParams
        if (params is android.view.ViewGroup.MarginLayoutParams) {
            params.leftMargin = baseMarginPx + left
            params.rightMargin = baseMarginPx + right
            view.layoutParams = params
        }
        insets
    }
    requestApplyInsets()
}
