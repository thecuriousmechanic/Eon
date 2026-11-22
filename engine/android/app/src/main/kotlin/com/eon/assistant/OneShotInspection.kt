package com.eon.assistant

/** Only this closed vocabulary may leave the inspection callback. There is no field for text or IDs. */
internal enum class ControlKind(val label: String) {
    BUTTON("Button"), IMAGE("Image"), TEXT("Read-only text"), PROGRESS("Progress indicator"),
    UNAVAILABLE("No inspectable focused control");

    companion object {
        fun classify(
            visible: Boolean, accessibilityFocused: Boolean, sensitive: Boolean,
            password: Boolean, editable: Boolean, inputType: Int, className: () -> CharSequence?,
        ): ControlKind {
            if (!visible || !accessibilityFocused || sensitive || password || editable || inputType != 0) {
                return UNAVAILABLE
            }
            return when (className()) {
                "android.widget.Button", "android.widget.ImageButton" -> BUTTON
                "android.widget.ImageView" -> IMAGE
                "android.widget.TextView" -> TEXT
                "android.widget.ProgressBar" -> PROGRESS
                else -> UNAVAILABLE
            }
        }
    }
}

/** Main-thread confined. Nothing here is saved in a Bundle, preference, file or service intent. */
internal class OneShotInspection(private val elapsedMillis: () -> Long) {
    companion object {
        const val LIFETIME_MS = 30_000L
    }

    private sealed interface State
    private object Idle : State
    private class Armed(val until: Long) : State
    private class Reading : State
    private class Result(val kind: ControlKind, val until: Long) : State
    private var state: State = Idle

    fun revoke() { state = Idle }

    fun allowNextShortcut(eligible: Boolean, confirmedAt: Long = elapsedMillis()): Boolean {
        val until = confirmedAt + LIFETIME_MS
        val allowed = eligible && elapsedMillis() < until
        state = if (allowed) Armed(until) else Idle
        return allowed
    }

    fun inspectOnShortcut(eligible: () -> Boolean, readFocused: () -> ControlKind): Boolean {
        val grant = state as? Armed ?: return false
        val reading = Reading()
        state = reading // Consume before calling the platform, including on failure or a second tap.
        try {
            if (elapsedMillis() >= grant.until || !eligible()) return false
            val kind = readFocused()
            if (state !== reading || elapsedMillis() >= grant.until || !eligible()) return false
            state = Result(kind, elapsedMillis() + LIFETIME_MS)
            return true
        } finally {
            if (state === reading) revoke()
        }
    }

    fun takeResult(eligible: Boolean): ControlKind? {
        val result = state as? Result ?: return null
        revoke()
        return result.kind.takeIf { eligible && elapsedMillis() < result.until }
    }
}
