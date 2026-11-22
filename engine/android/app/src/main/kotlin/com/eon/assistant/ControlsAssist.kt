package com.eon.assistant

import android.app.KeyguardManager
import android.app.role.RoleManager
import android.content.ComponentName
import android.content.Context
import android.os.Binder
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.os.SystemClock
import android.service.voice.VoiceInteractionService

/** Binds the exact volunteered value to one capability and one foreground controls instance. */
internal class ControlsConsent(private val clock: () -> Long) {
    class Ticket(val source: Any, val token: Any, val timing: Boolean, val until: Long)
    private var current: Ticket? = null
    fun issue(source: Any, token: Any, timing: Boolean) =
        Ticket(source, token, timing, clock() + 5_000).also { current = it }
    fun pending() = current?.takeIf { clock() < it.until }
    fun take(token: Any?): Ticket? {
        val ticket = pending()
        current = null
        return ticket?.takeIf { it.token === token }
    }
    fun cancel(ticket: Ticket?) { if (current === ticket) current = null }
    fun reset() { current = null }
}

internal fun selectedAndUnlocked(context: Context): Boolean = try {
    val role = context.getSystemService(RoleManager::class.java)
    context.getSystemService(PowerManager::class.java)?.isInteractive == true &&
        context.getSystemService(KeyguardManager::class.java)?.isKeyguardLocked == false &&
        role?.isRoleAvailable(RoleManager.ROLE_ASSISTANT) == true && role.isRoleHeld(RoleManager.ROLE_ASSISTANT) &&
        VoiceInteractionService.isActiveService(context, ComponentName(context, EonVoiceInteractionService::class.java))
} catch (_: SecurityException) {
    false
}

/** App-authored metadata, not AssistStructure or AssistContent. The two collection bits stay
 * disabled for the lifetime of the binding, including during simultaneous foreign invocations. */
internal object ControlsAssist {
    private const val KEY = "com.eon.assistant.REVIEW_CAPABILITY"
    private val consent = ControlsConsent(SystemClock::elapsedRealtime)
    private val main = Handler(Looper.getMainLooper())
    private var service: EonVoiceInteractionService? = null
    private var displayed: ControlsConsent.Ticket? = null
    internal val bindingReady get() = service != null

    fun ready(owner: EonVoiceInteractionService) {
        consent.reset()
        displayed = null
        service = owner
    }
    fun release(owner: EonVoiceInteractionService) {
        if (service !== owner) return
        consent.reset()
        displayed = null
        service = null
    }

    fun request(activity: AssistantActivity, source: Any, timing: Boolean): Boolean {
        val owner = service ?: return false
        if (!activity.hasWindowFocus() || activity.isInMultiWindowMode || activity.isInPictureInPictureMode ||
            !selectedAndUnlocked(activity)) return false
        val token = Binder()
        val ticket = consent.issue(source, token, timing)
        return try {
            check(owner.disabledShowContext and NO_ASSIST_CONTEXT == NO_ASSIST_CONTEXT)
            // Pass only a capability. The reviewed bit stays in process; the session checks
            // the user and admin policy before reading it. Neither context flag is requested.
            owner.showSession(Bundle().apply { putBinder(KEY, token) }, 0)
            main.postDelayed({ consent.cancel(ticket) }, 5_000)
            true
        } catch (_: RuntimeException) {
            consent.cancel(ticket)
            false
        }
    }

    fun cancelSource(source: Any) {
        consent.cancel(consent.pending()?.takeIf { it.source === source })
        if (displayed?.source === source) displayed = null
    }
    fun cancel(ticket: ControlsConsent.Ticket?) {
        consent.cancel(ticket)
        if (displayed === ticket) displayed = null
    }
    fun mayDisplay(ticket: ControlsConsent.Ticket) =
        displayed === ticket && SystemClock.elapsedRealtime() < ticket.until + 25_000

    fun takeShow(args: Bundle?, flags: Int): ControlsConsent.Ticket? {
        if (consent.pending() == null) return null
        if (flags != 0 || args == null) {
            consent.reset()
            return null // Never unpack ordinary app, gesture or data-bearing show arguments.
        }
        return try {
            consent.take(args.getBinder(KEY))?.also { displayed = it }
        } catch (_: RuntimeException) {
            consent.reset()
            null
        }
    }
}
