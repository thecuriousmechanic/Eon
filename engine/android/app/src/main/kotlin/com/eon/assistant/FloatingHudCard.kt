package com.eon.assistant

import android.content.Context
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.input.InputManager
import android.view.ContextThemeWrapper
import android.view.Display
import android.view.Gravity
import android.view.LayoutInflater
import android.view.View
import android.view.WindowManager
import android.widget.TextView

/** One small passive window on the default display. No input channel that can target controls. */
internal class FloatingHudCard(context: Context) {
    private val type = WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY
    private val display = checkNotNull(context.getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY))
    private val ui = ContextThemeWrapper(context.createDisplayContext(display).createWindowContext(type, null),
        android.R.style.Theme_Material_Light)
    private val windows = ui.getSystemService(WindowManager::class.java)
    private val card = LayoutInflater.from(ui).inflate(R.layout.floating_hud, null)
    private val status: TextView = card.findViewById(R.id.floating_status)
    private val track: VoiceHudTrack = card.findViewById(R.id.floating_track)
    private val meter: ToneGraph = card.findViewById(R.id.floating_meter)
    private var attached = false

    fun show() {
        if (attached) return
        val max = ui.getSystemService(InputManager::class.java).maximumObscuringOpacityForTouch
        check(max > 0f)
        val p = WindowManager.LayoutParams(
            WindowManager.LayoutParams.WRAP_CONTENT, WindowManager.LayoutParams.WRAP_CONTENT, type,
            WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE or
                WindowManager.LayoutParams.FLAG_SECURE, PixelFormat.TRANSLUCENT,
        ).apply {
            gravity = Gravity.END or Gravity.CENTER_VERTICAL
            // Android 12+ blocks touches through untrusted overlays above its opacity limit.
            alpha = minOf(0.72f, max)
            title = "EON floating HUD"
        }
        windows.addView(card, p)
        attached = true
    }

    fun hide() {
        if (!attached) return
        attached = false
        windows.removeViewImmediate(card)
    }

    fun render(check: ProfileCheck, tone: ToneScan, micActive: Boolean, micLevels: List<Int>) {
        track.check = check
        val levels = if (micActive || micLevels.isNotEmpty()) micLevels else when (tone) {
            is ToneScan.Reading -> tone.levels
            is ToneScan.Complete -> tone.levels
            else -> emptyList()
        }
        meter.levels = levels
        meter.visibility = if (levels.isEmpty()) View.GONE else View.VISIBLE
        val message = if (micActive) micLevels.lastOrNull()?.let { ui.getString(R.string.floating_mic_level, it) }
            ?: ui.getString(R.string.floating_mic_on)
        else if (micLevels.isNotEmpty()) ui.getString(R.string.floating_mic_done, micLevels.max())
        else when (tone) {
            is ToneScan.Reading -> ui.getString(R.string.floating_tone_reading)
            is ToneScan.Complete -> ui.getString(R.string.floating_tone_levels, levels.max())
            ToneScan.Unavailable -> ui.getString(R.string.voice_tone_failed)
            ToneScan.Idle -> if (check === ProfileCheck.Idle) ui.getString(R.string.floating_idle)
                else profileNotice(check, ui::getString).status
        }
        if (status.text != message) status.text = message
    }
}
