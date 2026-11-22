package com.eon.assistant

import java.io.File
import javax.xml.parsers.DocumentBuilderFactory
import org.w3c.dom.Element
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class InspectionManifestTest {
    private val android = "http://schemas.android.com/apk/res/android"
    private fun root(path: String): Element = DocumentBuilderFactory.newInstance().apply {
        isNamespaceAware = true
        setFeature("http://apache.org/xml/features/disallow-doctype-decl", true)
    }.newDocumentBuilder().parse(File("src/main/$path")).documentElement

    private fun Element.elements(tag: String) = getElementsByTagName(tag).let { nodes ->
        (0 until nodes.length).map { nodes.item(it) as Element }
    }
    private fun Element.attr(name: String) = getAttributeNS(android, name)

    @Test
    fun optionalMicrophoneAndUserStartedSpecialUseOverlayWithSystemGuardedServices() {
        val manifest = root("AndroidManifest.xml")
        assertFalse(manifest.elements("application").single().hasAttributeNS(android, "debuggable"))
        assertEquals(setOf("HIDE_OVERLAY_WINDOWS", "RECORD_AUDIO", "SYSTEM_ALERT_WINDOW", "POST_NOTIFICATIONS",
            "FOREGROUND_SERVICE", "FOREGROUND_SERVICE_SPECIAL_USE", "FOREGROUND_SERVICE_MICROPHONE",
            "FOREGROUND_SERVICE_MEDIA_PROJECTION").map { "android.permission.$it" }.toSet(),
            manifest.elements("uses-permission").map { it.attr("name") }.toSet())
        val microphone = manifest.elements("uses-feature").single { it.attr("name") == "android.hardware.microphone" }
        assertEquals("false", microphone.attr("required"))
        val services = manifest.elements("service").associateBy { it.attr("name") }
        assertEquals(setOf(".FocusedControlService", ".EonVoiceInteractionService", ".EonVoiceSessionService",
            ".FloatingHudService", ".ProjectionGrantService"), services.keys)
        val grant = services.getValue(".ProjectionGrantService")
        assertEquals("mediaProjection", grant.attr("foregroundServiceType"))
        assertEquals("false", grant.attr("exported"))
        assertEquals("true", grant.attr("stopWithTask"))
        assertEquals("@bool/projection_grant_supported", grant.attr("enabled"))
        assertTrue(grant.elements("intent-filter").isEmpty())
        val floating = services.getValue(".FloatingHudService")
        assertEquals("false", floating.attr("exported"))
        assertEquals(setOf("specialUse", "microphone"), floating.attr("foregroundServiceType").split('|').toSet())
        assertEquals("true", floating.attr("stopWithTask"))
        assertEquals("@bool/voice_hud_supported", floating.attr("enabled"))
        assertTrue(floating.elements("intent-filter").isEmpty())
        assertEquals("false", manifest.elements("activity").single { it.attr("name") == ".FloatingHudActivity" }.attr("exported"))
        val service = services.getValue(".FocusedControlService")
        assertEquals("android.permission.BIND_ACCESSIBILITY_SERVICE", service.attr("permission"))
        assertEquals("@bool/focused_control_supported", service.attr("enabled"))
        assertEquals("@xml/focused_control_service", service.elements("meta-data").single().attr("resource"))
        val result = manifest.elements("activity").single { it.attr("name") == ".InspectionResultActivity" }
        assertEquals("false", result.attr("exported"))
        assertEquals("true", result.attr("noHistory"))
        assertEquals("true", result.attr("excludeFromRecents"))
        assertTrue(manifest.elements("receiver").isEmpty())
    }

    @Test
    fun demoIsPrivateAndHasNoIntentActionOrLockscreenEntryPoint() {
        val demo = root("AndroidManifest.xml").elements("activity").single { it.attr("name") == ".DemoActionActivity" }
        assertEquals("false", demo.attr("exported"))
        assertEquals("true", demo.attr("noHistory"))
        assertEquals("@bool/focused_control_supported", demo.attr("enabled"))
        assertTrue(demo.elements("intent-filter").isEmpty())
        assertFalse(demo.hasAttributeNS(android, "showWhenLocked"))
        assertFalse(demo.hasAttributeNS(android, "turnScreenOn"))
    }

    @Test
    fun voiceBindingRequiresSystemIdentityAndDoesNotAdvertiseOtherTriggers() {
        val services = root("AndroidManifest.xml").elements("service").associateBy { it.attr("name") }
        val voice = services.getValue(".EonVoiceInteractionService")
        val session = services.getValue(".EonVoiceSessionService")
        for (endpoint in listOf(voice, session)) {
            assertEquals("android.permission.BIND_VOICE_INTERACTION", endpoint.attr("permission"))
            assertEquals("@bool/voice_hud_supported", endpoint.attr("enabled"))
        }
        assertEquals("android.service.voice.VoiceInteractionService", voice.elements("action").single().attr("name"))
        assertTrue(session.elements("intent-filter").isEmpty())
        val meta = voice.elements("meta-data").single()
        assertEquals("android.voice_interaction", meta.attr("name"))
        assertEquals("@xml/voice_interaction", meta.attr("resource"))
        val config = root("res/xml/voice_interaction.xml")
        assertEquals("com.eon.assistant" + session.attr("name"), config.attr("sessionService"))
        assertTrue(config.attr("recognitionService").isNotBlank())
        assertEquals("true", config.attr("supportsAssist"))
        assertEquals("true", config.attr("supportsLocalInteraction"))
        val source = root("AndroidManifest.xml").elements("activity").single { it.attr("name") == ".ProfileActionActivity" }
        assertEquals("false", source.attr("exported"))
        assertTrue(source.elements("intent-filter").isEmpty())
        assertEquals("false", config.attr("supportsLaunchVoiceAssistFromKeyguard"))
        assertFalse(config.hasAttributeNS(android, "hotwordDetectionService"))
        assertFalse(config.hasAttributeNS(android, "visualQueryDetectionService"))
        for ((dir, expected) in listOf("values" to "false", "values-v31" to "true")) {
            val enabled = root("res/$dir/voice_hud.xml").elements("bool").single { it.getAttribute("name") == "voice_hud_supported" }
            assertEquals(expected, enabled.textContent.trim())
        }
    }

    @Test
    fun focusServiceOptsOutOfEventsGestureInputAndSensitiveData() {
        val service = root("res/xml/focused_control_service.xml")
        assertEquals("true", service.attr("canRetrieveWindowContent"))
        assertEquals("false", service.attr("isAccessibilityTool"))
        assertEquals("flagRequestAccessibilityButton", service.attr("accessibilityFlags"))
        for (attribute in listOf("accessibilityEventTypes", "canPerformGestures", "canTakeScreenshot",
            "canRequestFilterKeyEvents", "canRequestTouchExplorationMode", "canControlMagnification")) {
            assertFalse(service.hasAttributeNS(android, attribute), attribute)
        }
        for ((dir, expected) in listOf("values" to "false", "values-v34" to "true")) {
            val enabled = root("res/$dir/inspector.xml").elements("bool").single { it.getAttribute("name") == "focused_control_supported" }
            assertEquals(expected, enabled.textContent.trim())
        }
    }
}
