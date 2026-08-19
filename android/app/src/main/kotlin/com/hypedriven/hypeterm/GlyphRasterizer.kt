package com.hypedriven.hypeterm

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Rect
import android.graphics.Typeface
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Rasterizes glyph clusters with the platform text stack (spec §10.2).
 *
 * Using `android.graphics` rather than bundling a shaper is what gives the terminal
 * system font fallback, correct combining-mark placement and scripts the app has no
 * font for. Cell width stays authoritative: a fallback glyph that is not monospaced
 * is scaled down to fit rather than allowed to overflow its cell.
 *
 * Called only from the native render thread, one glyph at a time, inside a bounded
 * per-frame budget.
 */
class GlyphRasterizer(private val typeface: Typeface = Typeface.MONOSPACE) {

    private val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.WHITE
        style = Paint.Style.FILL
        isSubpixelText = true
    }
    private val bounds = Rect()

    /**
     * Returns a packed bitmap: four big-endian ints (width, height, left, top) followed
     * by 8-bit coverage, or null when there is nothing to draw.
     */
    fun rasterize(
        cluster: String,
        bold: Boolean,
        italic: Boolean,
        cellWidth: Int,
        fontSizePx: Float,
        cellWidthPx: Float,
        cellHeightPx: Float,
        baselinePx: Float,
    ): ByteArray? {
        if (cluster.isEmpty() || cluster == " ") return null

        val style = when {
            bold && italic -> Typeface.BOLD_ITALIC
            bold -> Typeface.BOLD
            italic -> Typeface.ITALIC
            else -> Typeface.NORMAL
        }
        paint.typeface = Typeface.create(typeface, style)
        paint.textSize = fontSizePx

        val advanceLimit = cellWidthPx * cellWidth
        val measured = paint.measureText(cluster)
        // A fallback glyph wider than its cells is condensed rather than allowed to
        // bleed into the neighbouring cell.
        paint.textScaleX = if (measured > advanceLimit && measured > 0f) {
            advanceLimit / measured
        } else {
            1.0f
        }

        paint.getTextBounds(cluster, 0, cluster.length, bounds)
        val padding = 1
        val width = (maxOf(bounds.width(), 1) + padding * 2)
            .coerceAtMost((advanceLimit * 2).toInt().coerceAtLeast(1))
        val height = (maxOf(bounds.height(), 1) + padding * 2)
            .coerceAtMost((cellHeightPx * 2).toInt().coerceAtLeast(1))
        if (width <= 0 || height <= 0) return null

        val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ALPHA_8)
        val canvas = Canvas(bitmap)
        // Draw at the cluster's own origin so the bitmap is tight around the ink.
        canvas.drawText(cluster, (-bounds.left + padding).toFloat(),
            (-bounds.top + padding).toFloat(), paint)

        // ALPHA_8 rows are padded to an alignment, so `rowBytes` can exceed `width`.
        // Copy the padded form out and repack it tightly, because the atlas expects
        // exactly width*height bytes.
        val rowBytes = bitmap.rowBytes
        val padded = ByteArray(rowBytes * height)
        bitmap.copyPixelsToBuffer(ByteBuffer.wrap(padded))
        bitmap.recycle()

        val alpha = if (rowBytes == width) {
            padded
        } else {
            ByteArray(width * height).also { tight ->
                for (row in 0 until height) {
                    System.arraycopy(padded, row * rowBytes, tight, row * width, width)
                }
            }
        }

        val header = ByteBuffer.allocate(HEADER_BYTES).order(ByteOrder.BIG_ENDIAN)
        header.putInt(width)
        header.putInt(height)
        // Offsets are relative to the cell origin: x from the left edge, y from the top.
        header.putInt(bounds.left - padding)
        header.putInt(baselinePx.toInt() + bounds.top - padding)
        return header.array() + alpha
    }

    /**
     * Returns [cellWidth, cellHeight, baseline, underlineThickness, underlinePosition]
     * in device pixels for a font size.
     */
    fun measure(fontSizePx: Float, density: Float): FloatArray {
        paint.typeface = typeface
        paint.textSize = fontSizePx
        paint.textScaleX = 1.0f

        val metrics = paint.fontMetrics
        // The advance of a representative character defines the cell: for a monospace
        // face every character has the same one.
        val advance = paint.measureText("M")
        val cellWidth = maxOf(1f, kotlin.math.ceil(advance))
        val cellHeight = maxOf(1f, kotlin.math.ceil(metrics.descent - metrics.ascent +
            metrics.leading))
        val baseline = kotlin.math.ceil(-metrics.ascent)
        val underlineThickness = maxOf(1f, kotlin.math.floor(fontSizePx / 14f))
        val underlinePosition = maxOf(1f, kotlin.math.floor(cellHeight * 0.08f))
        return floatArrayOf(cellWidth, cellHeight, baseline, underlineThickness,
            underlinePosition)
    }

    private companion object {
        const val HEADER_BYTES = 16
    }
}
