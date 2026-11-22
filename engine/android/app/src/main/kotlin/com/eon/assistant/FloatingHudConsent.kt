package com.eon.assistant

import java.util.UUID

/** Main-thread, process-local. OS grants are prerequisites, never start commands. */
internal class FloatingHudConsent {
    private var pending: String? = null
    private var source: Any? = null
    var active: String? = null
        private set
    val startedOrPending get() = pending != null || active != null

    fun request(eligible: Boolean, requester: Any): String? {
        if (!eligible || startedOrPending) return null
        source = requester
        return UUID.randomUUID().toString().also { pending = it }
    }

    fun cancelPending(requester: Any) {
        if (source !== requester) return
        pending = null
        source = null
    }

    fun sourceFor(ticket: String?) = if (ticket != null && pending == ticket) source else null

    fun consume(ticket: String?, eligible: Boolean): Boolean {
        if (ticket == null || ticket != pending) return false
        pending = null
        source = null
        if (!eligible) return false
        active = ticket
        return true
    }

    fun accepts(ticket: String?) = ticket != null && active == ticket

    fun end() {
        pending = null
        source = null
        active = null
    }
}
