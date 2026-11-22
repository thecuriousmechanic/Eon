package com.eon.assistant

import android.app.Activity
import android.app.ActivityManager
import android.app.AlertDialog
import android.app.role.RoleManager
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.view.ViewGroup
import android.view.WindowManager
import android.widget.Button
import android.widget.CheckBox
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import uniffi.eon_uniffi.BridgeException
import uniffi.eon_uniffi.selectModelProfile

internal const val ASSISTANT_ROLE_REQUEST = 1

/** ACTION_ASSIST launches a normal, foreground activity. It doesn't inspect the incoming intent. */
class AssistantActivity : Activity() {
    private lateinit var measureNextProfile: CheckBox
    private var measureUidTraffic: CheckBox? = null
    private lateinit var roleStatus: TextView
    private lateinit var requestRole: Button
    private lateinit var inspectionStatus: TextView
    private var consent: AlertDialog? = null
    private var resumed = false
    private var selectionCanceled = false
    private var micPermissionPending = false
    private var micPermissionStatus: TextView? = null
    private val contextSource = Any()
    private var contextReview: Button? = null
    private var reviewingTiming: Boolean? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        if (Build.VERSION.SDK_INT >= 31) window.setHideOverlayWindows(true)
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            val pad = (16 * resources.displayMetrics.density).toInt()
            setPadding(pad, pad, pad, pad)
        }
        val result = TextView(this)
        column.addView(TextView(this).apply {
            text = "EON local assistant\nScreen and microphone capture are off. Choosing EON as assistant does not allow inspection."
        })
        measureNextProfile = CheckBox(this).apply {
            text = "Measure EON process CPU during the next profile check"
            isSaveEnabled = false
            filterTouchesWhenObscured = true
            setOnCheckedChangeListener { _, _ -> resetContextReview() }
        }
        column.addView(measureNextProfile)
        if (Build.VERSION.SDK_INT >= 35) {
            measureUidTraffic = CheckBox(this).apply {
                text = "Compare EON’s platform UID traffic counters during the next profile check"
                isSaveEnabled = false
                filterTouchesWhenObscured = true
            }.also(column::addView)
        }
        column.addView(Button(this).apply {
            text = "Check model profile"
            filterTouchesWhenObscured = true
            setOnClickListener {
                val cpu = measureNextProfile.isChecked
                val traffic = measureUidTraffic?.isChecked == true && resumed && hasWindowFocus()
                measureNextProfile.isChecked = false
                measureUidTraffic?.isChecked = false
                val delta = SelfUidTraffic.measure(traffic) { checkModelProfile(cpu, result) }
                if (traffic) {
                    result.append(if (delta.isPresent)
                        "\nPlatform UID counter change: ${delta.get().rxBytes} B received, ${delta.get().txBytes} B sent." +
                            " Android may cache these totals; they cover the whole UID, not just this action."
                        else "\nPlatform UID counters unavailable or no established baseline.")
                }
            }
        })
        column.addView(result)
        if (Build.VERSION.SDK_INT >= 31) {
            column.addView(Button(this).apply {
                setText(R.string.floating_controls)
                filterTouchesWhenObscured = true
                setOnClickListener {
                    if (resumed && hasWindowFocus()) startActivity(Intent(this@AssistantActivity, FloatingHudActivity::class.java))
                }
            })
        }
        roleStatus = TextView(this).apply { id = R.id.assistant_role_status; isSaveEnabled = false }
        requestRole = Button(this).apply {
            id = R.id.assistant_role_request
            text = "Choose EON as assistant"
            filterTouchesWhenObscured = true
            setOnClickListener {
                if (Build.VERSION.SDK_INT >= 29) {
                    val role = getSystemService(RoleManager::class.java)
                    if (role.isRoleAvailable(RoleManager.ROLE_ASSISTANT) && !role.isRoleHeld(RoleManager.ROLE_ASSISTANT)) {
                        try {
                            selectionCanceled = false
                            startActivityForResult(role.createRequestRoleIntent(RoleManager.ROLE_ASSISTANT), ASSISTANT_ROLE_REQUEST)
                        } catch (_: ActivityNotFoundException) {
                            roleStatus.text = "The system assistant picker is unavailable."
                        } catch (_: SecurityException) {
                            roleStatus.text = "Assistant selection is restricted on this device."
                        }
                    }
                }
            }
        }
        column.addView(roleStatus)
        column.addView(requestRole)
        if (Build.VERSION.SDK_INT >= 35) {
            column.addView(Button(this).apply {
                id = R.id.projection_grant_controls
                setText(R.string.projection_controls)
                filterTouchesWhenObscured = true
                setOnClickListener {
                    if (resumed && hasWindowFocus()) {
                        ProjectionGrantActivity.open(this@AssistantActivity)
                    }
                }
            })
        }
        if (Build.VERSION.SDK_INT >= 31) {
            contextReview = Button(this).apply {
                id = R.id.assistant_context_review
                filterTouchesWhenObscured = true
                isSaveEnabled = false
                setText(R.string.voice_context_review)
                setOnClickListener {
                    val timing = measureNextProfile.isChecked
                    if (!resumed || !hasWindowFocus() || !selectedAndUnlocked(this@AssistantActivity)) {
                        resetContextReview()
                    } else if (reviewingTiming != timing) {
                        reviewingTiming = timing
                        setText(if (timing) R.string.voice_context_confirm_on else R.string.voice_context_confirm_off)
                    } else {
                        resetContextReview()
                        if (!ControlsAssist.request(this@AssistantActivity, contextSource, timing)) {
                            roleStatus.setText(R.string.voice_context_unavailable)
                        }
                    }
                }
            }.also(column::addView)
            column.addView(Button(this).apply {
                id = R.id.profile_action_open
                setText(R.string.profile_action_open)
                filterTouchesWhenObscured = true
                setOnClickListener {
                    if (resumed && hasWindowFocus())
                        startActivity(Intent(this@AssistantActivity, ProfileActionActivity::class.java))
                }
            })
            column.addView(TextView(this).apply { setText(R.string.voice_hud_help) })
            micPermissionStatus = TextView(this).also(column::addView)
            column.addView(Button(this).apply {
                text = getString(R.string.voice_mic_permission)
                filterTouchesWhenObscured = true
                setOnClickListener {
                    if (!resumed || !hasWindowFocus() || micPermissionPending) return@setOnClickListener
                    if (MicPermission.granted(this@AssistantActivity)) {
                        updateMicPermissionStatus()
                    } else {
                        micPermissionPending = true
                        requestPermissions(arrayOf(MicPermission.NAME), MIC_REQUEST)
                    }
                }
            })
        }
        column.addView(Button(this).apply {
            text = "Manage default apps in Settings"
            setOnClickListener { openSettings(Settings.ACTION_MANAGE_DEFAULT_APPS_SETTINGS) }
        })
        inspectionStatus = TextView(this)
        column.addView(inspectionStatus)
        if (Build.VERSION.SDK_INT >= 34) {
            column.addView(TextView(this).apply {
                text = "Optional: in Android Accessibility settings, enable EON focused control and assign its accessibility shortcut. Enabling it alone does not read controls. To revoke access, turn the service off there."
            })
            column.addView(Button(this).apply {
                text = "Open Accessibility settings"
                setOnClickListener { openSettings(Settings.ACTION_ACCESSIBILITY_SETTINGS) }
            })
            column.addView(Button(this).apply {
                text = "Review one-time inspection"
                filterTouchesWhenObscured = true
                setOnClickListener { reviewInspection() }
            })
            column.addView(Button(this).apply {
                text = "Open EON accessibility action demo"
                setOnClickListener { startActivity(Intent(this@AssistantActivity, DemoActionActivity::class.java)) }
            })
        }
        setContentView(ScrollView(this).apply {
            fitsSystemWindows = true
            addView(column, ViewGroup.LayoutParams(-1, -2))
        })
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == ASSISTANT_ROLE_REQUEST) {
            // This result is UI feedback, never authority. onResume re-reads the role from Android.
            selectionCanceled = resultCode == RESULT_CANCELED
        }
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, results)
        if (requestCode != MIC_REQUEST || !micPermissionPending) return
        micPermissionPending = false
        val approved = permissions.singleOrNull() == MicPermission.NAME &&
            results.singleOrNull() == PackageManager.PERMISSION_GRANTED && MicPermission.granted(this)
        MicPermission.reviewed = approved
        updateMicPermissionStatus()
    }

    private fun resetContextReview() {
        reviewingTiming = null
        contextReview?.setText(R.string.voice_context_review)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus) {
            resetContextReview()
            measureUidTraffic?.isChecked = false
        }
        if (Build.VERSION.SDK_INT >= 35) ProjectionGrantActivity.launcherFocus(this, hasFocus)
    }

    internal val projectionForeground get() = resumed && hasWindowFocus() && !isInMultiWindowMode && !isInPictureInPictureMode

    private fun updateMicPermissionStatus() {
        micPermissionStatus?.setText(when {
            MicPermission.allowed(this) -> R.string.voice_mic_prepared
            MicPermission.granted(this) -> R.string.voice_mic_pregranted
            else -> R.string.voice_mic_unprepared
        })
    }

    private companion object { const val MIC_REQUEST = 2 }

    private fun checkModelProfile(cpu: Boolean, result: TextView) {
        val delta = SelfProcessCpu.measure(cpu) {
            val info = ActivityManager.MemoryInfo()
            getSystemService(ActivityManager::class.java).getMemoryInfo(info)
            result.text = try {
                "Model profile: ${selectModelProfile(info.totalMem / 1_000_000_000.0)}"
            } catch (_: BridgeException) {
                "Could not select a profile for this device."
            } catch (_: LinkageError) {
                "The native bridge could not be loaded."
            }
        }
        if (cpu) {
            result.append(if (delta.isPresent) "\nEON process CPU: ${delta.asLong} ms."
                          else "\nEON process CPU unavailable.")
        }
    }

    private fun openSettings(action: String) {
        try {
            startActivity(Intent(action))
        } catch (_: ActivityNotFoundException) {
            roleStatus.text = "This Settings page is unavailable."
        } catch (_: SecurityException) {
            roleStatus.text = "Settings access is restricted on this device."
        }
    }

    private fun reviewInspection() {
        InspectionAccess.cancel()
        consent?.dismiss()
        consent = null
        if (!InspectionAccess.eligible(this)) {
            inspectionStatus.text = "Inspection is off. Unlock the device, select EON as assistant, and enable its accessibility service and shortcut first."
            return
        }
        var confirmed = false
        val dialog = AlertDialog.Builder(this)
            .setTitle("Allow one focused control inspection?")
            .setMessage("EON will close. Focus the control you choose with accessibility, then tap EON’s accessibility shortcut within 30 seconds. EON shows only its kind (such as Button), not its label, contents or value. Hidden, sensitive and editable controls are skipped. Reopen EON to cancel.")
            .setNegativeButton("Cancel", null)
            .setPositiveButton("Allow next shortcut") { source, _ ->
                if (source !== consent || !resumed) return@setPositiveButton
                confirmed = true
                inspectionStatus.text = "Checking approval. No control has been read."
                InspectionAccess.confirm({ resumed && InspectionAccess.eligible(this) }) { allowed ->
                    if (allowed) finish() else inspectionStatus.text = "Inspection unavailable. No control was read."
                }
            }
            .create()
        consent = dialog
        // Negative button, back, outside tap, or other dismissal never leaves an approval alive.
        // The positive button also auto-dismisses, but its still-foreground proof may complete.
        dialog.setOnDismissListener {
            if (consent === dialog) {
                consent = null
                if (!confirmed) InspectionAccess.cancel()
            }
        }
        dialog.window?.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        if (Build.VERSION.SDK_INT >= 31) dialog.window?.setHideOverlayWindows(true)
        dialog.show()
        dialog.getButton(AlertDialog.BUTTON_POSITIVE).filterTouchesWhenObscured = true
    }

    override fun onResume() {
        super.onResume()
        resumed = true
        updateMicPermissionStatus()
        // Returning here cancels any outstanding shortcut, including after Settings or recreation.
        InspectionAccess.cancel()
        inspectionStatus.text = if (Build.VERSION.SDK_INT >= 34) {
            "Inspection is off. No context is being read."
        } else {
            "Inspection is unavailable. Android 14 or newer is required to exclude sensitive controls."
        }
        if (Build.VERSION.SDK_INT < 29) {
            roleStatus.text = "Assistant selection is managed in system settings."
            requestRole.isEnabled = false
            return
        }
        val role = getSystemService(RoleManager::class.java)
        val available = role.isRoleAvailable(RoleManager.ROLE_ASSISTANT)
        val held = available && role.isRoleHeld(RoleManager.ROLE_ASSISTANT)
        roleStatus.text = when {
            held -> "EON is the selected assistant."
            available && selectionCanceled -> getString(R.string.voice_role_canceled)
            available -> "EON is not the selected assistant."
            else -> "This device does not offer an assistant role."
        }
        requestRole.isEnabled = available && !held
        contextReview?.isEnabled = held
    }

    override fun onPause() {
        resumed = false
        if (Build.VERSION.SDK_INT >= 31) ControlsAssist.cancelSource(contextSource)
        resetContextReview()
        measureNextProfile.isChecked = false
        measureUidTraffic?.isChecked = false
        InspectionAccess.cancelPendingConfirmation()
        consent?.dismiss()
        consent = null
        super.onPause()
    }
}
