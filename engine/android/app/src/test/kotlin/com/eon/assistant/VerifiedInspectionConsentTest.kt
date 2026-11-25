package com.eon.assistant

import java.util.concurrent.Executor
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue

/** Deterministic main/worker ordering, not a mock provider or evidence about AndroidKeyStore. */
class VerifiedInspectionConsentTest {
    private class Queue : Executor {
        private val tasks = ArrayDeque<Runnable>()
        override fun execute(command: Runnable) { tasks.addLast(command) }
        fun runNext() { tasks.removeFirst().run() }
        fun empty() = tasks.isEmpty()
    }

    private val worker = Queue()
    private val main = Queue()
    private var now = 0L
    private var eligible = true
    private var verify: () -> Boolean = { true }
    private val gate = VerifiedInspectionConsent({ now }, worker, main) { verify() }
    private val outcomes = mutableListOf<Boolean>()
    private fun confirm() = gate.confirm({ eligible }) { outcomes += it }
    private fun finishWork() { worker.runNext(); main.runNext() }
    private fun noRead() = assertFalse(gate.inspect({ true }) { error("Focus accessed without consent") })

    @Test fun onlyVerifiedApprovalReadsTheCurrentFocusSynchronouslyAndOnce() {
        noRead()
        confirm()
        noRead()
        worker.runNext()
        noRead()
        main.runNext()
        assertEquals(listOf(true), outcomes)
        var focused = ControlKind.BUTTON
        var reads = 0
        assertTrue(gate.inspect({ true }) { reads++; noRead(); focused })
        focused = ControlKind.IMAGE
        assertEquals(1, reads)
        noRead()
        assertEquals(ControlKind.BUTTON, gate.takeResult(true))
        assertNull(gate.takeResult(true))
        assertTrue(worker.empty() && main.empty())
    }

    @Test fun openingThenCancellingAnotherReviewCannotPublishAnEarlierSuccess() {
        confirm()
        worker.runNext()
        gate.revoke() // New review opened.
        gate.revoke() // Dismissed without confirming.
        main.runNext()
        assertTrue(outcomes.isEmpty())
        noRead()
    }

    @Test fun secondConfirmationWhileStaleWorkDrainsFailsInForeground() {
        confirm()
        gate.revoke()
        confirm()
        assertEquals(listOf(false), outcomes)
        finishWork()
        assertEquals(listOf(false), outcomes)
        noRead()
    }

    @Test fun providerRejectionAndExceptionAlwaysDeliverDenialToCurrentReview() {
        for (f in listOf<() -> Boolean>({ false }, { error("provider unavailable") })) {
            verify = f
            confirm()
            finishWork()
            noRead()
        }
        assertEquals(listOf(false, false), outcomes)
    }

    @Test fun expiryAndEligibilityLossDuringVerificationDenyWithoutReading() {
        confirm()
        now += OneShotInspection.LIFETIME_MS
        finishWork()
        noRead()
        confirm()
        eligible = false
        finishWork()
        noRead()
        assertEquals(listOf(false, false), outcomes)
    }

    @Test fun pausingPendingConfirmationOrRevokingAnArmedPermitPreventsReads() {
        confirm()
        gate.cancelPending()
        finishWork()
        noRead()
        assertTrue(outcomes.isEmpty())
        confirm()
        finishWork()
        assertEquals(listOf(true), outcomes)
        gate.revoke()
        noRead()
    }
}
