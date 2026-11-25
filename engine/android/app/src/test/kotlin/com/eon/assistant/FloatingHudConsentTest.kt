package com.eon.assistant

import kotlin.test.Test
import kotlin.test.assertFalse
import kotlin.test.assertNotEquals
import kotlin.test.assertNotNull
import kotlin.test.assertNull
import kotlin.test.assertTrue

class FloatingHudConsentTest {
    private val consent = FloatingHudConsent()
    private val controls = Any()

    @Test fun grantsDoNotStartAndPermissionMustStillHoldOnDelivery() {
        assertNull(consent.request(false, controls))
        assertFalse(consent.consume(null, true))
        assertFalse(consent.consume("notification", true))
        val ticket = assertNotNull(consent.request(true, controls))
        assertFalse(consent.accepts(ticket))
        assertFalse(consent.consume(ticket, false))
        assertFalse(consent.consume(ticket, true))
        assertFalse(consent.startedOrPending)
    }

    @Test fun stopWhileStartIsQueuedPreventsALateWindow() {
        val old = assertNotNull(consent.request(true, controls))
        consent.end()
        val current = assertNotNull(consent.request(true, controls))
        assertNotEquals(old, current)
        assertFalse(consent.consume(old, true))
        assertTrue(consent.consume(current, true))
        assertFalse(consent.accepts(old))
        assertTrue(consent.accepts(current))
    }

    @Test fun onlyOneRunAndNeitherRedeliveryNorOldNoticeActionsCanRestartIt() {
        val ticket = assertNotNull(consent.request(true, controls))
        assertNull(consent.request(true, controls))
        assertTrue(consent.consume(ticket, true))
        assertFalse(consent.consume(ticket, true))
        assertNull(consent.request(true, controls))
        consent.end()
        assertFalse(consent.accepts(ticket))
        assertFalse(consent.consume(ticket, true))
        val afterProcessDeath = FloatingHudConsent()
        assertFalse(afterProcessDeath.accepts(ticket))
        assertFalse(afterProcessDeath.consume(ticket, true))
    }

    @Test fun lostFocusBeforeDeliveryInvalidatesTicketEvenAfterResumingUnlocked() {
        val old = assertNotNull(consent.request(true, controls))
        consent.cancelPending(controls)
        assertNull(consent.sourceFor(old))
        assertFalse(consent.consume(old, true))
        val replacement = Any()
        val fresh = assertNotNull(consent.request(true, replacement))
        consent.cancelPending(controls)
        assertTrue(consent.sourceFor(fresh) === replacement)
        assertTrue(consent.consume(fresh, true))
        consent.cancelPending(replacement) // ordinary Home after successful Start
        assertTrue(consent.accepts(fresh))
    }
}
