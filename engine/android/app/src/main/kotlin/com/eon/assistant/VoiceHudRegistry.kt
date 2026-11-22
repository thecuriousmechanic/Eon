package com.eon.assistant

internal sealed interface ProfileCheck {
    data object Idle : ProfileCheck
    // Identity ties a result to one request in one showing of the HUD.
    class Requested : ProfileCheck
    data class Complete(val profile: String, val completedTasks: UInt) : ProfileCheck
    data object Unavailable : ProfileCheck
}

internal sealed interface ToneScan {
    data object Idle : ToneScan
    class Request
    data class Reading(val request: Request, val levels: List<Int> = emptyList()) : ToneScan
    data class Complete(val levels: List<Int>) : ToneScan
    data object Unavailable : ToneScan
}

internal data class VoiceHudFrame(val expanded: Boolean = false, val check: ProfileCheck = ProfileCheck.Idle,
                                 val tone: ToneScan = ToneScan.Idle, val tasks: TaskSample? = null)

internal sealed interface TaskSample {
    class Request : TaskSample
    data class Result(val counts: OwnActivityTasks.Counts?) : TaskSample
}

/** Main-thread only. A window belongs to one OS binding; neither survives a process restart. */
internal class VoiceHudRegistry {
    class Binding {
        var latestLocalShowId: Int? = null
        var preparedProfile: ProfileActionReview? = null
    }
    var binding: Binding? = null
        private set
    private var visible: Surface? = null

    fun ready(newBinding: Binding) {
        binding = newBinding
        revokeVisible()
    }

    fun shutdown(oldBinding: Binding) {
        if (binding !== oldBinding) return
        binding = null
        revokeVisible()
    }

    private fun revokeVisible() {
        val old = visible
        old?.retire()
        old?.onRevoked?.invoke()
    }

    inner class Surface(private val owner: Binding?, internal val onRevoked: () -> Unit) {
        private var retired = false
        var frame: VoiceHudFrame? = null
            private set

        fun show(userInvocation: Boolean, eligible: Boolean): Boolean {
            if (retired || owner == null || owner !== binding || !userInvocation || !eligible) {
                hide()
                return false
            }
            if (visible !== this) revokeVisible()
            visible = this
            frame = VoiceHudFrame()
            return true
        }

        fun canUse(eligible: Boolean): Boolean = visible === this && owner === binding && eligible

        fun toggleDetails() {
            frame = frame?.let { it.copy(expanded = !it.expanded, tone = ToneScan.Idle, tasks = null) }
        }

        fun requestTasks(): TaskSample.Request? {
            val current = frame ?: return null
            if (!current.expanded || current.tasks is TaskSample.Request) return null
            return TaskSample.Request().also { frame = current.copy(tasks = it) }
        }

        fun finishTasks(request: TaskSample.Request, counts: OwnActivityTasks.Counts?) {
            val current = frame ?: return
            if (current.tasks === request) frame = current.copy(tasks = TaskSample.Result(counts))
        }

        fun requestTone(): ToneScan.Request? {
            val current = frame ?: return null
            if (!current.expanded || current.tone is ToneScan.Reading) return null
            return ToneScan.Request().also { frame = current.copy(tone = ToneScan.Reading(it)) }
        }

        fun addLevel(request: ToneScan.Request, level: Int) {
            val current = frame ?: return
            val reading = current.tone as? ToneScan.Reading ?: return
            if (reading.request === request && level in 0..100 && reading.levels.size < ToneLevels.LIMIT) {
                frame = current.copy(tone = reading.copy(levels = reading.levels + level))
            }
        }

        fun finishTone(request: ToneScan.Request, ok: Boolean) {
            val current = frame ?: return
            val reading = current.tone as? ToneScan.Reading ?: return
            if (reading.request === request) {
                frame = current.copy(tone = if (ok && reading.levels.isNotEmpty())
                    ToneScan.Complete(reading.levels) else ToneScan.Unavailable)
            }
        }

        fun clearTone() {
            frame = frame?.copy(tone = ToneScan.Idle)
        }

        fun requestCheck(): ProfileCheck.Requested? {
            val current = frame ?: return null
            if (!current.expanded || current.check is ProfileCheck.Requested) return null
            return ProfileCheck.Requested().also { frame = current.copy(check = it) }
        }

        fun acceptResult(request: ProfileCheck.Requested, completion: ProfileCheck.Complete?) {
            val current = frame ?: return
            if (current.check === request) {
                frame = current.copy(check = completion ?: ProfileCheck.Unavailable)
            }
        }

        fun consumeAction(eligible: Boolean): Boolean {
            val allowed = canUse(eligible)
            hide()
            return allowed
        }

        fun hide() {
            if (visible === this) visible = null
            frame = null
        }

        fun retire() {
            retired = true
            hide()
        }
    }
}
