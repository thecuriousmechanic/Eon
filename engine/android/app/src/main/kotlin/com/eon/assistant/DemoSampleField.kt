package com.eon.assistant

import android.content.Context
import android.os.Bundle
import android.text.InputFilter
import android.view.View
import android.view.ViewGroup
import android.view.accessibility.AccessibilityNodeInfo
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.widget.EditText

/** A real EditText for locating, not data entry. No draft, IME session or text action. */
internal class DemoSampleField(context: Context) : EditText(context) {
    init {
        id = R.id.demo_field
        isSaveEnabled = false
        isSaveFromParentEnabled = false
        importantForAccessibility = IMPORTANT_FOR_ACCESSIBILITY_YES
        importantForAutofill = IMPORTANT_FOR_AUTOFILL_NO_EXCLUDE_DESCENDANTS
        importantForContentCapture = IMPORTANT_FOR_CONTENT_CAPTURE_NO_EXCLUDE_DESCENDANTS
        filters = arrayOf(InputFilter.LengthFilter(0))
        showSoftInputOnFocus = false
        isFocusable = false
        isClickable = false
        isLongClickable = false
    }

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? = null

    override fun onInitializeAccessibilityNodeInfo(info: AccessibilityNodeInfo) {
        super.onInitializeAccessibilityNodeInfo(info)
        info.text = null
        info.hintText = null
        info.actionList.toList().filterNot { focusAction(it.id) }.forEach { info.removeAction(it) }
    }

    private fun focusAction(action: Int) = action == AccessibilityNodeInfo.ACTION_ACCESSIBILITY_FOCUS ||
        action == AccessibilityNodeInfo.ACTION_CLEAR_ACCESSIBILITY_FOCUS

    override fun performAccessibilityAction(action: Int, arguments: Bundle?): Boolean =
        focusAction(action) && super.performAccessibilityAction(action, arguments)
}

/** Returns only presence. The group, label and field are retained by one private activity. */
internal class DemoFieldLookup(private val group: ViewGroup, private val label: View, private val field: DemoSampleField) {
    fun present(): Boolean {
        if (!group.isAttachedToWindow || !group.isShown || group.childCount != 2 ||
            label.parent !== group || field.parent !== group || label.isAccessibilityDataSensitive ||
            field.isAccessibilityDataSensitive || !field.isEnabled || !field.isShown ||
            field.windowToken !== group.windowToken || field.id != R.id.demo_field || label.id != R.id.demo_field_label) return false
        val root = group.createAccessibilityNodeInfo() ?: return false
        if (root.childCount != 2) return false
        val pkg = group.context.packageName
        var foundField = false
        var foundLabel = false
        try {
            root.setQueryFromAppProcessEnabled(group, true)
            for (i in 0 until 2) {
                val node = root.getChild(i, 0) ?: return false
                if (node.windowId != root.windowId || node.packageName != pkg || node.isAccessibilityDataSensitive ||
                    !node.isVisibleToUser || node.isPassword) return false
                when (node.viewIdResourceName) {
                    "$pkg:id/demo_field" -> {
                        if (foundField || !node.isEnabled || node.className != "android.widget.EditText" ||
                            node != field.createAccessibilityNodeInfo()) return false
                        foundField = true
                    }
                    "$pkg:id/demo_field_label" -> {
                        if (foundLabel || node.className != "android.widget.TextView" ||
                            node != label.createAccessibilityNodeInfo()) return false
                        foundLabel = true
                    }
                    else -> return false
                }
            }
        } finally {
            root.setQueryFromAppProcessEnabled(group, false)
        }
        return foundField && foundLabel
    }
}
