package com.eon.assistant

import android.app.Activity
import android.app.KeyguardManager
import android.database.ContentObserver
import android.graphics.Rect
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.Settings
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.view.accessibility.AccessibilityNodeInfo
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView

/** Private API 34+ demo. Neither intents nor the accessibility service supply actions or targets. */
class DemoActionActivity : Activity() {
    private lateinit var targets: LinearLayout
    private lateinit var counter: Button
    private lateinit var samples: ScrollView
    private lateinit var list: LinearLayout
    private lateinit var rows: List<TextView>
    private var count = 0
    private lateinit var gate: DemoActionGate<View>
    private lateinit var status: TextView
    private lateinit var confirm: Button
    private lateinit var cancel: Button
    private val handler = Handler(Looper.getMainLooper())
    private val expire = Runnable { clearReview("Review expired. No action was taken.") }
    private var observation: Runnable? = null
    private var resumed = false
    private var observingShortcut = false
    private val shortcutChanges = object : ContentObserver(handler) {
        override fun onChange(selfChange: Boolean) = clearReview("Shortcut changed. No action is approved.")
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (Build.VERSION.SDK_INT < 34) { finish(); return }
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        window.setHideOverlayWindows(true)
        title = "EON accessibility action demo"
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            val pad = dp(12)
            setPadding(pad, pad, pad, pad)
            fitsSystemWindows = true
            isSaveFromParentEnabled = false
        }
        val controls = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        controls.addView(TextView(this).apply {
            text = "Review and confirm one action on the sample controls below. The accessibility shortcut only inspects; it never confirms an action."
        })
        targets = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        counter = Button(this).apply {
            id = R.id.demo_counter
            text = "Demo count: 0"
            setOnClickListener { text = "Demo count: ${++count}" }
        }
        rows = (1..16).map { i -> TextView(this).apply {
            text = "Sample row $i"
            minHeight = dp(36)
        } }
        list = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            rows.forEach { addView(it) }
        }
        samples = ScrollView(this).apply {
            id = R.id.demo_samples
            addView(list)
        }
        targets.addView(counter)
        targets.addView(samples, -1, dp(104))
        column.addView(targets)
        gate = DemoActionGate<View>(counter, samples, { DemoNodeLookup.findView(targets, it) }, ::offer,
            ::liveScope, SystemClock::elapsedRealtime, ::readValue, ::dispatch)
        controls.addView(sampleModeButton())
        controls.addView(reviewButton(R.id.demo_review_click, "Review counter click", DemoAction.CLICK_COUNTER))
        controls.addView(reviewButton(R.id.demo_review_scroll, "Review list scroll", DemoAction.SCROLL_SAMPLES_FORWARD))
        status = TextView(this).apply {
            id = R.id.demo_status
            accessibilityLiveRegion = View.ACCESSIBILITY_LIVE_REGION_POLITE
        }
        controls.addView(status)
        confirm = protectedButton().apply { id = R.id.demo_confirm }
        cancel = protectedButton().apply {
            id = R.id.demo_cancel
            text = "Cancel"
            setOnClickListener { clearReview("Canceled. No action was taken.") }
        }
        controls.addView(confirm)
        controls.addView(cancel)
        val fieldGroup = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val fieldLabel = TextView(this).apply {
            id = R.id.demo_field_label
            text = "Sample text field · lookup only, no input stored"
        }
        val field = DemoSampleField(this)
        fieldGroup.addView(fieldLabel)
        fieldGroup.addView(field)
        controls.addView(fieldGroup)
        val lookup = DemoFieldLookup(fieldGroup, fieldLabel, field)
        controls.addView(protectedButton().apply {
            id = R.id.demo_locate_field
            text = "Locate sample field"
            setOnClickListener {
                clearReview("")
                val found = try {
                    val scope = liveScope()
                    scope != null && lookup.present() && liveScope() == scope
                } catch (_: RuntimeException) { false }
                status.text = if (found) "Located the sample text field. No value was read or changed."
                    else "Sample field unavailable. No action was taken."
            }
        })
        clearReview("No action is approved.")
        val retireOnDetach = object : View.OnAttachStateChangeListener {
            override fun onViewAttachedToWindow(view: View) = Unit
            override fun onViewDetachedFromWindow(view: View) = clearReview("Review cleared. No action is approved.")
        }
        counter.addOnAttachStateChangeListener(retireOnDetach)
        samples.addOnAttachStateChangeListener(retireOnDetach)
        column.addView(ScrollView(this).apply { addView(controls) }, LinearLayout.LayoutParams(-1, 0, 1f))
        setContentView(column)
    }

    private fun dp(value: Int) = (value * resources.displayMetrics.density + 0.5f).toInt()

    private fun protectedButton(): Button = object : Button(this) {
        init {
            filterTouchesWhenObscured = true
            setAccessibilityDataSensitive(View.ACCESSIBILITY_DATA_SENSITIVE_YES)
        }
        override fun onFilterTouchEventForSecurity(event: MotionEvent): Boolean =
            event.flags and MotionEvent.FLAG_WINDOW_IS_PARTIALLY_OBSCURED == 0 && super.onFilterTouchEventForSecurity(event)
    }

    private fun reviewButton(id: Int, label: String, action: DemoAction) = protectedButton().also {
        it.id = id
        it.text = label
        it.setOnClickListener { review(action) }
    }

    // Let an operator reproduce missing actions on stock widgets. This changes only these two
    // samples' providers, not the gate, and never invokes an action or approves a review.
    private fun sampleModeButton() = protectedButton().apply {
        id = R.id.demo_sample_mode
        val switchLabel = "Try samples without native actions"
        text = switchLabel
        var reduced = false
        setOnClickListener {
            clearReview("Sample mode changed. No action is approved.")
            reduced = !reduced
            fun without(vararg actions: AccessibilityNodeInfo.AccessibilityAction): View.AccessibilityDelegate? {
                if (!reduced) return null
                return object : View.AccessibilityDelegate() {
                    override fun onInitializeAccessibilityNodeInfo(host: View, info: AccessibilityNodeInfo) {
                        super.onInitializeAccessibilityNodeInfo(host, info)
                        actions.forEach { info.removeAction(it) }
                    }
                    override fun performAccessibilityAction(host: View, action: Int, args: Bundle?): Boolean =
                        actions.none { it.id == action } && super.performAccessibilityAction(host, action, args)
                }
            }
            counter.accessibilityDelegate = without(AccessibilityNodeInfo.AccessibilityAction.ACTION_CLICK)
            samples.accessibilityDelegate = without(AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_FORWARD,
                AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_DOWN)
            text = if (reduced) "Restore native sample actions" else switchLabel
        }
    }

    private fun liveScope(): DemoScope? {
        if (!resumed || isFinishing || isDestroyed || !hasWindowFocus() || isInMultiWindowMode ||
            isInPictureInPictureMode || !targets.isAttachedToWindow || targets.rootView !== window.decorView ||
            !targets.isShown || getSystemService(KeyguardManager::class.java)?.isDeviceLocked != false ||
            !observingShortcut || !DemoShortcut.eligible(this)) return null
        return DemoScope(InspectionAccess.environmentEpoch, window.decorView.windowToken ?: return null)
    }

    // Read only capability metadata on one known, same-window View. Never enumerate OS windows,
    // accessibility roots, node text, other package controls, or children of unknown views.
    private fun safeNode(view: View): AccessibilityNodeInfo? {
        if (view.context !== this || view.parent !== targets || view.windowToken !== window.decorView.windowToken ||
            !view.isAttachedToWindow || !view.isShown || !view.isEnabled || view.isAccessibilityDataSensitive) return null
        val action = if (view === counter) DemoAction.CLICK_COUNTER else DemoAction.SCROLL_SAMPLES_FORWARD
        return DemoNodeLookup.find(targets, action)?.takeIf { it.first === view }?.second?.takeIf { node ->
            node.packageName == packageName && node.isVisibleToUser && node.isEnabled &&
                !node.isAccessibilityDataSensitive && !node.isPassword && !node.isEditable && node.inputType == 0
        }
    }

    // Neither the point nor a receiver comes from the caller. Resolve the whole, unclipped
    // control's center against this decor only. Reject any ambiguous sibling, regardless of Z.
    private fun hasLocalHit(view: View): Boolean {
        val bounds = Rect()
        if (!view.getGlobalVisibleRect(bounds) || bounds.isEmpty ||
            bounds.width() != view.width || bounds.height() != view.height) return false
        val x = bounds.centerX()
        val y = bounds.centerY()
        var hit: View = when (view) {
            counter -> counter
            samples -> {
                if (samples.childCount != 1 || samples.getChildAt(0) !== list ||
                    list.childCount != rows.size || rows.indices.any { list.getChildAt(it) !== rows[it] }) return false
                rows.singleOrNull { it.isShown && it.getGlobalVisibleRect(bounds) && bounds.contains(x, y) }
                    ?.takeIf { !it.isAccessibilityDataSensitive && !it.isClickable && !it.isLongClickable }
                    ?: return false
            }
            else -> return false
        }
        while (true) {
            if (!hit.isShown || hit.alpha != 1f || !hit.matrix.isIdentity || hit.animation != null ||
                hit.clipBounds != null || !hit.getGlobalVisibleRect(bounds) || !bounds.contains(x, y)) return false
            if (hit === window.decorView) return true
            val parent = hit.parent as? ViewGroup ?: return false
            for (i in 0 until parent.childCount) {
                val sibling = parent.getChildAt(i)
                if (sibling !== hit && sibling.visibility == View.VISIBLE && sibling.getGlobalVisibleRect(bounds) &&
                    bounds.contains(x, y)) return false
            }
            hit = parent
        }
    }

    private fun offer(view: View, action: DemoAction): DemoOffer? {
        val node = safeNode(view) ?: return null
        val route = if (node.actionList.any { it.id == action.nativeId() }) DemoRoute.NATIVE else DemoRoute.LOCAL_HIT
        val before = visibleValue(view, action) ?: return null
        val offer = when (action) {
            DemoAction.CLICK_COUNTER -> if (view === counter && counter.isClickable && count < Int.MAX_VALUE)
                DemoOffer(route, before, 1) else null
            DemoAction.SCROLL_SAMPLES_FORWARD -> if (view === samples && samples.canScrollVertically(1))
                DemoOffer(route, before, samples.height - samples.paddingTop - samples.paddingBottom) else null
        }
        return offer?.takeIf { hasLocalHit(view) }
    }

    private fun dispatch(view: View, action: DemoAction, offer: DemoOffer): Boolean = when (offer.route) {
        DemoRoute.NATIVE -> view.performAccessibilityAction(action.nativeId(), null)
        // Synchronous in-process operations on the hit-tested receiver. No touch stream, event
        // queue, coordinates in an intent, or platform gesture that could land in another window.
        DemoRoute.LOCAL_HIT -> when (action) {
            DemoAction.CLICK_COUNTER -> view.performClick()
            DemoAction.SCROLL_SAMPLES_FORWARD -> { view.scrollBy(0, offer.maxDelta); true }
        }
    }

    private fun readValue(view: View, action: DemoAction): Int? {
        if (safeNode(view) == null || !hasLocalHit(view)) return null
        return visibleValue(view, action)
    }

    // Measure the rendered EON sample, not just the value assigned by the listener or a
    // provider's return code. This never reads arbitrary accessibility text or other windows.
    private fun visibleValue(view: View, action: DemoAction): Int? {
        return when (action) {
            DemoAction.CLICK_COUNTER -> count.takeIf { view === counter && counter.text.toString() == "Demo count: $it" }
            DemoAction.SCROLL_SAMPLES_FORWARD -> if (view === samples) DemoSamplePosition.read(samples, list, rows) else null
        }
    }

    private fun DemoAction.nativeId(): Int = when (this) {
        DemoAction.CLICK_COUNTER -> AccessibilityNodeInfo.ACTION_CLICK
        DemoAction.SCROLL_SAMPLES_FORWARD -> AccessibilityNodeInfo.ACTION_SCROLL_FORWARD
    }

    private fun review(action: DemoAction) {
        clearReview("")
        val request = try { gate.review(action) } catch (_: RuntimeException) { null }
        if (request == null) {
            status.text = "Unavailable. Unlock the device, use full screen, select EON as assistant, and enable its accessibility service and shortcut. The sample control must be visible and support the action."
            return
        }
        val effect = when (action) {
            DemoAction.CLICK_COUNTER -> "increment demo count once"
            DemoAction.SCROLL_SAMPLES_FORWARD -> "scroll sample list forward once"
        }
        val route = if (request.offer.route == DemoRoute.NATIVE) "native action" else "local hit fallback"
        confirm.text = "Confirm: $effect ($route)"
        status.text = "Review expires in 15 seconds. Nothing has happened yet."
        confirm.setOnClickListener {
            val dispatched = try { gate.confirm(request) } catch (_: RuntimeException) { false }
            hideReview()
            if (!dispatched) {
                clearReview("Change not verified. No retry or approval remains.")
            } else {
                val checkEffect = Runnable {
                    val changed = try { gate.verify(request) } catch (_: RuntimeException) { false }
                    clearReview(if (changed) "Verified: $effect ($route)." else "Change not verified. No retry or approval remains.")
                }
                if (request.offer.route == DemoRoute.NATIVE && action == DemoAction.SCROLL_SAMPLES_FORWARD) {
                    // Stock ScrollView scrolls asynchronously. This callback only reads; it cannot dispatch.
                    observation = checkEffect
                    status.text = "Checking list movement. No approval remains."
                    handler.postDelayed(checkEffect, 400L)
                } else checkEffect.run()
            }
        }
        confirm.visibility = View.VISIBLE
        cancel.visibility = View.VISIBLE
        handler.postDelayed(expire, DemoActionGate.REVIEW_MS)
    }

    private fun hideReview() {
        handler.removeCallbacks(expire)
        confirm.setOnClickListener(null)
        confirm.visibility = View.GONE
        cancel.visibility = View.GONE
    }

    private fun clearReview(message: String) {
        if (!::gate.isInitialized) return
        gate.cancel()
        observation?.let { handler.removeCallbacks(it) }
        observation = null
        hideReview()
        status.text = message
    }

    override fun onUserInteraction() {
        super.onUserInteraction()
        if (observation != null) clearReview("Verification interrupted. No retry or approval remains.")
    }

    override fun onResume() {
        super.onResume()
        resumed = true
        try {
            contentResolver.registerContentObserver(Settings.Secure.getUriFor(DemoShortcut.SETTING), false, shortcutChanges)
            observingShortcut = true
        } catch (_: SecurityException) {
            observingShortcut = false
        }
    }
    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus) clearReview("Review cleared. No action is approved.")
    }
    override fun onPause() {
        resumed = false
        if (observingShortcut) {
            contentResolver.unregisterContentObserver(shortcutChanges)
            observingShortcut = false
        }
        clearReview("Review cleared. No action is approved.")
        super.onPause()
    }
}
