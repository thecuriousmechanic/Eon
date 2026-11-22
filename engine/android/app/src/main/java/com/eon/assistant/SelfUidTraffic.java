package com.eon.assistant;

import android.net.TrafficStats;
import android.os.Process;
import android.os.SystemClock;

import java.util.Optional;
import java.util.function.LongSupplier;

/** A one-shot comparison of Android's reported byte counters for the calling UID. */
public final class SelfUidTraffic {
    static final long MAX_INTERVAL_NS = 10_000_000_000L;

    private SelfUidTraffic() {}

    public static final class Delta {
        public final long rxBytes;
        public final long txBytes;

        private Delta(long rxBytes, long txBytes) {
            this.rxBytes = rxBytes;
            this.txBytes = txBytes;
        }
    }

    /**
     * Runs the action once, even when measurement is off or unavailable. No snapshot is retained.
     * The platform can cache samples and includes concurrent work by this UID on all interfaces;
     * these differences do not attribute traffic to the action. No traffic is generated.
     */
    public static Optional<Delta> measure(boolean optedIn, Runnable action) {
        return measure(optedIn, action, SystemClock::elapsedRealtimeNanos,
                () -> TrafficStats.getUidRxBytes(Process.myUid()),
                () -> TrafficStats.getUidTxBytes(Process.myUid()));
    }

    static Optional<Delta> measure(boolean optedIn, Runnable action, LongSupplier clock,
                                   LongSupplier rx, LongSupplier tx) {
        if (!optedIn) {
            action.run();
            return Optional.empty();
        }
        long start = read(clock);
        long beforeRx = read(rx);
        long beforeTx = read(tx);
        action.run();
        // AOSP also returns zeroes for an absent UID map entry. With separate public API
        // reads, conservatively require an established baseline in *both* directions.
        if (start <= 0 || beforeRx <= 0 || beforeTx <= 0) return Optional.empty();
        long afterRx = read(rx);
        long afterTx = read(tx);
        long end = read(clock);
        if (afterRx < beforeRx || afterTx < beforeTx || end <= start || end - start > MAX_INTERVAL_NS) {
            return Optional.empty();
        }
        return Optional.of(new Delta(afterRx - beforeRx, afterTx - beforeTx));
    }

    private static long read(LongSupplier counter) {
        try {
            return counter.getAsLong();
        } catch (RuntimeException | LinkageError unavailable) {
            // Includes platform service failure as well as permission/unsupported API errors.
            return -1;
        }
    }
}
