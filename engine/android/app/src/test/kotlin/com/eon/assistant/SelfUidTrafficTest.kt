package com.eon.assistant

import java.util.function.LongSupplier
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse

class SelfUidTrafficTest {
    private fun samples(vararg values: Long): LongSupplier {
        val it = values.iterator()
        return LongSupplier { if (it.hasNext()) it.nextLong() else throw AssertionError("extra read") }
    }

    @Test fun consentIsPerCallAndDoesNotRerunOrSwallowTheAction() {
        val order = mutableListOf<String>()
        fun read(name: String) = LongSupplier { order.add(name); order.size.toLong() }
        val action = Runnable { order.add("action") }
        val delta = SelfUidTraffic.measure(true, action, read("clock"), read("rx"), read("tx")).get()
        assertEquals(3L, delta.rxBytes)
        assertEquals(3L, delta.txBytes)
        assertFalse(SelfUidTraffic.measure(false, action, samples(), samples(), samples()).isPresent)
        assertEquals(listOf("clock", "rx", "tx", "action", "rx", "tx", "clock", "action"), order)
        assertFailsWith<IllegalStateException> {
            SelfUidTraffic.measure(true, { error("action failed") }, samples(1), samples(3), samples(4))
        }
    }

    @Test fun invalidSamplesDropBothDirectionsWithoutSkippingTheAction() {
        fun invalid(clock: LongSupplier, rx: LongSupplier, tx: LongSupplier) {
            var calls = 0
            assertFalse(SelfUidTraffic.measure(true, { calls++ }, clock, rx, tx).isPresent)
            assertEquals(1, calls)
        }
        for (unavailable in listOf(samples(-1), samples(0), LongSupplier { throw SecurityException() },
                LongSupplier { throw UnsupportedOperationException() }, LongSupplier { throw NoSuchMethodError() })) {
            invalid(samples(1), unavailable, samples(9))
        }
        invalid(samples(1), samples(9), samples(0))
        invalid(samples(1, 2), samples(9, 9), samples(9, -1))
        invalid(samples(1, 2), samples(9, 8), samples(9, 12))
        invalid(samples(1, 2), samples(9, 12), samples(9, 8))
        invalid(samples(1, 0), samples(9, 10), samples(9, 10))
        invalid(samples(1, 1), samples(9, 10), samples(9, 10))
        invalid(samples(1, SelfUidTraffic.MAX_INTERVAL_NS + 2), samples(9, 10), samples(9, 10))
    }

    @Test fun establishedUnchangedCounterAndLastValidInstantAreUsable() {
        val delta = SelfUidTraffic.measure(true, {}, samples(1, 1 + SelfUidTraffic.MAX_INTERVAL_NS),
            samples(5, 5), samples(Long.MAX_VALUE - 7, Long.MAX_VALUE)).get()
        assertEquals(0L, delta.rxBytes)
        assertEquals(7L, delta.txBytes)
    }
}
