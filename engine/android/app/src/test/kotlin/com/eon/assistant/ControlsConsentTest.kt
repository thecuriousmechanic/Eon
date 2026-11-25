package com.eon.assistant

import java.io.File
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertSame
import kotlin.test.assertTrue

class ControlsConsentTest {
    private var now = 0L
    private val gate = ControlsConsent { now }

    @Test fun onlyTheSameFreshCapabilityAndReviewedValueCanBeConsumedOnce() {
        val source = Any()
        val old = gate.issue(source, Any(), true)
        val fresh = gate.issue(source, Any(), false)
        gate.cancel(old)
        assertFalse(fresh.timing)
        assertSame(fresh, gate.take(fresh.token))
        assertNull(gate.take(fresh.token))
        val wrongToken = gate.issue(source, Any(), false)
        assertNull(gate.take(Any()))
        assertNull(gate.take(wrongToken.token))
        val timed = gate.issue(source, Any(), true)
        assertTrue(timed.timing)
        assertSame(timed, gate.take(timed.token))
    }

    @Test fun expiryRevocationAndRestartAllForgetTheCapability() {
        val ticket = gate.issue(Any(), Any(), false)
        now = ticket.until
        assertNull(gate.take(ticket.token))
        val next = gate.issue(Any(), Any(), false)
        gate.reset()
        assertNull(gate.take(next.token))
        val last = gate.issue(Any(), Any(), true)
        assertNull(ControlsConsent { now }.take(last.token))
    }

    @Test fun ownerContextRequiresBothUserAndAdminApprovalAndFailsClosedOnUnavailablePolicy() {
        for (mask in 0..3) for (restricted in listOf(false, true, null)) for (disabled in listOf(false, true, null)) {
            assertEquals(mask and 1 == 0 && restricted == false && disabled == false,
                ownedContextAllowed({ mask }, { restricted }, { disabled }))
        }
        assertFalse(ownedContextAllowed({ throw SecurityException() }, { false }, { false }))
        assertFalse(ownedContextAllowed({ 0 }, { throw SecurityException() }, { false }))
        assertFalse(ownedContextAllowed({ 0 }, { false }, { throw IllegalStateException() }))
    }

    @Test fun noProductionPathOpensTheBindingWideCollectionPolicy() {
        val files = File("src/main").walk().filter { it.extension in listOf("java", "kt") }.toList()
        val writes = Regex("""setDisabledShowContext\s*\(\s*([^)]*?)\s*\)""")
        val calls = files.flatMap { f -> writes.findAll(f.readText()).map { f.name to it.groupValues[1] } }
        assertEquals(listOf("EonVoiceInteractionService.kt" to "NO_ASSIST_CONTEXT"), calls)
        assertEquals(3, NO_ASSIST_CONTEXT)
        for (f in files) {
            assertFalse(Regex("""\bdisabledShowContext\s*=(?!=)""").containsMatchIn(f.readText()), f.name)
        }
    }
}
