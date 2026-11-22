package com.eon.assistant

import android.Manifest
import android.app.Activity
import android.app.ActivityManager
import android.app.AppOpsManager
import android.app.KeyguardManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.net.Uri
import android.os.Build
import android.os.CancellationSignal
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import uniffi.eon_uniffi.runProfileWorkflow

/** A passive HUD unless a focused Activity explicitly starts one short microphone take. */
class FloatingHudService : Service() {
    companion object {
        internal const val CHANNEL = "eon.floating.status"
        private const val NOTICE = 940
        private const val START = "hud.start"
        private const val STOP = "hud.stop"
        private const val CHECK = "hud.profile"
        private const val TONE = "hud.tone"
        private val consent = FloatingHudConsent()
        private var owner: FloatingHudService? = null
        internal val startedOrPending get() = consent.startedOrPending
        internal var changed: (() -> Unit)? = null

        internal fun createChannel(c: Context) {
            val channel = NotificationChannel(CHANNEL, c.getString(R.string.floating_channel), NotificationManager.IMPORTANCE_LOW)
            channel.setShowBadge(false)
            c.getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
        }

        internal fun notificationsAllowed(c: Context): Boolean {
            val manager = c.getSystemService(NotificationManager::class.java)
            val importance = manager.getNotificationChannel(CHANNEL)?.importance ?: return false
            return (Build.VERSION.SDK_INT < 33 || c.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
                PackageManager.PERMISSION_GRANTED) && manager.areNotificationsEnabled() && importance != NotificationManager.IMPORTANCE_NONE
        }

        internal fun unlocked(c: Context) = c.getSystemService(PowerManager::class.java).isInteractive &&
            !c.getSystemService(KeyguardManager::class.java).isKeyguardLocked

        private fun eligible(c: Context) = unlocked(c) && Settings.canDrawOverlays(c) && notificationsAllowed(c)

        internal fun cancelPending(activity: FloatingHudActivity) = consent.cancelPending(activity)

        internal fun requestStart(activity: FloatingHudActivity): Boolean {
            val ticket = consent.request(activity.canStart && eligible(activity), activity) ?: return false
            return try {
                // Called only during the foreground Activity's explicit Start click. A stale delivery
                // is allowed to stop without first promoting an unwanted service/notification.
                checkNotNull(activity.startService(command(activity, START, ticket)))
                changed?.invoke()
                true
            } catch (_: RuntimeException) {
                consent.end()
                changed?.invoke()
                false
            }
        }

        internal fun stopFrom(context: Context) {
            if (!startedOrPending) return
            consent.end()
            val service = owner
            if (service != null) service.stopNow()
            else context.stopService(Intent(context, FloatingHudService::class.java))
            changed?.invoke()
        }

        // Intentionally NOT a service Intent, notification action, binding or queued operation.
        internal fun startMic(activity: FloatingHudActivity): Boolean {
            val service = owner ?: return false
            if (!activity.canStart || !FloatingHudActivities.showing || !eligible(activity) ||
                !MicPermission.allowed(activity)) return false
            return service.openMic(activity)
        }

        private fun command(c: Context, action: String, ticket: String) =
            Intent(c, FloatingHudService::class.java).setAction(action).setData(Uri.fromParts("eon-hud", ticket, null))
    }

    private val main = Handler(Looper.getMainLooper())
    private var card: FloatingHudCard? = null
    private var observingScreen = false
    private var observingOps = false
    private var work: CancellationSignal? = null
    private var check: ProfileCheck = ProfileCheck.Idle
    private var tone: ToneScan = ToneScan.Idle
    private var microphone: SessionMic? = null
    private var micLevels = emptyList<Int>()
    private val micGuard = object : Runnable {
        override fun run() {
            if (microphone == null) return
            if (!MicPermission.allowed(this@FloatingHudService) || !eligible(this@FloatingHudService)) stopNow()
            else main.postDelayed(this, 150)
        }
    }

    private val screen = object : BroadcastReceiver() {
        override fun onReceive(c: Context, i: Intent) {
            if (i.action == Intent.ACTION_SCREEN_OFF || i.action == Intent.ACTION_USER_BACKGROUND) stopNow()
        }
    }
    private val ops = AppOpsManager.OnOpChangedListener { _, _ ->
        main.post { if (owner === this) reconcile() }
    }
    private val guard = object : Runnable {
        override fun run() {
            if (reconcile()) main.postDelayed(this, 2_000)
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val ticket = intent?.data?.schemeSpecificPart
        val source = consent.sourceFor(ticket) as? FloatingHudActivity
        if (intent?.action == START && consent.consume(ticket, source?.canStart == true && eligible(this))) {
            owner = this
            try {
                foreground(checkNotNull(ticket), liveMic = false)
                registerReceiver(screen, IntentFilter(Intent.ACTION_SCREEN_OFF).apply { addAction(Intent.ACTION_USER_BACKGROUND) })
                observingScreen = true
                getSystemService(AppOpsManager::class.java).startWatchingMode(AppOpsManager.OPSTR_SYSTEM_ALERT_WINDOW, packageName, ops)
                observingOps = true
                card = FloatingHudCard(this)
                FloatingHudActivities.changed = {
                    if (FloatingHudActivities.showing && microphone != null) stopNow() else reconcile()
                }
                guard.run()
            } catch (_: RuntimeException) {
                stopNow()
            }
            changed?.invoke()
        } else if (owner === this && consent.accepts(ticket)) {
            when (intent?.action) {
                STOP -> stopNow()
                CHECK -> if (reconcile()) profile()
                TONE -> if (reconcile()) meterTone()
            }
        } else if (owner !== this && !consent.startedOrPending) {
            // A stale notification or redelivery is not a request to recreate a card.
            stopSelfResult(startId)
            changed?.invoke()
        }
        return START_NOT_STICKY
    }

    private fun foreground(ticket: String, liveMic: Boolean) {
        val baseline = if (Build.VERSION.SDK_INT >= 34) ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE else 0
        val type = if (liveMic) baseline or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE else baseline
        // Never let startForeground infer the microphone type from the manifest for an idle HUD.
        startForeground(NOTICE, notification(ticket, liveMic), type)
    }

    private fun notification(ticket: String, liveMic: Boolean): Notification {
        val immutable = PendingIntent.FLAG_IMMUTABLE
        fun action(name: String) = PendingIntent.getService(this, 0, command(this, name, ticket), immutable)
        val controls = PendingIntent.getActivity(this, 0, Intent(this, FloatingHudActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP), immutable)
        val builder = Notification.Builder(this, CHANNEL)
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentTitle(getString(R.string.floating_notification_title))
            .setContentText(getString(if (liveMic) R.string.floating_mic_active_notice else R.string.floating_notification_text))
            .setVisibility(Notification.VISIBILITY_SECRET)
            .setContentIntent(controls)
            .setDeleteIntent(action(STOP))
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE)
            .addAction(Notification.Action.Builder(null, getString(R.string.floating_stop), action(STOP)).build())
        if (!liveMic) {
            builder.addAction(Notification.Action.Builder(null, getString(R.string.floating_check), action(CHECK)).build())
            builder.addAction(Notification.Action.Builder(null, getString(R.string.floating_tone), action(TONE)).build())
        }
        return builder.build()
    }

    private fun reconcile(): Boolean {
        if (owner !== this) return false
        try {
            if (consent.active != null && eligible(this)) {
                val panel = checkNotNull(card)
                panel.render(check, tone, microphone != null, micLevels)
                if (FloatingHudActivities.showing) panel.hide() else panel.show()
                return true
            }
        } catch (_: RuntimeException) {
            // E.g. special access lost during addView, removed display, or invalid window token.
        }
        stopNow()
        return false
    }

    private fun profile() {
        if (work != null || microphone != null) return
        micLevels = emptyList()
        val request = CancellationSignal().also { work = it }
        tone = ToneScan.Idle
        check = ProfileCheck.Requested()
        if (!reconcile()) return
        val app = applicationContext
        Thread({
            val receipt = try {
                request.throwIfCanceled()
                val info = ActivityManager.MemoryInfo()
                app.getSystemService(ActivityManager::class.java).getMemoryInfo(info)
                runProfileWorkflow(info.totalMem / 1_000_000_000.0).let { ProfileCheck.Complete(it.profile, it.completedTasks) }
            } catch (_: Exception) { null } catch (_: LinkageError) { null }
            main.post {
                if (work === request) {
                    work = null
                    check = receipt ?: ProfileCheck.Unavailable
                    reconcile()
                }
            }
        }, "EON floating profile check").start()
    }

    private fun meterTone() {
        if (work != null || microphone != null) return
        micLevels = emptyList()
        val request = CancellationSignal().also { work = it }
        val reading = ToneScan.Reading(ToneScan.Request())
        check = ProfileCheck.Idle
        tone = reading
        if (!reconcile()) return
        val app = applicationContext
        Thread({
            val ok = try {
                StockToneReader.read(app, request) { level ->
                    main.post {
                        if (work !== request) return@post
                        val current = tone as? ToneScan.Reading ?: return@post
                        tone = current.copy(levels = current.levels + level)
                        reconcile()
                    }
                }
                true
            } catch (_: Exception) { false }
            main.post {
                if (work === request) {
                    work = null
                    val levels = (tone as? ToneScan.Reading)?.levels.orEmpty()
                    tone = if (ok && levels.isNotEmpty()) ToneScan.Complete(levels) else ToneScan.Unavailable
                    reconcile()
                }
            }
        }, "EON floating stock tone").start()
    }

    private fun openMic(activity: Activity): Boolean {
        val ticket = consent.active ?: return false
        if (work != null || microphone != null) return false
        try {
            foreground(ticket, liveMic = true) // while the tapping Activity is still visible
            val take = SessionMic.open()
            microphone = take
            check = ProfileCheck.Idle
            tone = ToneScan.Idle
            micLevels = emptyList()
            if (!activity.moveTaskToBack(true)) {
                stopNow()
                return false
            }
            main.post(micGuard)
            val app = applicationContext
            Thread({
                val complete = take.measure(app) { level ->
                    main.post {
                        if (microphone === take) {
                            micLevels = micLevels + level
                            reconcile()
                        }
                    }
                }
                main.post {
                    if (microphone !== take) return@post
                    microphone = null // worker already released AudioRecord
                    main.removeCallbacks(micGuard)
                    if (!complete || micLevels.size != ToneLevels.LIMIT || !MicPermission.allowed(this) || !eligible(this)) {
                        stopNow()
                    } else {
                        try {
                            foreground(ticket, liveMic = false)
                            reconcile()
                        } catch (_: RuntimeException) { stopNow() }
                    }
                }
            }, "EON floating mic RMS").start()
            return true
        } catch (_: RuntimeException) {
            stopNow()
            return false
        }
    }

    private fun release() {
        if (owner !== this) return
        owner = null
        consent.end()
        FloatingHudActivities.changed = null
        val take = microphone
        microphone = null
        take?.close()
        micLevels = emptyList()
        val canceled = work
        work = null
        canceled?.cancel()
        main.removeCallbacksAndMessages(null)
        try { card?.hide() } catch (_: IllegalArgumentException) { /* Already removed. */ }
        card = null
        check = ProfileCheck.Idle
        tone = ToneScan.Idle
        if (observingScreen) unregisterReceiver(screen)
        observingScreen = false
        if (observingOps) getSystemService(AppOpsManager::class.java).stopWatchingMode(ops)
        observingOps = false
        stopForeground(STOP_FOREGROUND_REMOVE)
        changed?.invoke()
    }

    private fun stopNow() {
        release()
        stopSelf()
    }

    override fun onDestroy() {
        release()
        super.onDestroy()
    }
}
