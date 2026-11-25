package com.eon.assistant

import android.Manifest
import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.view.MotionEvent
import android.view.WindowManager
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView

/** Private foreground controls. Neither returning from Settings nor a permission result starts work. */
class FloatingHudActivity : Activity() {
    private lateinit var status: TextView
    private lateinit var notifications: Button
    private lateinit var start: Button
    private lateinit var stop: Button
    private lateinit var micAccess: Button
    private lateinit var micStart: Button
    private var requestingMic = false
    private var resumed = false
    internal val canStart get() = resumed && hasWindowFocus() && !isFinishing && !isDestroyed
    private val refresh = { render() }

    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        window.setHideOverlayWindows(true)
        FloatingHudService.createChannel(this)
        setTitle(R.string.floating_controls)
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            val pad = (16 * resources.displayMetrics.density).toInt()
            setPadding(pad, pad, pad, pad)
        }
        fun button(label: Int, click: () -> Unit) = Button(this).apply {
            setText(label)
            filterTouchesWhenObscured = true
            setOnClickListener {
                if (resumed && hasWindowFocus() && FloatingHudService.unlocked(this@FloatingHudActivity)) click()
            }
        }.also(column::addView)
        column.addView(TextView(this).apply { setText(R.string.floating_explanation) })
        status = TextView(this).apply { accessibilityLiveRegion = TextView.ACCESSIBILITY_LIVE_REGION_POLITE }.also(column::addView)
        button(R.string.floating_review_display) {
            FloatingHudService.stopFrom(this)
            settings(Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION, Uri.parse("package:$packageName")))
        }
        notifications = button(R.string.floating_review_notifications) {
            if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
                requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
            } else {
                settings(Intent(Settings.ACTION_CHANNEL_NOTIFICATION_SETTINGS)
                    .putExtra(Settings.EXTRA_APP_PACKAGE, packageName)
                    .putExtra(Settings.EXTRA_CHANNEL_ID, FloatingHudService.CHANNEL))
            }
        }
        button(R.string.floating_notification_settings) {
            settings(Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE, packageName))
        }
        start = button(R.string.floating_start) {
            if (!FloatingHudService.requestStart(this)) status.setText(R.string.floating_unavailable)
        }
        stop = button(R.string.floating_stop) { FloatingHudService.stopFrom(this) }
        column.addView(TextView(this).apply { setText(R.string.floating_mic_help) })
        micAccess = button(R.string.voice_mic_permission) {
            if (!MicPermission.granted(this) && !requestingMic) {
                requestingMic = true
                requestPermissions(arrayOf(MicPermission.NAME), 2)
            } else if (!MicPermission.allowed(this)) status.setText(R.string.voice_mic_pregranted)
        }
        micStart = button(R.string.floating_mic_start) {
            if (!FloatingHudService.startMic(this)) status.setText(R.string.floating_mic_unavailable)
        }
        setContentView(ScrollView(this).apply {
            fitsSystemWindows = true
            addView(column)
        })
    }

    override fun onRequestPermissionsResult(code: Int, permissions: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(code, permissions, results)
        if (code != 2 || !requestingMic) return
        requestingMic = false
        MicPermission.reviewed = permissions.singleOrNull() == MicPermission.NAME &&
            results.singleOrNull() == PackageManager.PERMISSION_GRANTED && MicPermission.granted(this)
        render() // Never start from this callback.
    }

    override fun dispatchTouchEvent(event: MotionEvent): Boolean {
        if (event.flags and (MotionEvent.FLAG_WINDOW_IS_OBSCURED or MotionEvent.FLAG_WINDOW_IS_PARTIALLY_OBSCURED) != 0) return true
        return super.dispatchTouchEvent(event)
    }

    override fun onResume() {
        super.onResume()
        resumed = true
        FloatingHudService.changed = refresh
        render()
    }

    override fun onWindowFocusChanged(focus: Boolean) {
        super.onWindowFocusChanged(focus)
        if (!focus) FloatingHudService.cancelPending(this)
        if (resumed && focus) render()
    }

    override fun onPause() {
        resumed = false
        FloatingHudService.cancelPending(this)
        if (FloatingHudService.changed === refresh) FloatingHudService.changed = null
        super.onPause()
    }

    private fun settings(intent: Intent) {
        try {
            startActivity(intent)
        } catch (_: ActivityNotFoundException) {
            status.setText(R.string.floating_settings_unavailable)
        } catch (_: SecurityException) {
            status.setText(R.string.floating_settings_unavailable)
        }
    }

    private fun render() {
        val overlay = Settings.canDrawOverlays(this)
        val notice = FloatingHudService.notificationsAllowed(this)
        val running = FloatingHudService.startedOrPending
        status.setText(when {
            running -> R.string.floating_running
            !overlay -> R.string.floating_display_needed
            !notice -> R.string.floating_notification_needed
            else -> R.string.floating_ready
        })
        notifications.isEnabled = !notice
        start.isEnabled = !running && overlay && notice
        stop.isEnabled = running
        micAccess.isEnabled = !MicPermission.allowed(this)
        micStart.isEnabled = running
    }
}
