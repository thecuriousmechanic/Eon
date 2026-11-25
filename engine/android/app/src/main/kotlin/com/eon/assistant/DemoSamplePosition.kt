package com.eon.assistant

import android.graphics.Rect
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView

/** Rendered position of the fixed, EON-owned sample rows. This observer cannot dispatch input. */
internal object DemoSamplePosition {
    fun read(samples: ScrollView, list: LinearLayout, rows: List<TextView>): Int? {
        val viewport = Rect()
        if (!samples.getGlobalVisibleRect(viewport) || viewport.height() != samples.height ||
            list.parent !== samples || rows.size != list.childCount) return null
        val bounds = Rect()
        val row = rows.firstOrNull {
            it.parent === list && it.isShown && it.getGlobalVisibleRect(bounds) &&
                bounds.contains(viewport.centerX(), viewport.top)
        } ?: return null
        val rowAt = IntArray(2)
        val viewportAt = IntArray(2)
        row.getLocationInWindow(rowAt)
        samples.getLocationInWindow(viewportAt)
        return (row.top + viewportAt[1] - rowAt[1]).takeIf { it == samples.scrollY }
    }
}
