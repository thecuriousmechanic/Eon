package com.eon.assistant

import android.accessibilityservice.AccessibilityServiceInfo
import android.app.KeyguardManager
import android.app.role.RoleManager
import android.content.ComponentName
import android.content.Context
import android.os.PowerManager
import android.provider.Settings
import android.view.accessibility.AccessibilityManager

/** The private demo needs an assigned shortcut, not the visibility of SystemUI's navigation bar. */
internal object DemoShortcut {
    // @Readable in AOSP Settings.Secure, including for a floating accessibility button. No
    // privileged API or write. On a platform that does not expose it, the demo stays disabled.
    const val SETTING = "accessibility_button_targets"

    fun assigned(value: String?, full: String, short: String): Boolean =
        value != null && value.length <= 8192 && value.splitToSequence(':').any { it == full || it == short }

    fun eligible(context: Context): Boolean = try {
        val role = context.getSystemService(RoleManager::class.java)
        val self = ComponentName(context, FocusedControlService::class.java)
        context.getSystemService(PowerManager::class.java)?.isInteractive == true &&
            context.getSystemService(KeyguardManager::class.java)?.isKeyguardLocked == false &&
            role?.isRoleAvailable(RoleManager.ROLE_ASSISTANT) == true && role.isRoleHeld(RoleManager.ROLE_ASSISTANT) &&
            context.getSystemService(AccessibilityManager::class.java)
                ?.getEnabledAccessibilityServiceList(AccessibilityServiceInfo.FEEDBACK_ALL_MASK)
                ?.any { ComponentName.unflattenFromString(it.id) == self &&
                    it.flags and AccessibilityServiceInfo.FLAG_REQUEST_ACCESSIBILITY_BUTTON != 0 } == true &&
            assigned(Settings.Secure.getString(context.contentResolver, SETTING), self.flattenToString(), self.flattenToShortString())
    } catch (_: RuntimeException) {
        false
    }
}
