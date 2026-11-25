package com.eon.assistant;

import android.os.Process;
import android.os.SystemClock;

import java.util.OptionalLong;
import java.util.function.LongSupplier;

/** One synchronous local action. No baseline, consent or result is stored across calls. */
public final class SelfProcessCpu {
    static final long MAX_INTERVAL_NS = 10_000_000_000L;

    private SelfProcessCpu() {}

    /**
     * Run action once on the caller's thread. Only when opted in, return the change in Android's
     * cumulative CPU milliseconds for this entire process (not wall time or the action in isolation).
     * Empty means disabled or unavailable. The 10 s limit bounds the measurement, not the action.
     */
    public static OptionalLong measure(boolean optedIn, Runnable action) {
        return measure(optedIn, action, () -> SystemClock.elapsedRealtimeNanos(), () -> Process.getElapsedCpuTime());
    }

    static OptionalLong measure(boolean optedIn, Runnable action, LongSupplier clock, LongSupplier cpu) {
        if (!optedIn) {
            action.run();
            return OptionalLong.empty();
        }
        long start = read(clock);
        long before = read(cpu);
        action.run();
        // Do not mistake a missing/zero baseline for a measurement from process start.
        if (start <= 0 || before <= 0) return OptionalLong.empty();
        long after = read(cpu);
        long end = read(clock);
        if (after < before || end <= start || end - start > MAX_INTERVAL_NS) return OptionalLong.empty();
        return OptionalLong.of(after - before);
    }

    private static long read(LongSupplier counter) {
        try {
            return counter.getAsLong();
        } catch (UnsupportedOperationException | SecurityException | LinkageError unavailable) {
            return -1;
        }
    }
}
