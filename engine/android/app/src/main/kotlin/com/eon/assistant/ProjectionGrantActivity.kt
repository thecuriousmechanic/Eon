package com.eon.assistant

import android.app.Activity
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.media.projection.MediaProjectionManager
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.WindowManager
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import java.lang.ref.WeakReference
import java.util.UUID

/** One foreground operator, one picker result. No result is saved or included in a service intent. */
class ProjectionGrantActivity : Activity() {
    companion object {
        private var requesting: ProjectionGrantActivity? = null
        internal fun bindingEnded() { requesting?.end(R.string.projection_unavailable) }
        internal fun pending(ticket: String?) = requesting?.takeIf { ticket != null && it.dispatch == ticket }
        private var launcher = WeakReference<AssistantActivity>(null)
        internal fun open(activity: AssistantActivity) {
            if (requesting == null) launcher = WeakReference(activity)
            activity.startActivity(Intent(activity, ProjectionGrantActivity::class.java))
        }
        // Android can bring the selected EON task to the front before delivering window focus.
        // Only the exact launcher controls which opened this singleton are a valid second owner.
        internal fun launcherFocus(activity: AssistantActivity, focus: Boolean) {
            if (launcher.get() === activity) requesting?.let { source ->
                if (focus) source.dispatchIfFocused()
                else if (source.result != null) source.end(R.string.projection_stopped)
            }
        }
    }

    private val main = Handler(Looper.getMainLooper())
    private lateinit var status: TextView
    private lateinit var start: Button
    private lateinit var stop: Button
    private var resumed = false
    private var picking: Int? = null
    private var nextRequest = 1
    private var result: Intent? = null
    private var dispatch: String? = null
    private var deadline = 0L
    private val inTime get() = SystemClock.elapsedRealtime() < deadline
    private var holder: ProjectionGrantService? = null
    private val expireReturn = Runnable { if (result != null) end(R.string.projection_stopped) }
    private val eligible get() = ControlsAssist.bindingReady && selectedAndUnlocked(this)
    private val foreground get() = resumed && hasWindowFocus() && !isInMultiWindowMode && !isInPictureInPictureMode
    private val ownedForeground get() = foreground || launcher.get()?.projectionForeground == true

    private val screen = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (requesting === this@ProjectionGrantActivity) end(R.string.projection_stopped)
        }
    }
    private val guard = object : Runnable {
        override fun run() {
            if (requesting !== this@ProjectionGrantActivity) return
            if (!inTime || !eligible) end(R.string.projection_stopped) else main.postDelayed(this, 500)
        }
    }

    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        window.setHideOverlayWindows(true)
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            fitsSystemWindows = true
            addView(TextView(context).apply { setText(R.string.projection_help) })
        }
        status = TextView(this).apply { id = R.id.projection_grant_status; isSaveEnabled = false }
        column.addView(status)
        fun button(idValue: Int, label: Int, click: () -> Unit) = Button(this).apply {
            id = idValue
            setText(label)
            filterTouchesWhenObscured = true
            isSaveEnabled = false
            setOnClickListener { if (this@ProjectionGrantActivity.foreground) click() }
        }.also(column::addView)
        start = button(R.id.projection_grant_start, R.string.projection_request, ::request)
        stop = button(R.id.projection_grant_stop, R.string.projection_stop) { end(R.string.projection_stopped) }
        setContentView(column)
        registerReceiver(screen, IntentFilter(Intent.ACTION_SCREEN_OFF).apply { addAction(Intent.ACTION_USER_BACKGROUND) },
            Context.RECEIVER_NOT_EXPORTED)
        render(R.string.projection_idle)
    }

    private fun render(message: Int) {
        status.setText(message)
        start.isEnabled = requesting !== this
        stop.isEnabled = requesting === this
    }

    private fun request() {
        if (requesting != null || !eligible || !foreground || nextRequest > 0xffff) {
            render(R.string.projection_unavailable)
            return
        }
        val code = nextRequest++
        requesting = this
        deadline = SystemClock.elapsedRealtime() + 90_000
        picking = code
        render(R.string.projection_waiting)
        try {
            startActivityForResult(getSystemService(MediaProjectionManager::class.java).createScreenCaptureIntent(), code)
            guard.run()
        } catch (_: RuntimeException) { end(R.string.projection_unavailable) }
    }

    override fun onActivityResult(code: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(code, resultCode, data)
        if (requesting !== this || picking != code) return
        picking = null
        if (resultCode != RESULT_OK || data == null) end(R.string.projection_denied)
        else if (!inTime || !eligible) end(R.string.projection_unavailable)
        else {
            result = data
            deadline = SystemClock.elapsedRealtime() + 1_500
            // Activity results arrive before onResume/window focus. Allow only that short transition.
            main.postDelayed(expireReturn, 1_500)
            dispatchIfFocused()
        }
    }

    private fun dispatchIfFocused() {
        if (result == null || dispatch != null || !ownedForeground) return
        if (!inTime || !eligible) return end(R.string.projection_unavailable)
        val ticket = UUID.randomUUID().toString()
        dispatch = ticket
        try {
            checkNotNull(startService(Intent(this, ProjectionGrantService::class.java).setAction(ticket)))
        } catch (_: RuntimeException) { end(R.string.projection_unavailable) }
    }

    internal fun consume(service: ProjectionGrantService): Intent? {
        val grant = result
        dispatch = null
        result = null
        main.removeCallbacks(expireReturn)
        if (!inTime || !eligible || !ownedForeground || grant == null) {
            end(R.string.projection_unavailable)
            return null
        }
        holder = service
        // A purely diagnostic lease is short even if a platform stop predates callback registration.
        deadline = SystemClock.elapsedRealtime() + 60_000
        return grant
    }

    internal fun active() = render(R.string.projection_active)

    internal fun end(message: Int) {
        if (requesting !== this) return
        requesting = null
        main.removeCallbacks(guard)
        main.removeCallbacks(expireReturn)
        val picker = picking
        picking = null
        result = null
        dispatch = null
        if (picker != null) finishActivity(picker)
        val service = holder
        holder = null
        service?.release(this)
        render(message)
    }

    override fun onResume() { super.onResume(); resumed = true; dispatchIfFocused() }
    override fun onWindowFocusChanged(focus: Boolean) {
        super.onWindowFocusChanged(focus)
        if (focus) dispatchIfFocused() else if (dispatch != null) end(R.string.projection_stopped)
    }
    override fun onPause() {
        resumed = false
        if (dispatch != null && launcher.get()?.projectionForeground != true) end(R.string.projection_stopped)
        super.onPause()
    }
    override fun onDestroy() {
        end(R.string.projection_stopped)
        unregisterReceiver(screen)
        super.onDestroy()
    }
}
