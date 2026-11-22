package com.eon.assistant

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertNotNull
import kotlin.test.assertNull
import kotlin.test.assertTrue

class DemoActionGateTest {
    private class Node(val offers: DemoAction? = null, vararg children: Node) {
        val children = children.toMutableList()
        var available = true
        var route = DemoRoute.NATIVE
        var value = 0
        var step = if (offers == DemoAction.SCROLL_SAMPLES_FORWARD) 104 else 1
    }

    private class Fixture {
        val count = Node(DemoAction.CLICK_COUNTER)
        val samples = Node(DemoAction.SCROLL_SAMPLES_FORWARD, Node(), Node())
        val root = Node(null, count, samples)
        var epoch: Any? = Any()
        var window = Any()
        var time = 700L
        val reads = mutableListOf<Int>()
        val checked = mutableListOf<Node>()
        val actions = mutableListOf<Pair<Node, DemoAction>>()
        val routes = mutableListOf<DemoRoute>()
        var onCheck: () -> Unit = {}
        var onDispatch: () -> Boolean = { true }
        var updateState = true
        val gate = DemoActionGate(count, samples, { action ->
            val slot = if (action == DemoAction.CLICK_COUNTER) 0 else 1
            reads.add(slot)
            root.children.getOrNull(slot)
        }, { node, action ->
            checked.add(node)
            onCheck()
            if (node.available && node.offers == action) DemoOffer(node.route, node.value, node.step) else null
        }, { epoch?.let { DemoScope(it, window) } }, { time }, { node, _ -> node.value }) { node, action, offer ->
            actions.add(node to action)
            routes.add(offer.route)
            if (updateState) node.value += offer.maxDelta
            onDispatch()
        }
        fun click() = assertNotNull(gate.review(DemoAction.CLICK_COUNTER))
        fun scroll() = assertNotNull(gate.review(DemoAction.SCROLL_SAMPLES_FORWARD))
    }

    @Test fun eachConfirmationPerformsOnlyItsExactActionOnce() = with(Fixture()) {
        for ((target, action) in listOf(count to DemoAction.CLICK_COUNTER, samples to DemoAction.SCROLL_SAMPLES_FORWARD)) {
            val before = actions.size
            val r = assertNotNull(gate.review(action))
            assertEquals(before, actions.size)
            assertTrue(gate.confirm(r))
            assertFalse(gate.confirm(r))
            assertTrue(gate.verify(r))
            assertFalse(gate.verify(r))
            assertEquals(before + 1, actions.size)
            assertEquals(target to action, actions.last())
        }
        assertEquals(setOf(0, 1), reads.toSet())
    }

    @Test fun unrelatedSameShapedNodesCannotBeSubstitutedAndAreNotInspected() = with(Fixture()) {
        val r = click()
        root.children[0] = Node(DemoAction.CLICK_COUNTER, count)
        assertFalse(gate.confirm(r))
        assertNull(gate.review(DemoAction.CLICK_COUNTER))
        assertEquals(listOf(count), checked)
        assertTrue(actions.isEmpty())
    }

    @Test fun replacementOrCancellationRetiresTheOldConfirmation() = with(Fixture()) {
        val first = click()
        val second = scroll()
        assertFalse(gate.confirm(first))
        assertTrue(gate.confirm(second))
        val third = click()
        gate.cancel()
        assertFalse(gate.confirm(third))
        assertEquals(listOf(samples to DemoAction.SCROLL_SAMPLES_FORWARD), actions)
    }

    @Test fun foreignSurfaceReviewCannotAct() = with(Fixture()) {
        val other = Fixture()
        assertFalse(other.gate.confirm(click()))
        assertTrue(other.reads.isEmpty())
        assertTrue(other.actions.isEmpty())
    }

    @Test fun expiredReviewCannotInspectOrDispatchAtDeadline() = with(Fixture()) {
        val r = scroll()
        time += DemoActionGate.REVIEW_MS
        assertFalse(gate.confirm(r))
        time--
        assertFalse(gate.confirm(r))
        assertEquals(listOf(1, 1), reads)
        assertTrue(actions.isEmpty())
    }

    @Test fun unavailableOrReconnectedEnvironmentCannotUseEarlierConsent() = with(Fixture()) {
        val r = click()
        epoch = null
        assertFalse(gate.confirm(r))
        assertNull(gate.review(DemoAction.CLICK_COUNTER))
        epoch = Any()
        val second = click()
        epoch = Any() // Same package and Views, but not the same uninterrupted connection.
        assertFalse(gate.confirm(second))
        assertEquals(listOf(0, 0, 0, 0), reads)
        assertTrue(actions.isEmpty())
    }

    @Test fun lossOfActionSupportIsRecheckedAndConsumesConsent() = with(Fixture()) {
        val r = scroll()
        samples.available = false
        assertFalse(gate.confirm(r))
        samples.available = true
        assertFalse(gate.confirm(r))
        assertTrue(actions.isEmpty())
    }

    @Test fun revocationDuringTargetResolutionPreventsDispatch() = with(Fixture()) {
        val r = click()
        onCheck = gate::cancel
        assertFalse(gate.confirm(r))
        assertTrue(actions.isEmpty())
    }

    @Test fun canceledAcquisitionAndReparentDuringOfferAreNotAuthorizations() = with(Fixture()) {
        onCheck = gate::cancel
        assertNull(gate.review(DemoAction.CLICK_COUNTER))
        onCheck = {}
        val r = click()
        onCheck = { root.children[0] = Node(DemoAction.CLICK_COUNTER, count) }
        assertFalse(gate.confirm(r))
        assertFalse(gate.confirm(r))
        assertTrue(actions.isEmpty())
    }

    @Test fun reentryAndProviderFailureDoNotReopenApproval() = with(Fixture()) {
        val r = click()
        onDispatch = {
            assertFalse(gate.confirm(r))
            error("detached")
        }
        assertFailsWith<IllegalStateException> { gate.confirm(r) }
        assertFalse(gate.confirm(r))
        assertEquals(1, actions.size)
    }

    @Test fun unavailableNativeActionCanUseOnlyReviewedLocalRouteAndVerifyOnce() = with(Fixture()) {
        for (node in listOf(count, samples)) {
            node.route = DemoRoute.LOCAL_HIT
            val r = assertNotNull(gate.review(node.offers!!))
            assertEquals(DemoRoute.LOCAL_HIT, r.offer.route)
            assertFalse(gate.verify(r))
            assertTrue(gate.confirm(r))
            assertFalse(gate.confirm(r))
            assertTrue(gate.verify(r))
            assertFalse(gate.verify(r))
        }
        assertEquals(listOf(DemoRoute.LOCAL_HIT, DemoRoute.LOCAL_HIT), routes)
    }

    @Test fun routeStateViewportAndWindowCannotChangeUnderReview() = with(Fixture()) {
        val native = scroll()
        samples.route = DemoRoute.LOCAL_HIT
        assertFalse(gate.confirm(native))
        val local = scroll()
        samples.route = DemoRoute.NATIVE
        assertFalse(gate.confirm(local))
        samples.route = DemoRoute.LOCAL_HIT
        val moved = scroll()
        samples.value += 2
        assertFalse(gate.confirm(moved))
        val resized = scroll()
        samples.step--
        assertFalse(gate.confirm(resized))
        val otherWindow = scroll()
        window = Any()
        assertFalse(gate.confirm(otherWindow))
        assertTrue(actions.isEmpty())
    }

    @Test fun ambiguousNativeFailureDoesNotFallBackEvenIfEffectOccurred() = with(Fixture()) {
        val r = click()
        onDispatch = { count.route = DemoRoute.LOCAL_HIT; false }
        assertFalse(gate.confirm(r))
        assertFalse(gate.verify(r))
        assertFalse(gate.confirm(r))
        assertEquals(1, count.value)
        assertEquals(listOf(DemoRoute.NATIVE), routes)
    }

    @Test fun acceptedDispatchIsNotProofOfAnEffectOrLicenseToRetry() = with(Fixture()) {
        updateState = false
        for (node in listOf(count, samples)) {
            for (delta in listOf(0, -1, node.step + 1)) {
                node.value = 20
                val r = assertNotNull(gate.review(node.offers!!))
                assertTrue(gate.confirm(r))
                node.value += delta
                assertFalse(gate.verify(r))
                assertFalse(gate.confirm(r))
            }
        }
        assertEquals(6, actions.size)
    }

    @Test fun delayedObservationCannotReadReplacementOrAuthorizeAnything() = with(Fixture()) {
        val r = scroll()
        assertTrue(gate.confirm(r))
        root.children[1] = Node(DemoAction.SCROLL_SAMPLES_FORWARD).also { it.value = samples.value }
        assertFalse(gate.verify(r))
        root.children[1] = samples
        val next = scroll()
        assertTrue(gate.confirm(next))
        gate.cancel()
        assertFalse(gate.verify(next))
        assertFalse(gate.confirm(next))
        assertEquals(2, actions.size)
    }
}
