package com.eon.assistant

import java.io.File
import java.util.Locale
import javax.xml.parsers.DocumentBuilderFactory
import org.w3c.dom.Element
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNotNull
import kotlin.test.assertNull
import kotlin.test.assertSame
import kotlin.test.assertTrue

class VoiceHudPresentationTest {
    private val binding = VoiceHudRegistry.Binding()
    private val registry = VoiceHudRegistry().also { it.ready(binding) }
    private var revoked = 0
    private val hud = registry.Surface(binding) { revoked++ }

    private fun startCheck(): ProfileCheck.Requested {
        assertTrue(hud.show(userInvocation = true, eligible = true))
        hud.toggleDetails()
        return assertNotNull(hud.requestCheck())
    }

    @Test
    fun androidTaskCountsRequireAnExplicitRequestInTheCurrentExpandedShowing() {
        assertNull(hud.requestTasks())
        assertTrue(hud.show(true, true))
        assertNull(hud.requestTasks())
        hud.toggleDetails()
        val first = assertNotNull(hud.requestTasks())
        assertNull(hud.requestTasks())
        hud.toggleDetails()
        hud.toggleDetails()
        val next = assertNotNull(hud.requestTasks())
        val counts = OwnActivityTasks.Counts(1, 0)
        hud.finishTasks(first, counts)
        assertSame(next, hud.frame?.tasks)
        hud.finishTasks(next, counts)
        assertEquals(TaskSample.Result(counts), hud.frame?.tasks)
        val unavailable = assertNotNull(hud.requestTasks())
        hud.finishTasks(unavailable, null)
        assertEquals(TaskSample.Result(null), hud.frame?.tasks)
        val pending = assertNotNull(hud.requestTasks())
        registry.shutdown(binding)
        hud.finishTasks(pending, counts)
        assertNull(hud.frame)
    }

    @Test
    fun openingIsIdleAndOnlyAnExplicitExpandedCheckStartsWork() {
        hud.toggleDetails()
        assertNull(hud.frame)
        assertNull(hud.requestCheck())
        assertTrue(hud.show(true, true))
        assertEquals(VoiceHudFrame(expanded = false, check = ProfileCheck.Idle), hud.frame)
        assertNull(hud.requestCheck())
        hud.toggleDetails()
        val request = assertNotNull(hud.requestCheck())
        assertSame(request, hud.frame?.check)
        assertNull(hud.requestCheck())
        hud.toggleDetails()
        // Collapsing the details does not fabricate completion or start another check.
        assertSame(request, hud.frame?.check)
        val completion = ProfileCheck.Complete("p0_mobile_ultralight", 1u)
        hud.acceptResult(request, completion)
        assertEquals(VoiceHudFrame(false, completion), hud.frame)
    }

    @Test
    fun aFailedCheckIsNotASuggestionAndOldResultsCannotWinARetry() {
        val first = startCheck()
        hud.acceptResult(first, null)
        assertEquals(ProfileCheck.Unavailable, hud.frame?.check)
        val retry = assertNotNull(hud.requestCheck())
        hud.acceptResult(first, ProfileCheck.Complete("old", 2u))
        assertSame(retry, hud.frame?.check)
        val completion = ProfileCheck.Complete("new", 17u)
        hud.acceptResult(retry, completion)
        hud.acceptResult(retry, null)
        assertEquals(completion, hud.frame?.check)
    }

    @Test
    fun onlyTheSuccessfulCurrentWorkflowSuppliesTheVisibleCountAndProfile() {
        val resources = DocumentBuilderFactory.newInstance().apply {
            setFeature("http://apache.org/xml/features/disallow-doctype-decl", true)
        }.newDocumentBuilder().parse(File("src/main/res/values/voice_hud.xml")).getElementsByTagName("string")
        val english = (0 until resources.length).map { resources.item(it) as Element }.associate {
            R.string::class.java.getField(it.getAttribute("name")).getInt(null) to it.textContent
        }
        fun notice() = profileNotice(hud.frame?.check) { id, args -> english.getValue(id).format(Locale.US, *args) }

        assertEquals(ProfileNotice(null), notice())
        assertTrue(hud.show(true, true))
        hud.toggleDetails()
        hud.finishTasks(assertNotNull(hud.requestTasks()), OwnActivityTasks.Counts(9, 3))
        assertEquals(ProfileNotice("Session open"), notice())

        val check = assertNotNull(hud.requestCheck())
        assertEquals(ProfileNotice("Checking model profile…"), notice())
        // Distinct from either Android task count and outside the positive signed Int range.
        hud.acceptResult(check, ProfileCheck.Complete("p1_mobile_flagship", UInt.MAX_VALUE))
        assertEquals(ProfileNotice("Profile workflow complete. Completed tasks: 4294967295",
            "Suggested profile: p1_mobile_flagship"), notice())

        val retry = assertNotNull(hud.requestCheck())
        hud.acceptResult(check, ProfileCheck.Complete("late", 1u))
        assertEquals(ProfileNotice("Checking model profile…"), notice())
        hud.acceptResult(retry, null)
        assertEquals(ProfileNotice("Profile check unavailable"), notice())
        hud.hide()
        assertEquals(ProfileNotice(null), notice())
    }

    @Test
    fun hideAndRepeatedInvocationDropDetailsResultsAndOutstandingRequests() {
        val hidden = startCheck()
        hud.hide()
        val late = ProfileCheck.Complete("late", 2u)
        hud.acceptResult(hidden, late)
        assertNull(hud.frame)
        val firstShowing = startCheck()
        val nextShowing = startCheck() // same OS session object
        hud.acceptResult(firstShowing, late)
        assertSame(nextShowing, hud.frame?.check)
        hud.acceptResult(nextShowing, ProfileCheck.Complete("finished", 3u))
        assertTrue(hud.show(true, true))
        assertEquals(VoiceHudFrame(), hud.frame)
        assertFalse(hud.show(false, true))
        assertNull(hud.frame)
    }

    @Test
    fun revocationClearsTheFrameBeforeTheCloseCallbackAndRejectsLateNativeResults() {
        lateinit var old: VoiceHudRegistry.Surface
        old = registry.Surface(binding) {
            assertNull(old.frame)
            assertNull(old.requestCheck())
            revoked++
        }
        assertTrue(old.show(true, true))
        old.toggleDetails()
        val oldCheck = assertNotNull(old.requestCheck())
        val current = startCheck()
        assertEquals(1, revoked)
        old.acceptResult(oldCheck, ProfileCheck.Complete("stale", 8u))
        assertNull(old.frame)
        assertSame(current, hud.frame?.check)
        registry.shutdown(binding)
        hud.acceptResult(current, ProfileCheck.Complete("also stale", 1u))
        assertNull(hud.frame)
        assertEquals(2, revoked)
    }

    @Test
    fun losingEligibilityOrEndingTheSessionClearsEvenAPendingResult() {
        val denied = startCheck()
        assertFalse(hud.consumeAction(false))
        val late = ProfileCheck.Complete("late", 2u)
        hud.acceptResult(denied, late)
        assertNull(hud.frame)
        val ended = startCheck()
        hud.retire()
        hud.acceptResult(ended, late)
        assertNull(hud.frame)
        assertFalse(hud.show(true, true))
    }

    @Test
    fun toneConsentIsSeparateFromShowingExpansionAndTheProfileCheck() {
        assertNull(hud.requestTone())
        assertTrue(hud.show(true, true))
        assertNull(hud.requestTone())
        hud.toggleDetails()
        assertNotNull(hud.requestCheck())
        assertEquals(ToneScan.Idle, hud.frame?.tone)
        val request = assertNotNull(hud.requestTone())
        assertNull(hud.requestTone())
        hud.addLevel(request, -1)
        hud.addLevel(request, 101)
        for (i in 0..30) hud.addLevel(request, i)
        assertEquals(ToneScan.Reading(request, (0 until 12).toList()), hud.frame?.tone)
        hud.finishTone(request, true)
        assertEquals(ToneScan.Complete((0 until 12).toList()), hud.frame?.tone)
        hud.clearTone()
        assertEquals(ToneScan.Idle, hud.frame?.tone)
    }

    @Test
    fun collapseRetryHideAndRevokeRejectQueuedLevelsAndCompletion() {
        startCheck()
        val first = assertNotNull(hud.requestTone())
        hud.toggleDetails()
        hud.toggleDetails()
        val next = assertNotNull(hud.requestTone())
        hud.addLevel(first, 77)
        hud.finishTone(first, true)
        assertEquals(ToneScan.Reading(next), hud.frame?.tone)
        hud.finishTone(next, true) // No samples is not success.
        assertEquals(ToneScan.Unavailable, hud.frame?.tone)
        val failed = assertNotNull(hud.requestTone())
        hud.addLevel(failed, 88)
        hud.finishTone(failed, false)
        assertEquals(ToneScan.Unavailable, hud.frame?.tone)
        val hidden = assertNotNull(hud.requestTone())
        startCheck()
        hud.addLevel(hidden, 2)
        hud.finishTone(hidden, true)
        assertEquals(ToneScan.Idle, hud.frame?.tone)
        val revoked = assertNotNull(hud.requestTone())
        registry.shutdown(binding)
        hud.addLevel(revoked, 91)
        hud.finishTone(revoked, true)
        assertNull(hud.frame)
    }
}
