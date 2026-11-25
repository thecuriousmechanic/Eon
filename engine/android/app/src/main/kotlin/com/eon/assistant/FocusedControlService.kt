package com.eon.assistant

import android.accessibilityservice.AccessibilityButtonController
import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.AccessibilityServiceInfo
import android.app.KeyguardManager
import android.app.role.RoleManager
import android.content.BroadcastReceiver
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.os.SystemClock
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityManager
import android.view.accessibility.AccessibilityNodeInfo
import java.util.concurrent.Executors

internal object InspectionAccess {
    private val handler = Handler(Looper.getMainLooper())
    private val consent = VerifiedInspectionConsent(
        SystemClock::elapsedRealtime, Executors.newSingleThreadExecutor(), { handler.post(it) },
    ) { InspectionCredential.issue()?.use { it.redeem() } == true }
    private val revoke = Runnable { cancel() }
    var shortcutAvailable = false
        private set
    // An availability interruption must invalidate even an in-app demo review made before it.
    var environmentEpoch = Any()
        private set

    fun interrupt() {
        environmentEpoch = Any()
        cancel()
    }

    fun setShortcut(available: Boolean) {
        interrupt()
        shortcutAvailable = available
    }

    fun cancel() {
        handler.removeCallbacks(revoke)
        consent.revoke()
    }

    fun cancelPendingConfirmation() = consent.cancelPending()

    fun confirm(eligible: () -> Boolean, ready: (Boolean) -> Unit) {
        cancel()
        consent.confirm(eligible) { allowed ->
            if (allowed) clearAfterTimeout()
            ready(allowed)
        }
    }

    fun inspect(eligible: () -> Boolean, readFocused: () -> ControlKind, deliver: () -> Unit) {
        if (consent.inspect(eligible, readFocused)) {
            clearAfterTimeout()
            deliver()
        }
    }

    fun takeResult(eligible: Boolean): ControlKind? = consent.takeResult(eligible)

    private fun clearAfterTimeout() {
        handler.removeCallbacks(revoke)
        handler.postDelayed(revoke, OneShotInspection.LIFETIME_MS)
    }

    fun eligible(context: Context): Boolean {
        if (Build.VERSION.SDK_INT < 34 || !shortcutAvailable) return false
        return try {
            val role = context.getSystemService(RoleManager::class.java)
            val keyguard = context.getSystemService(KeyguardManager::class.java)
            val power = context.getSystemService(PowerManager::class.java)
            val manager = context.getSystemService(AccessibilityManager::class.java)
            val self = ComponentName(context, FocusedControlService::class.java)
            power?.isInteractive == true && keyguard?.isKeyguardLocked == false &&
                role?.isRoleAvailable(RoleManager.ROLE_ASSISTANT) == true && role.isRoleHeld(RoleManager.ROLE_ASSISTANT) &&
                manager?.getEnabledAccessibilityServiceList(AccessibilityServiceInfo.FEEDBACK_ALL_MASK)
                    ?.any { ComponentName.unflattenFromString(it.id) == self } == true
        } catch (_: RuntimeException) {
            false
        }
    }
}

class FocusedControlService : AccessibilityService() {
    private var listening = false
    private val screenOff = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            InspectionAccess.interrupt()
        }
    }
    private val shortcut = object : AccessibilityButtonController.AccessibilityButtonCallback() {
        override fun onAvailabilityChanged(controller: AccessibilityButtonController, available: Boolean) {
            InspectionAccess.setShortcut(available)
        }

        override fun onClicked(controller: AccessibilityButtonController) {
            if (!listening || Build.VERSION.SDK_INT < 34) return
            try {
                InspectionAccess.inspect(
                    eligible = { listening && controller.isAccessibilityButtonAvailable && InspectionAccess.eligible(this@FocusedControlService) },
                    readFocused = {
                        // One accessibility-focused node, not input focus, the active root or window list.
                        val n = findFocus(AccessibilityNodeInfo.FOCUS_ACCESSIBILITY)
                        if (n == null) ControlKind.UNAVAILABLE else ControlKind.classify(
                            n.isVisibleToUser, n.isAccessibilityFocused, n.isAccessibilityDataSensitive,
                            n.isPassword, n.isEditable, n.inputType,
                        ) { n.className }
                    },
                ) {
                    startActivity(Intent(this@FocusedControlService, InspectionResultActivity::class.java)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
                }
            } catch (_: RuntimeException) {
                // Disconnection, a lost node, or a blocked foreground launch cannot reopen a grant.
                InspectionAccess.cancel()
            }
        }
    }

    override fun onServiceConnected() {
        super.onServiceConnected()
        disconnect()
        if (Build.VERSION.SDK_INT < 34 || !setCacheEnabled(false)) {
            disableSelf()
            return
        }
        registerReceiver(screenOff, IntentFilter(Intent.ACTION_SCREEN_OFF), Context.RECEIVER_NOT_EXPORTED)
        accessibilityButtonController.registerAccessibilityButtonCallback(shortcut)
        listening = true
        InspectionAccess.setShortcut(accessibilityButtonController.isAccessibilityButtonAvailable)
    }

    // No event types are requested. Never open an event source, even if a platform sends one.
    override fun onAccessibilityEvent(event: AccessibilityEvent?) = Unit
    override fun onInterrupt() = InspectionAccess.interrupt()

    private fun disconnect() {
        InspectionAccess.setShortcut(false)
        if (listening) {
            listening = false
            accessibilityButtonController.unregisterAccessibilityButtonCallback(shortcut)
            unregisterReceiver(screenOff)
        }
    }

    override fun onUnbind(intent: Intent?): Boolean {
        disconnect()
        return super.onUnbind(intent)
    }

    override fun onDestroy() {
        disconnect()
        super.onDestroy()
    }
}
