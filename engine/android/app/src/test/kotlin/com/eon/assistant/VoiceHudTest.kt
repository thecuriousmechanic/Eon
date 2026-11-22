package com.eon.assistant

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class VoiceHudTest {
    private val registry = VoiceHudRegistry()
    private val binding = VoiceHudRegistry.Binding()
    private var closes = 0
    private fun window() = registry.Surface(registry.binding) { closes++ }

    @Test
    fun systemContextBitsDoNotAuthorizeAppOrUnknownInvocations() {
        // Android's public SHOW_* flags: context bits 1/2, gesture 4, application 8,
        // activity 16, push-to-talk 32, notification 64. No Bundle data participates.
        assertEquals(3, NO_ASSIST_CONTEXT)
        for (context in 0..3) {
            assertTrue(userInvokedVoiceHud(4 or context))
            assertTrue(userInvokedVoiceHud(32 or context))
            for (source in listOf(0, 8, 16, 64, 128, 256, 4 or 8, 4 or 32)) {
                assertFalse(userInvokedVoiceHud(source or context))
            }
        }
    }

    @Test
    fun onlyAConfirmedBindingAndFreshUserInvocationOpenAWindow() {
        val early = window()
        assertFalse(early.show(true, true))
        registry.ready(binding)
        assertFalse(early.show(true, true))
        val current = window()
        assertFalse(current.show(false, true))
        assertFalse(current.show(true, false))
        assertFalse(current.consumeAction(true))
        assertTrue(current.show(true, true))
        assertTrue(current.consumeAction(true))
        assertFalse(current.consumeAction(true))
    }

    @Test
    fun roleLossOrHiddenWindowRejectsAQueuedTap() {
        registry.ready(binding)
        val hud = window()
        assertTrue(hud.show(true, true))
        assertFalse(hud.consumeAction(false))
        assertFalse(hud.consumeAction(true))
        assertTrue(hud.show(true, true))
        hud.hide()
        assertFalse(hud.consumeAction(true))
        assertTrue(hud.show(true, true))
        registry.shutdown(binding)
        assertEquals(1, closes)
        assertFalse(hud.consumeAction(true))
        assertFalse(hud.show(true, true))
    }

    @Test
    fun supersededWindowAndOldBindingCannotControlTheirReplacement() {
        registry.ready(binding)
        val old = window()
        val next = window()
        assertTrue(old.show(true, true))
        assertTrue(next.show(true, true))
        assertEquals(1, closes)
        old.hide()
        assertFalse(old.consumeAction(true))
        assertFalse(old.show(true, true))
        assertTrue(next.canUse(true))
        registry.ready(VoiceHudRegistry.Binding())
        assertEquals(2, closes)
        val rebound = window()
        assertTrue(rebound.show(true, true))
        registry.shutdown(binding)
        assertFalse(next.show(true, true))
        assertTrue(rebound.consumeAction(true))
    }

    @Test
    fun processRestartHasNoBindingWindowOrPendingAction() {
        registry.ready(binding)
        assertTrue(window().show(true, true))
        val restarted = VoiceHudRegistry()
        val newWindow = restarted.Surface(restarted.binding) { error("No window to revoke") }
        assertFalse(newWindow.canUse(true))
        assertFalse(newWindow.show(true, true))
    }
}
