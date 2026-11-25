package com.eon.assistant

import android.app.Activity
import android.app.ActivityManager
import android.app.DirectAction
import android.app.VoiceInteractor
import android.os.Binder
import android.os.Build
import android.os.Bundle
import android.os.CancellationSignal
import android.os.SystemClock
import android.service.voice.VoiceInteractionSession
import android.view.View
import android.view.WindowManager
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import java.util.function.Consumer
import uniffi.eon_uniffi.selectModelProfile

/** Private, non-editable source of one read-only DirectAction. No intent-supplied state. */
class ProfileActionActivity : Activity() {
    private lateinit var outcome: TextView
    private var resumed = false
    private var awaitingStart = false
    internal var review: ProfileActionReview? = null
        private set

    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        window.setHideOverlayWindows(true)
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            fitsSystemWindows = true
            importantForAutofill = View.IMPORTANT_FOR_AUTOFILL_NO_EXCLUDE_DESCENDANTS
            addView(TextView(context).apply {
                id = R.id.profile_action_status
                setText(R.string.profile_action_review)
            })
            outcome = TextView(context).apply {
                id = R.id.profile_action_outcome
                isSaveEnabled = false
                visibility = View.GONE
            }.also(::addView)
            addView(Button(context).apply {
                id = R.id.profile_action_confirm
                setText(R.string.profile_action_confirm)
                filterTouchesWhenObscured = true
                setOnClickListener { confirm() }
            })
        })
    }

    internal fun activePage() = resumed && !isFinishing && !isDestroyed && !isInMultiWindowMode &&
        !isInPictureInPictureMode && selectedAndUnlocked(this)

    private fun showOutcome(message: Int?) {
        outcome.text = message?.let(::getString)
        outcome.visibility = if (message == null) View.GONE else View.VISIBLE
    }

    private fun confirm() {
        if (review != null) return
        val binding = voiceHud.binding
        if (binding == null || !hasWindowFocus() || !activePage() || !isLocalVoiceInteractionSupported || isVoiceInteraction) {
            showOutcome(R.string.profile_action_unavailable)
            return
        }
        showOutcome(null)
        val next = ProfileActionReview(this, binding)
        review = next
        awaitingStart = true
        try {
            startLocalVoiceInteraction(next.issue())
            outcome.postDelayed({
                if (review === next && !next.presenting) {
                    cancel()
                    showOutcome(R.string.profile_action_unavailable)
                }
            }, ProfileActionReview.TIMEOUT)
        } catch (_: RuntimeException) {
            cancel()
            showOutcome(R.string.profile_action_unavailable)
        }
    }

    override fun onLocalVoiceInteractionStarted() {
        super.onLocalVoiceInteractionStarted()
        val r = review
        val interactor = voiceInteractor
        awaitingStart = false
        if (r == null || !r.valid() || r.endShowing == null || interactor == null || !r.attach(interactor)) {
            cancel()
            finish()
            return
        }
        // The OS inserts this activity's own task ID and assist token into the notification.
        interactor.notifyDirectActionsChanged()
    }

    override fun onGetDirectActions(stop: CancellationSignal, callback: Consumer<MutableList<DirectAction>>) {
        val r = review
        if (r == null || !r.discovering || r.offered || r.permitted?.invoke() != true || stop.isCanceled ||
            !r.sameInteractor()) {
            callback.accept(mutableListOf())
            return
        }
        r.offered = true
        callback.accept(mutableListOf(DirectAction.Builder(ProfileActionReview.ACTION).build()))
    }

    override fun onPerformDirectAction(id: String, args: Bundle, stop: CancellationSignal, callback: Consumer<Bundle>) {
        val r = review
        val result = Bundle()
        if (id == ProfileActionReview.ACTION && r != null && r.permitted?.invoke() == true &&
            !stop.isCanceled && r.sameInteractor() && r.consumePerform(args)) {
            try {
                val memory = ActivityManager.MemoryInfo()
                getSystemService(ActivityManager::class.java).getMemoryInfo(memory)
                result.putString(ProfileActionReview.RESULT, selectModelProfile(memory.totalMem / 1_000_000_000.0))
                showOutcome(R.string.profile_action_done)
            } catch (_: Exception) {
                showOutcome(R.string.profile_action_unavailable)
            } catch (_: LinkageError) {
                showOutcome(R.string.profile_action_unavailable)
            }
        }
        callback.accept(result)
    }

    internal fun release(old: ProfileActionReview) {
        if (review !== old) return
        review = null
        old.forget()
        // Android owns ActivityRecord.pendingVoiceInteractionStart. A failed/missing ACK
        // can leave it set even after our ticket is gone. Never retry using that record.
        if (awaitingStart) finish()
    }

    private fun cancel() {
        val old = review ?: return
        val end = old.endShowing
        release(old)
        // Finish only the session that accepted this ticket, never a replacement interaction.
        end?.invoke(true)
    }

    override fun onLocalVoiceInteractionStopped() {
        cancel()
        super.onLocalVoiceInteractionStopped()
    }
    override fun onWindowFocusChanged(focus: Boolean) {
        super.onWindowFocusChanged(focus)
        if (!focus && review?.presenting != true) {
            cancel()
            showOutcome(null)
        }
    }
    override fun onResume() { super.onResume(); resumed = true }
    override fun onPause() { resumed = false; cancel(); showOutcome(null); super.onPause() }
}

internal class ProfileActionReview(val source: ProfileActionActivity, private val binding: VoiceHudRegistry.Binding) {
    private val token = Binder()
    private val deadline = SystemClock.elapsedRealtime() + TIMEOUT
    private var preparedId: Int? = null
    private var interactor: VoiceInteractor? = null
    var endShowing: ((finishSession: Boolean) -> Unit)? = null
    var permitted: (() -> Boolean)? = null
    var discovering = false
    var offered = false
    private var performToken: Binder? = null
    var completed = false
        private set
    var presenting = false
    fun ownsPage() = source.review === this && voiceHud.binding === binding && source.activePage()
    fun valid() = ownsPage() && source.hasWindowFocus() && SystemClock.elapsedRealtime() < deadline
    fun attach(value: VoiceInteractor): Boolean {
        if (interactor != null && interactor !== value) return false
        interactor = value
        return sameInteractor()
    }
    fun sameInteractor() = try {
        interactor?.let { it === source.voiceInteractor && !it.isDestroyed && it.packageName == source.packageName } == true
    } catch (_: RuntimeException) { false }
    fun issue() = Bundle().apply { pending = this@ProfileActionReview; putBinder(KEY, token) }
    private fun matchesShow(flags: Int, args: Bundle?) = try {
        flags == VoiceInteractionSession.SHOW_SOURCE_ACTIVITY && valid() && args?.getBinder(KEY) === token
    } catch (_: RuntimeException) { false }
    private fun currentPreparation() = Build.VERSION.SDK_INT < 34 ||
        (binding.preparedProfile === this && preparedId != null && preparedId == binding.latestLocalShowId)
    fun beginPerform(): Bundle? {
        if (!currentPreparation() || !offered || completed || performToken != null) return null
        // The ticket sent in the original show options cannot authorize a perform callback.
        return Bundle().apply { putBinder(KEY, Binder().also { performToken = it }) }
    }
    fun consumePerform(args: Bundle): Boolean {
        val expected = performToken ?: return false
        val actual = try { args.getBinder(KEY) } catch (_: RuntimeException) { null }
        if (!currentPreparation() || actual !== expected) return false
        performToken = null
        completed = true
        return true
    }
    fun forget() {
        if (pending === this) pending = null
        if (binding.preparedProfile === this) binding.preparedProfile = null
        preparedId = null
        performToken = null
        permitted = null
        endShowing = null
        interactor = null
    }
    companion object {
        const val TIMEOUT = 5_000L
        const val KEY = "com.eon.assistant.PROFILE_AUTHORIZATION"
        const val ACTION = "com.eon.assistant.CHECK_MODEL_PROFILE"
        const val RESULT = "profile"
        private var pending: ProfileActionReview? = null
        fun showId(args: Bundle?) = try {
            args?.getInt(VoiceInteractionSession.KEY_SHOW_SESSION_ID, -1)?.takeIf { it > 0 }
        } catch (_: RuntimeException) { null }
        fun prepared(owner: VoiceHudRegistry.Binding, flags: Int, args: Bundle) {
            if (owner !== voiceHud.binding) return
            // Every preparation supersedes the previous show, including a gesture with
            // no pending ticket. Read only the OS ID for local shows; not foreground data.
            owner.latestLocalShowId = if (flags == VoiceInteractionSession.SHOW_SOURCE_ACTIVITY) showId(args) else null
            owner.preparedProfile?.let { old ->
                val end = old.endShowing
                old.source.release(old)
                end?.invoke(false) // Android may reuse this session for the replacement
            }
            val r = pending ?: return
            if (r.binding === owner && owner.latestLocalShowId != null && r.matchesShow(flags, args)) {
                r.preparedId = owner.latestLocalShowId
                owner.preparedProfile = r // stays bound to this preparation after take()
            }
        }
        // Deliberately refuse to reuse a session with assist callbacks from an earlier showing.
        fun candidate() = pending
        fun take(candidate: ProfileActionReview?, flags: Int, args: Bundle?): ProfileActionReview? {
            val r = pending ?: return null
            if (!r.matchesShow(flags, args) || !r.currentPreparation() ||
                (Build.VERSION.SDK_INT >= 34 && r.preparedId != showId(args))) return null
            pending = null
            return r.takeIf { it === candidate }
        }
    }
}
