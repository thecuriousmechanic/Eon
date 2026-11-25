package com.eon.assistant

import android.app.Activity
import android.os.Bundle
import android.view.WindowManager
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView

class InspectionResultActivity : Activity() {
    private lateinit var result: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        val kind = InspectionAccess.takeResult(InspectionAccess.eligible(this))
        if (kind == null || savedInstanceState != null) {
            finish()
            return
        }
        title = "EON inspection result"
        result = TextView(this).apply {
            text = "${kind.label}.\nInspection has ended. No text or screen image was read."
        }
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            val pad = (16 * resources.displayMetrics.density).toInt()
            setPadding(pad, pad, pad, pad)
            addView(result)
            addView(Button(context).apply {
                text = "Dismiss"
                setOnClickListener { finish() }
            })
        })
    }

    override fun onPause() {
        if (::result.isInitialized) result.text = ""
        finish()
        super.onPause()
    }
}
