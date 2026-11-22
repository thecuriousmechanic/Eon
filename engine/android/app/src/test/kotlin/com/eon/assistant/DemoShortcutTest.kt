package com.eon.assistant

import kotlin.test.Test
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class DemoShortcutTest {
    @Test fun onlyAnExactAssignedComponentMatches() {
        val full = "com.eon.assistant/com.eon.assistant.FocusedControlService"
        val short = "com.eon.assistant/.FocusedControlService"
        for (value in listOf(full, short, "other/.Service:$full", "$short:other/.Service")) {
            assertTrue(DemoShortcut.assigned(value, full, short))
        }
        for (value in listOf(null, "", "other/.FocusedControlService", full + "2", "prefix$short", "x".repeat(8193) + ":$full")) {
            assertFalse(DemoShortcut.assigned(value, full, short))
        }
    }
}
