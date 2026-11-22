package com.eon.assistant

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Handler
import android.os.IBinder
import android.os.Looper

/** Holds only Android's revocable grant. The grant never leaves this service. */
class ProjectionGrantService : Service() {
    private var owner: ProjectionGrantActivity? = null
    private var projection: MediaProjection? = null
    private var observer: MediaProjection.Callback? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val source = ProjectionGrantActivity.pending(intent?.action)
        val data = source?.consume(this)
        if (source == null || data == null) {
            if (owner == null) stopSelfResult(startId)
            return START_NOT_STICKY
        }
        owner = source
        try {
            val channel = "eon.projection.grant"
            getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel(channel, getString(R.string.projection_channel), NotificationManager.IMPORTANCE_LOW))
            val notice = Notification.Builder(this, channel)
                .setSmallIcon(android.R.drawable.ic_dialog_info)
                .setContentTitle(getString(R.string.projection_channel))
                .setContentText(getString(R.string.projection_active))
                .setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE)
                .setOngoing(true).build()
            startForeground(951, notice, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
            val grant = checkNotNull(getSystemService(MediaProjectionManager::class.java)
                .getMediaProjection(Activity.RESULT_OK, data))
            projection = grant
            val callback = object : MediaProjection.Callback() {
                override fun onStop() {
                    if (projection === grant) {
                        // Already stopped by Android. Don't issue a second stop.
                        projection = null
                        grant.unregisterCallback(this)
                        source.end(R.string.projection_revoked)
                    }
                }
            }
            observer = callback
            grant.registerCallback(callback, Handler(Looper.getMainLooper()))
            source.active()
        } catch (_: RuntimeException) { source.end(R.string.projection_unavailable) }
        return START_NOT_STICKY
    }

    internal fun release(source: ProjectionGrantActivity) {
        if (owner !== source) return
        owner = null
        val grant = projection
        projection = null
        val callback = observer
        observer = null
        try {
            if (grant != null && callback != null) grant.unregisterCallback(callback)
        } finally {
            try { grant?.stop() } finally {
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
            }
        }
    }

    override fun onTaskRemoved(rootIntent: Intent?) { owner?.end(R.string.projection_stopped) }
    override fun onDestroy() { owner?.end(R.string.projection_stopped); super.onDestroy() }
}
