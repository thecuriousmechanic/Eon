package com.eon.assistant

import android.app.Activity
import android.app.Application
import android.os.Bundle

// setHideOverlayWindows excludes same-UID overlays. Remove ours before our own windows appear.
internal object FloatingHudActivities : Application.ActivityLifecycleCallbacks {
    private val visible = mutableSetOf<Activity>()
    val showing get() = visible.isNotEmpty()
    var changed: (() -> Unit)? = null

    override fun onActivityPreStarted(activity: Activity) {
        if (visible.add(activity)) changed?.invoke()
    }
    override fun onActivityStopped(activity: Activity) {
        if (visible.remove(activity)) changed?.invoke()
    }
    override fun onActivityDestroyed(activity: Activity) = onActivityStopped(activity)
    override fun onActivityCreated(activity: Activity, state: Bundle?) = Unit
    override fun onActivityStarted(activity: Activity) = Unit
    override fun onActivityResumed(activity: Activity) = Unit
    override fun onActivityPaused(activity: Activity) = Unit
    override fun onActivitySaveInstanceState(activity: Activity, state: Bundle) = Unit
}
