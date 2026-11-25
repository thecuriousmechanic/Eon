package com.eon.assistant

import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Region
import android.graphics.Rect
import android.util.AttributeSet
import android.view.View
import android.view.WindowInsets
import android.widget.Button
import android.widget.FrameLayout
import android.widget.ImageButton
import android.widget.ScrollView
import android.widget.TextView
import kotlin.math.roundToInt

internal fun interface HudStrings {
    fun get(id: Int, vararg args: Any): String
}

internal data class ProfileNotice(val status: String?, val result: String? = null)

internal fun profileNotice(check: ProfileCheck?, strings: HudStrings): ProfileNotice = when (check) {
    null -> ProfileNotice(null)
    ProfileCheck.Idle -> ProfileNotice(strings.get(R.string.voice_hud_ready))
    is ProfileCheck.Requested -> ProfileNotice(strings.get(R.string.voice_hud_checking))
    ProfileCheck.Unavailable -> ProfileNotice(strings.get(R.string.voice_hud_failed))
    is ProfileCheck.Complete -> ProfileNotice(
        strings.get(R.string.voice_hud_checked, check.completedTasks.toLong()),
        strings.get(R.string.voice_hud_recommendation, check.profile),
    )
}

internal class VoiceHudView(
    context: Context,
    toggleDetails: Runnable,
    checkProfile: Runnable,
    sampleTasks: Runnable,
    analyzeTone: Runnable,
    clearTone: Runnable,
    openControls: Runnable,
    dismiss: Runnable,
) : FrameLayout(context) {
    init {
        inflate(context, R.layout.voice_hud, this)
        setOnApplyWindowInsetsListener { _, insets ->
            val safe = insets.getInsets(WindowInsets.Type.systemBars() or
                WindowInsets.Type.displayCutout() or WindowInsets.Type.ime())
            setPadding(safe.left, safe.top, safe.right, safe.bottom)
            insets
        }
    }

    private val panel: ScrollView = findViewById(R.id.hud_panel)
    private val track: VoiceHudTrack = findViewById(R.id.hud_track)
    private val status: TextView = findViewById(R.id.hud_status)
    private val details: View = findViewById(R.id.hud_details)
    private val result: TextView = findViewById(R.id.hud_result)
    private val toggle: ImageButton = findViewById(R.id.hud_toggle)
    private val check: Button = findViewById(R.id.hud_check)
    private val tasks: Button = findViewById(R.id.hud_tasks)
    private val taskStatus: TextView = findViewById(R.id.hud_task_status)
    private val scan: Button = findViewById(R.id.hud_tone_scan)
    private val clear: Button = findViewById(R.id.hud_tone_clear)
    private val graph: ToneGraph = findViewById(R.id.hud_tone_graph)
    private val reading: TextView = findViewById(R.id.hud_tone_status)
    private val position = IntArray(2)
    private val mic: Button = findViewById(R.id.hud_mic)
    private val title: TextView = findViewById(R.id.hud_title)
    private val micStatus: TextView = findViewById(R.id.hud_mic_status)
    private val micGraph: ToneGraph = findViewById(R.id.hud_mic_graph)
    private val visible = Rect()
    var onMicWindowLost: (() -> Unit)? = null

    fun setMicClick(click: () -> Unit) = mic.setOnClickListener { click() }

    fun micVisible(): Boolean = isAttachedToWindow && hasWindowFocus() && windowVisibility == VISIBLE &&
        mic.isShown && mic.getGlobalVisibleRect(visible) && visible.height() >= mic.height / 2

    override fun onWindowFocusChanged(hasWindowFocus: Boolean) {
        super.onWindowFocusChanged(hasWindowFocus)
        if (!hasWindowFocus) onMicWindowLost?.invoke()
    }

    override fun onWindowVisibilityChanged(visibility: Int) {
        super.onWindowVisibilityChanged(visibility)
        if (visibility != VISIBLE) onMicWindowLost?.invoke()
    }

    override fun onDetachedFromWindow() {
        onMicWindowLost?.invoke()
        super.onDetachedFromWindow()
    }

    fun renderMic(active: Boolean, levels: List<Int>, message: String?) {
        title.setText(if (active) R.string.voice_mic_title else R.string.voice_hud_title)
        mic.setText(if (active) R.string.voice_mic_stop else R.string.voice_mic_start)
        micStatus.text = message
        micStatus.visibility = if (message == null) GONE else VISIBLE
        micGraph.levels = levels
        micGraph.visibility = if (levels.isEmpty()) GONE else VISIBLE
    }

    init {
        toggle.setOnClickListener { toggleDetails.run() }
        check.setOnClickListener { checkProfile.run() }
        tasks.setOnClickListener { sampleTasks.run() }
        scan.setOnClickListener { analyzeTone.run() }
        clear.setOnClickListener { clearTone.run() }
        findViewById<Button>(R.id.hud_controls).setOnClickListener { openControls.run() }
        findViewById<ImageButton>(R.id.hud_end).setOnClickListener { dismiss.run() }
    }

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        val lp = panel.layoutParams as LayoutParams
        val available = MeasureSpec.getSize(widthMeasureSpec) - paddingLeft - paddingRight - lp.leftMargin - lp.rightMargin
        lp.width = minOf((360 * resources.displayMetrics.density).roundToInt(), available.coerceAtLeast(0))
        super.onMeasure(widthMeasureSpec, heightMeasureSpec)
    }

    fun render(frame: VoiceHudFrame?) {
        panel.visibility = if (frame == null) GONE else VISIBLE
        val expanded = frame?.expanded == true
        details.visibility = if (expanded) VISIBLE else GONE
        if (!expanded) panel.scrollTo(0, 0)
        toggle.rotation = if (expanded) 180f else 0f
        toggle.contentDescription = context.getString(if (expanded) R.string.voice_hud_collapse else R.string.voice_hud_expand)
        check.isEnabled = expanded && frame.check !is ProfileCheck.Requested
        val sample = frame?.tasks
        tasks.isEnabled = expanded && sample !is TaskSample.Request
        taskStatus.text = when (sample) {
            null -> null
            is TaskSample.Request -> context.getString(R.string.voice_tasks_reading)
            is TaskSample.Result -> sample.counts?.let {
                context.getString(R.string.voice_tasks_counts, it.running, it.retained)
            } ?: context.getString(R.string.voice_tasks_unavailable)
        }
        taskStatus.visibility = if (sample == null) GONE else VISIBLE
        track.check = frame?.check ?: ProfileCheck.Idle
        val notice = profileNotice(frame?.check, context::getString)
        status.text = notice.status
        result.text = notice.result
        result.visibility = if (notice.result == null) GONE else VISIBLE
        val tone = frame?.tone ?: ToneScan.Idle
        val levels = when (tone) {
            is ToneScan.Reading -> tone.levels
            is ToneScan.Complete -> tone.levels
            else -> emptyList()
        }
        graph.levels = levels
        graph.visibility = if (levels.isEmpty()) GONE else VISIBLE
        scan.isEnabled = expanded && tone !is ToneScan.Reading
        clear.visibility = if (tone == ToneScan.Idle) GONE else VISIBLE
        reading.text = when (tone) {
            ToneScan.Idle -> null
            is ToneScan.Reading -> context.getString(R.string.voice_tone_progress, levels.size, ToneLevels.LIMIT)
            is ToneScan.Complete -> context.getString(R.string.voice_tone_complete, levels.size, levels.max())
            ToneScan.Unavailable -> context.getString(R.string.voice_tone_failed)
        }
        reading.visibility = if (tone == ToneScan.Idle) GONE else VISIBLE
    }

    fun fillTouchableRegion(region: Region) {
        region.setEmpty()
        if (!panel.isShown || !panel.isLaidOut) return
        panel.getLocationInWindow(position)
        region.set(position[0], position[1], position[0] + panel.width, position[1] + panel.height)
    }
}

/** Bars use a fixed 0–100% full-scale RMS axis; silence is zero, not a decorative minimum. */
internal class ToneGraph(context: Context, attrs: AttributeSet) : View(context, attrs) {
    private val ink = Paint().apply { color = 0xff11665e.toInt() }
    var levels: List<Int> = emptyList()
        set(value) {
            if (value != field) {
                field = value
                invalidate()
            }
        }

    override fun onDraw(canvas: Canvas) {
        super.onDraw(canvas)
        val slot = width.toFloat() / ToneLevels.LIMIT
        levels.forEachIndexed { index, rms ->
            canvas.drawRect((index + 0.1f) * slot, height * (1f - rms / 100f),
                (index + 0.9f) * slot, height.toFloat(), ink)
        }
    }
}

/** Discrete milestones (session, request, result), not a waveform or an estimate of progress. */
internal class VoiceHudTrack(context: Context, attrs: AttributeSet) : View(context, attrs) {
    private val pen = Paint(Paint.ANTI_ALIAS_FLAG)
    var check: ProfileCheck = ProfileCheck.Idle
        set(value) {
            if (value != field) {
                field = value
                invalidate()
            }
        }

    override fun onDraw(canvas: Canvas) {
        val phase = check
        val reached = when (phase) {
            ProfileCheck.Idle -> 1
            is ProfileCheck.Requested -> 2
            is ProfileCheck.Complete, ProfileCheck.Unavailable -> 3
        }
        val y = height / 2f
        val radius = minOf(width / 12f, height / 4f)
        pen.strokeWidth = resources.displayMetrics.density * 1.5f
        for (i in 0..2) {
            val x = width * (2 * i + 1) / 6f
            pen.color = if (i < reached) 0xff11665e.toInt() else 0xff68787a.toInt()
            if (i != 0) canvas.drawLine(x - width / 3f + radius, y, x - radius, y, pen)
            if (i == 2 && phase == ProfileCheck.Unavailable) {
                pen.color = 0xff983b24.toInt()
                canvas.drawLine(x - radius, y - radius, x + radius, y + radius, pen)
                canvas.drawLine(x - radius, y + radius, x + radius, y - radius, pen)
                continue
            }
            pen.style = if (i < reached) Paint.Style.FILL else Paint.Style.STROKE
            canvas.drawCircle(x, y, radius, pen)
        }
    }
}
