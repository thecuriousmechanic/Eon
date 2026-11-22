package com.eon.assistant

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue

class OneShotInspectionTest {
    private var now = 100L
    private val session = OneShotInspection { now }
    private var reads = 0
    private fun read(): ControlKind { reads++; return ControlKind.BUTTON }
    private fun tap(eligible: Boolean = true) = session.inspectOnShortcut({ eligible }, ::read)

    @Test
    fun enablingTheOSCapabilityIsNotConsent() {
        assertFalse(tap())
        assertFalse(session.allowNextShortcut(false))
        assertFalse(tap())
        assertNull(session.takeResult(true))
        assertEquals(0, reads)
    }

    @Test
    fun aConfirmationNeedsAnotherTapAndOnlyOpensOneNodeAndOneResult() {
        assertTrue(session.allowNextShortcut(true))
        assertEquals(0, reads)
        now += OneShotInspection.LIFETIME_MS - 1
        assertTrue(tap())
        assertFalse(tap())
        assertEquals(ControlKind.BUTTON, session.takeResult(true))
        assertNull(session.takeResult(true))
        assertFalse(tap())
        assertEquals(1, reads)
    }

    @Test
    fun expirationConsumesTheGrantWithoutTouchingTheNode() {
        session.allowNextShortcut(true)
        now += OneShotInspection.LIFETIME_MS
        assertFalse(tap())
        assertEquals(0, reads)
        assertTrue(session.allowNextShortcut(true))
        assertTrue(tap())
        now += OneShotInspection.LIFETIME_MS
        assertNull(session.takeResult(true))
    }

    @Test
    fun losingEligibilityBetweenConsentAndUseOrDisplayCannotResumeTheGrant() {
        session.allowNextShortcut(true)
        assertFalse(tap(eligible = false))
        assertFalse(tap())
        assertEquals(0, reads)
        session.allowNextShortcut(true)
        assertTrue(tap())
        assertNull(session.takeResult(false))
        assertNull(session.takeResult(true))
    }

    @Test
    fun explicitCancellationBeforeTapOrAfterReadErasesAuthorityAndResult() {
        session.allowNextShortcut(true)
        session.revoke()
        assertFalse(tap())
        assertEquals(0, reads)
        session.allowNextShortcut(true)
        assertTrue(tap())
        session.revoke()
        assertNull(session.takeResult(true))
    }

    @Test
    fun expiryRevocationOrRoleLossDuringReadDiscardsTheResult() {
        session.allowNextShortcut(true)
        assertFalse(session.inspectOnShortcut({ true }) {
            now += OneShotInspection.LIFETIME_MS
            read()
        })
        assertNull(session.takeResult(true))
        session.allowNextShortcut(true)
        assertFalse(session.inspectOnShortcut({ true }) {
            session.revoke()
            read()
        })
        assertNull(session.takeResult(true))
        var eligible = true
        session.allowNextShortcut(true)
        assertFalse(session.inspectOnShortcut({ eligible }) {
            eligible = false
            read()
        })
        assertNull(session.takeResult(true))
    }

    @Test
    fun failingOrReentrantPlatformCallsDoNotCreateAnotherAttempt() {
        session.allowNextShortcut(true)
        assertFailsWith<IllegalStateException> {
            session.inspectOnShortcut({ true }) {
                assertFalse(tap())
                error("Node disappeared")
            }
        }
        assertFalse(tap())
        assertNull(session.takeResult(true))
        assertEquals(0, reads)
    }

    @Test
    fun aFreshProcessHasNeitherThePreviousConsentNorItsResult() {
        session.allowNextShortcut(true)
        val restarted = OneShotInspection { now }
        assertFalse(restarted.inspectOnShortcut({ true }, ::read))
        assertEquals(0, reads)
        assertTrue(tap())
        assertNull(OneShotInspection { now }.takeResult(true))
    }

    @Test
    fun redactionPrecedesTheAllowlistAndUnknownTypesAreNeverEchoed() {
        fun classify(
            visible: Boolean = true, focused: Boolean = true, sensitive: Boolean = false,
            password: Boolean = false, editable: Boolean = false, input: Int = 0,
            type: String? = "android.widget.TextView",
        ) = ControlKind.classify(visible, focused, sensitive, password, editable, input) { type }

        assertEquals(ControlKind.TEXT, classify())
        assertEquals(ControlKind.BUTTON, classify(type = "android.widget.ImageButton"))
        for (blocked in listOf(
            classify(visible = false), classify(focused = false), classify(sensitive = true),
            classify(password = true), classify(editable = true), classify(input = 0x81),
            classify(input = 0x12), classify(type = "android.widget.EditText"),
            classify(type = "private.provider.accountLabel"), classify(type = null),
        )) assertEquals(ControlKind.UNAVAILABLE, blocked)
        assertEquals(ControlKind.UNAVAILABLE,
            ControlKind.classify(true, true, true, false, false, 0) { error("Sensitive node's class was read") })
    }
}
