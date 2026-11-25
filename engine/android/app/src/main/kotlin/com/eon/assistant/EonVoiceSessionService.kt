package com.eon.assistant

import android.app.ActivityManager
import android.app.admin.DevicePolicyManager
import android.content.ActivityNotFoundException
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.graphics.Bitmap
import android.os.Build
import android.os.Bundle
import android.os.CancellationSignal
import android.os.Handler
import android.os.Looper
import android.os.UserManager
import android.service.voice.VoiceInteractionSession
import android.service.voice.VoiceInteractionSessionService
import android.view.View
import android.view.WindowManager
import android.widget.TextView
import java.util.concurrent.Callable
import java.util.concurrent.ExecutionException
import java.util.concurrent.FutureTask
import uniffi.eon_uniffi.runProfileWorkflow

class EonVoiceSessionService : VoiceInteractionSessionService() {
    override fun onNewSession(args: Bundle?): VoiceInteractionSession = EonVoiceSession(this)
}

internal fun userInvokedVoiceHud(flags: Int): Boolean = when (flags and NO_ASSIST_CONTEXT.inv()) {
    VoiceInteractionSession.SHOW_SOURCE_ASSIST_GESTURE, VoiceInteractionSession.SHOW_SOURCE_PUSH_TO_TALK -> true
    else -> false
}

internal inline fun ownedContextAllowed(userDisabled: () -> Int, assistRestricted: () -> Boolean?,
    captureDisabled: () -> Boolean?): Boolean = try {
    userDisabled() and VoiceInteractionSession.SHOW_WITH_ASSIST == 0 &&
        assistRestricted() == false && captureDisabled() == false
} catch (_: RuntimeException) {
    false
}

private class EonVoiceSession(context: Context) : VoiceInteractionSession(context) {
    private val binding = voiceHud.binding
    private val surface = voiceHud.Surface(binding, ::closeHud)
    private val main = Handler(Looper.getMainLooper())
    private var hud: VoiceHudView? = null
    private var work: FutureTask<ProfileCheck.Complete>? = null
    private var toneStop: CancellationSignal? = null
    private var taskStop: CancellationSignal? = null
    private var microphone: SessionMic? = null
    private var contextDisplay: ControlsConsent.Ticket? = null
    private var contextNotice: String? = null
    private var firstCandidate = ProfileActionReview.candidate()
    private var actionReview: ProfileActionReview? = null
    private var assistId: ActivityId? = null
    private var sourceId: ActivityId? = null
    private var actionStop: CancellationSignal? = null
    private val contextGuard = object : Runnable {
        override fun run() {
            val receipt = contextDisplay ?: return
            if (!requireVisible()) return
            if (!contextSharingEnabled() || !ControlsAssist.mayDisplay(receipt)) return unavailableContext()
            main.postDelayed(this, 300)
        }
    }
    private var micLevels = emptyList<Int>()
    private val micGuard = object : Runnable {
        override fun run() {
            if (microphone == null) return
            if (!micEligible()) clearMic(R.string.voice_mic_failed)
            else main.postDelayed(this, 100)
        }
    }
    private var observingScreenOff = false
    private val screenOff = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (intent.action == Intent.ACTION_SCREEN_OFF) closeHud()
        }
    }

    private fun eligible(): Boolean {
        return try {
            selectedAndUnlocked(context) &&
                disabledShowContext and NO_ASSIST_CONTEXT == NO_ASSIST_CONTEXT
        } catch (_: SecurityException) {
            false
        }
    }

    private fun contextSharingEnabled() = ownedContextAllowed(
        { userDisabledShowContext }, // The system's user switches do not include admin policy.
        {
            if (Build.VERSION.SDK_INT < 35) false else context.getSystemService(UserManager::class.java)?.let {
                // The SDK has no ordinary isPrivateProfile() query. Refuse unclassified profiles
                // rather than request hidden-profile access or inspect other users or windows.
                it.hasUserRestriction(UserManager.DISALLOW_ASSIST_CONTENT) || (it.isProfile && !it.isManagedProfile)
            }
        },
        { context.getSystemService(DevicePolicyManager::class.java)?.getScreenCaptureDisabled(null) }
    )

    override fun onPrepareShow(args: Bundle?, showFlags: Int) {
        clearShowing()
        val eligible = eligible()
        val review = ProfileActionReview.take(firstCandidate, showFlags, args)
        firstCandidate = null
        val ticket = if (eligible) ControlsAssist.takeShow(args, showFlags) else null
        val allowed = surface.show(review != null || ticket != null || userInvokedVoiceHud(showFlags), eligible)
        if (allowed && review != null) {
            actionReview = review
            review.endShowing = { finishSession ->
                if (actionReview === review) {
                    if (finishSession) closeHud() else {
                        clearShowing()
                        setUiEnabled(false)
                    }
                }
            }
        }
        if (allowed && ticket != null) {
            contextDisplay = ticket
            if (contextSharingEnabled()) {
                contextNotice = context.getString(if (ticket.timing) R.string.voice_context_timed else R.string.voice_context_untimed)
            } else unavailableContext()
        } else ControlsAssist.cancel(ticket)
        setUiEnabled(allowed && actionReview == null)
    }

    override fun onShow(args: Bundle?, showFlags: Int) {
        if (showFlags == SHOW_SOURCE_ACTIVITY && !surface.canUse(eligible())) {
            clearShowing()
            setUiEnabled(false)
            // doShow sends the OS local-start acknowledgement only after onShow returns.
            // Leave this rejected show headless; release it only if it wasn't replaced.
            val id = if (Build.VERSION.SDK_INT >= 34) ProfileActionReview.showId(args) else null
            main.postDelayed({
                if (binding != null && voiceHud.binding === binding &&
                    (Build.VERSION.SDK_INT < 34 || (id != null && id == binding.latestLocalShowId))) closeHud()
            }, ProfileActionReview.TIMEOUT)
            return
        }
        if (!requireVisible()) return
        FloatingHudService.stopFrom(context)
        InspectionAccess.cancel()
        try {
            // SCREEN_OFF is a protected system broadcast. Observe only while this card is shown.
            context.applicationContext.registerReceiver(screenOff, IntentFilter(Intent.ACTION_SCREEN_OFF))
            observingScreenOff = true
        } catch (_: SecurityException) {
            closeHud()
            return
        }
        if (requireVisible()) {
            render()
            if (contextDisplay != null) main.post(contextGuard)
            actionReview?.let { review ->
                main.postDelayed({
                    if (actionReview === review && !review.presenting) closeHud()
                }, ProfileActionReview.TIMEOUT)
            }
        }
    }

    // A local show is scoped by Android to the starting activity token. Do not open any
    // AssistData, AssistStructure, AssistContent, screenshot or visible-activity list.
    override fun onHandleAssist(state: AssistState) {
        val r = actionReview ?: return
        if (!authorized(r) || !state.isFocused || state.count != 1 || assistId != null) return
        assistId = state.activityId
        discover(r)
    }
    override fun onHandleScreenshot(screenshot: Bitmap?) = Unit

    override fun onDirectActionsInvalidated(activityId: ActivityId) {
        val r = actionReview ?: return
        if (!r.sameInteractor() || !r.valid() || r.presenting || sourceId != null) return
        // Wait for the owner's local interactor before transferring focus, even for denial.
        if (!contextSharingEnabled()) { presentAction(r, null); return }
        if (!authorized(r)) return
        sourceId = activityId
        discover(r)
    }

    private fun authorized(r: ProfileActionReview) = actionReview === r && !r.presenting && r.valid() &&
        surface.canUse(eligible()) && contextSharingEnabled()

    private fun discover(r: ProfileActionReview) {
        val id = assistId ?: return
        if (id != sourceId || !authorized(r) || !r.sameInteractor() || r.discovering) return
        r.discovering = true
        r.permitted = { authorized(r) }
        val stop = CancellationSignal().also { actionStop = it }
        try {
            requestDirectActions(id, stop, context.mainExecutor) { actions ->
                if (!authorized(r) || !r.sameInteractor()) return@requestDirectActions
                val action = if (r.offered) actions.singleOrNull() else null
                if (action?.id != ProfileActionReview.ACTION) return@requestDirectActions presentAction(r, null)
                val permit = r.beginPerform() ?: return@requestDirectActions
                try {
                    performDirectAction(action, permit, stop, context.mainExecutor) { result ->
                        if (authorized(r) && r.sameInteractor())
                            presentAction(r, if (r.completed) result?.getString(ProfileActionReview.RESULT) else null)
                    }
                } catch (_: RuntimeException) { presentAction(r, null) }
            }
        } catch (_: RuntimeException) { presentAction(r, null) }
    }

    private fun presentAction(r: ProfileActionReview, profile: String?) {
        if (actionReview !== r || !requireVisible() || r.presenting) return
        if (!r.valid()) { closeHud(); return }
        r.presenting = true
        r.permitted = null
        contextNotice = if (profile != null && contextSharingEnabled())
            context.getString(R.string.profile_action_result, profile)
            else context.getString(R.string.profile_action_unavailable)
        setUiEnabled(true)
        render()
        main.postDelayed({ if (actionReview === r) closeHud() }, 30_000)
        if (profile != null) main.post(actionResultGuard)
    }

    private val actionResultGuard = object : Runnable {
        override fun run() {
            val r = actionReview ?: return
            if (!r.ownsPage() || !r.sameInteractor() || !contextSharingEnabled() || !eligible()) closeHud()
            else main.postDelayed(this, 300)
        }
    }

    private fun discardContext() {
        ControlsAssist.cancel(contextDisplay)
        contextDisplay = null
        contextNotice = null
        main.removeCallbacks(contextGuard)
    }

    private fun unavailableContext() {
        discardContext()
        contextNotice = context.getString(R.string.voice_context_unavailable)
        render()
    }

    override fun onCreateContentView(): View {
        window?.window?.apply {
            addFlags(WindowManager.LayoutParams.FLAG_SECURE or WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL)
            clearFlags(WindowManager.LayoutParams.FLAG_DIM_BEHIND)
            setBackgroundDrawableResource(android.R.color.transparent)
            setHideOverlayWindows(true)
        }
        return VoiceHudView(context, ::toggleDetails, ::checkProfile, ::sampleTasks, ::analyzeTone, ::clearTone,
            ::openControls, ::closeHud).also {
            hud = it
            it.setMicClick(::toggleMic)
            it.onMicWindowLost = { clearMic() }
            render()
        }
    }

    override fun onComputeInsets(outInsets: Insets) {
        // Leave the app underneath at its original size; only the floating card takes touches.
        outInsets.contentInsets.set(0, hud?.rootView?.height ?: 0, 0, 0)
        outInsets.touchableInsets = Insets.TOUCHABLE_INSETS_REGION
        outInsets.touchableRegion.setEmpty()
        if (surface.frame != null) hud?.fillTouchableRegion(outInsets.touchableRegion)
    }

    private fun render() {
        hud?.render(surface.frame)
        hud?.findViewById<TextView>(R.id.hud_context)?.apply {
            text = contextNotice
            visibility = if (contextNotice == null) View.GONE else View.VISIBLE
        }
    }

    private fun requireVisible(): Boolean {
        if (surface.canUse(eligible())) return true
        closeHud()
        return false
    }

    private fun toggleDetails() {
        if (!requireVisible()) return
        if (surface.frame?.expanded == true) {
            clearMic()
            clearTone()
            cancelTaskSample()
        }
        surface.toggleDetails()
        render()
    }

    private fun micEligible() = surface.frame?.expanded == true && hud?.micVisible() == true &&
        surface.canUse(eligible()) && MicPermission.allowed(context)

    private fun toggleMic() {
        if (microphone != null) {
            clearMic(R.string.voice_mic_stopped)
            return
        }
        if (!requireVisible() || surface.frame?.expanded != true || hud?.micVisible() != true) return
        if (!MicPermission.allowed(context)) {
            clearMic(R.string.voice_mic_denied)
            return
        }
        val take = try { SessionMic.open() } catch (_: RuntimeException) {
            clearMic(R.string.voice_mic_failed)
            return
        }
        microphone = take
        micLevels = emptyList()
        hud?.renderMic(true, micLevels, context.getString(R.string.voice_mic_starting))
        main.post(micGuard)
        val app = context.applicationContext
        Thread({
            val ok = take.measure(app) { level ->
                main.post {
                    if (microphone !== take) return@post
                    if (!micEligible()) clearMic(R.string.voice_mic_failed)
                    else {
                        micLevels = micLevels + level
                        hud?.renderMic(true, micLevels, context.getString(R.string.voice_mic_progress, micLevels.size, level))
                    }
                }
            }
            main.post {
                if (microphone === take) {
                    if (ok && micLevels.size == ToneLevels.LIMIT && micEligible()) {
                        microphone = null // worker already released AudioRecord
                        main.removeCallbacks(micGuard)
                        hud?.renderMic(false, micLevels, context.getString(R.string.voice_mic_complete, micLevels.max()))
                    } else clearMic(R.string.voice_mic_failed)
                }
            }
        }, "EON visible microphone meter").start()
    }

    private fun clearMic(message: Int? = null) {
        val old = microphone
        microphone = null
        old?.close()
        main.removeCallbacks(micGuard)
        micLevels = emptyList()
        hud?.renderMic(false, micLevels, message?.let(context::getString))
    }

    private fun sampleTasks() {
        if (!requireVisible()) return
        val request = surface.requestTasks() ?: return
        val stop = CancellationSignal().also { taskStop = it }
        render()
        val app = context.applicationContext
        Thread({
            val counts = try { OwnActivityTasks.read(app, stop) } catch (_: RuntimeException) { null }
            main.post {
                if (taskStop === stop && requireVisible()) {
                    taskStop = null
                    surface.finishTasks(request, counts)
                    render()
                }
            }
        }, "EON activity tasks").start()
    }

    private fun cancelTaskSample() {
        taskStop?.cancel()
        taskStop = null
    }

    private fun analyzeTone() {
        if (!requireVisible()) return
        val request = surface.requestTone() ?: return
        val stop = CancellationSignal()
        toneStop = stop
        render()
        val app = context.applicationContext
        Thread({
            val ok = try {
                StockToneReader.read(app, stop) { level ->
                    main.post {
                        if (toneStop === stop && requireVisible()) {
                            surface.addLevel(request, level)
                            render()
                        }
                    }
                }
                true
            } catch (_: Exception) {
                false
            }
            main.post {
                if (toneStop === stop && requireVisible()) {
                    toneStop = null
                    surface.finishTone(request, ok)
                    render()
                }
            }
        }, "EON stock tone").start()
    }

    private fun clearTone() {
        val old = toneStop
        toneStop = null
        old?.cancel()
        surface.clearTone()
        render()
    }

    private fun checkProfile() {
        if (!requireVisible()) return
        val request = surface.requestCheck() ?: return
        render()
        val app = context.applicationContext
        val task = object : FutureTask<ProfileCheck.Complete>(Callable {
            val memory = ActivityManager.MemoryInfo()
            app.getSystemService(ActivityManager::class.java).getMemoryInfo(memory)
            val receipt = runProfileWorkflow(memory.totalMem / 1_000_000_000.0)
            ProfileCheck.Complete(receipt.profile, receipt.completedTasks)
        }) {
            override fun done() {
                if (isCancelled) return
                main.post {
                    if (surface.frame?.check !== request) return@post
                    work = null
                    if (!requireVisible()) return@post
                    // done() has already run: get() cannot block on the native call here.
                    val completion = try { get() } catch (_: ExecutionException) { null }
                    surface.acceptResult(request, completion)
                    render()
                }
            }
        }
        work = task
        Thread(task, "EON profile check").start()
    }

    private fun openControls() {
        try {
            if (surface.frame?.expanded == true && surface.consumeAction(eligible())) {
                startAssistantActivity(Intent(context, AssistantActivity::class.java))
            }
        } catch (_: SecurityException) {
            // A role revocation during this call is not a reason to retry.
        } catch (_: ActivityNotFoundException) {
            // The launcher activity may have been disabled in the meantime.
        } finally {
            closeHud()
        }
    }

    private fun clearShowing() {
        actionReview?.let { it.source.release(it) }
        actionReview = null
        actionStop?.cancel()
        actionStop = null
        assistId = null
        sourceId = null
        discardContext()
        clearMic()
        surface.hide()
        cancelTaskSample()
        clearTone()
        work?.cancel(true)
        work = null
        main.removeCallbacksAndMessages(null)
        if (observingScreenOff) {
            context.applicationContext.unregisterReceiver(screenOff)
            observingScreenOff = false
        }
        render()
    }

    private fun closeHud() {
        surface.retire()
        clearShowing()
        setUiEnabled(false)
        try {
            finish()
        } catch (_: SecurityException) {
            // A revoked binding has no authority to finish a replacement session.
        }
    }

    override fun onHide() {
        clearShowing()
        super.onHide()
    }

    override fun onBackPressed() = closeHud()
    override fun onLockscreenShown() = closeHud()
    override fun onCloseSystemDialogs() = closeHud()

    override fun onDestroy() {
        surface.retire()
        clearShowing()
        hud = null
        super.onDestroy()
    }
}
