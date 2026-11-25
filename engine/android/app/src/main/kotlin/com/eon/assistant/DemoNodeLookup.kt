package com.eon.assistant

import android.view.View
import android.view.ViewGroup
import android.view.accessibility.AccessibilityNodeInfo

/** API 34+ local connection: only the two immediate accessibility children of EON's sample group. */
internal object DemoNodeLookup {
    fun findView(group: ViewGroup, action: DemoAction): View? = find(group, action)?.first

    fun find(group: ViewGroup, action: DemoAction): Pair<View, AccessibilityNodeInfo>? {
        if (!group.isAttachedToWindow || !group.isShown || group.childCount != 2) return null
        val root = group.createAccessibilityNodeInfo() ?: return null
        if (root.childCount != 2) return null
        val pkg = group.context.packageName
        var result: Pair<View, AccessibilityNodeInfo>? = null
        val seen = mutableSetOf<Int>()
        try {
            root.setQueryFromAppProcessEnabled(group, true)
            for (i in 0 until root.childCount) {
                val node = root.getChild(i, 0) ?: return null // no descendant prefetch
                if (node.windowId != root.windowId || node.packageName != pkg) return null
                val id = when (node.viewIdResourceName) {
                    "$pkg:id/demo_counter" -> R.id.demo_counter.takeIf { node.className == "android.widget.Button" }
                    "$pkg:id/demo_samples" -> R.id.demo_samples.takeIf { node.className == "android.widget.ScrollView" }
                    else -> null
                } ?: return null
                if (!seen.add(id)) return null
                if (id == if (action == DemoAction.CLICK_COUNTER) R.id.demo_counter else R.id.demo_samples) {
                    val view = group.findViewById<View>(id) ?: return null
                    if (view.parent !== group || view.windowToken !== group.windowToken ||
                        node != view.createAccessibilityNodeInfo()) return null
                    result = view to node
                }
            }
        } finally {
            root.setQueryFromAppProcessEnabled(group, false)
        }
        return result
    }
}
