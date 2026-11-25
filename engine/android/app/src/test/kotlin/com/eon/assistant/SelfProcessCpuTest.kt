package com.eon.assistant

import java.util.OptionalLong
import java.util.function.LongSupplier
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class SelfProcessCpuTest {
    private fun samples(vararg values: Long): LongSupplier {
        val iterator = values.iterator()
        return LongSupplier { iterator.nextLong() }
    }

    private fun measure(start: Long, end: Long, before: Long, after: Long): OptionalLong =
        SelfProcessCpu.measure(true, {}, samples(start, end), samples(before, after))

    @Test
    fun optInIsPerCallAndWrapsExactlyOneAction() {
        val calls = mutableListOf<String>()
        val action = Runnable { calls.add("action") }
        val clock = LongSupplier { calls.add("clock"); calls.size.toLong() }
        val cpu = LongSupplier { calls.add("cpu"); calls.size.toLong() }
        assertEquals(2L, SelfProcessCpu.measure(true, action, clock, cpu).asLong)
        assertFalse(SelfProcessCpu.measure(false, action, clock, cpu).isPresent)
        assertEquals(listOf("clock", "cpu", "action", "cpu", "clock", "action"), calls)
    }

    @Test
    fun deltaIsInCpuMillisecondsAndCanBeZeroOnlyWithValidSamples() {
        assertEquals(0L, measure(1, 2, 13, 13).asLong)
        assertEquals(7L, measure(100, 100 + SelfProcessCpu.MAX_INTERVAL_NS, 11, 18).asLong)
        assertEquals(1L, measure(Long.MAX_VALUE - 1, Long.MAX_VALUE, Long.MAX_VALUE - 1, Long.MAX_VALUE).asLong)
    }

    @Test
    fun missingResetReversedAndExpiredSamplesNeverReportZero() {
        for (invalid in listOf(
            measure(-1, 2, 3, 5), measure(1, -1, 3, 5),
            measure(1, 2, -1, 5), measure(1, 2, 3, -1), measure(1, 2, 0, 0),
            measure(8, 2, 3, 5), measure(8, 8, 3, 5), measure(1, 2, 7, 3),
            measure(1, SelfProcessCpu.MAX_INTERVAL_NS + 2, 3, 5)
        )) assertFalse(invalid.isPresent)
    }

    @Test
    fun unsupportedSamplesDoNotDisableTheActionOrCarryStateToTheNextCall() {
        var ran = false
        val unavailable = LongSupplier { throw UnsupportedOperationException() }
        assertFalse(SelfProcessCpu.measure(true, { ran = true }, samples(1), unavailable).isPresent)
        assertTrue(ran)
        assertFailsWith<IllegalStateException> {
            SelfProcessCpu.measure(true, { error("action failed") }, samples(42), samples(72))
        }
        assertEquals(3L, measure(1, 5, 1, 4).asLong)
    }
}
