package com.eon.assistant

import android.app.ActivityManager
import android.content.Context
import android.os.CancellationSignal

/** Counts only tasks returned for EON's UID. Never query global recents, intents, labels or IDs. */
internal object OwnActivityTasks {
    data class Counts(val running: Int, val retained: Int)

    fun read(context: Context, stop: CancellationSignal): Counts? {
        stop.throwIfCanceled()
        val manager = context.getSystemService(ActivityManager::class.java) ?: return null
        val tasks = manager.appTasks
        if (tasks.size > 16) return null
        var running = 0
        for (task in tasks) {
            stop.throwIfCanceled()
            // A task removed during the query is unavailable, not evidence that it completed.
            if (task.taskInfo.isRunning) running++
        }
        stop.throwIfCanceled()
        return Counts(running, tasks.size - running)
    }
}
