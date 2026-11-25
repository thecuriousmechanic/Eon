package com.eon.assistant;

import android.app.Activity;
import android.app.Application;
import android.app.Instrumentation;
import android.os.Bundle;
import android.os.SystemClock;

import java.util.OptionalLong;
import java.util.zip.CRC32;

/** Exercises the installed app's stock process CPU counter with bounded, local-only work. */
public final class SelfCpuAcceptance extends Instrumentation {
    @Override public void onCreate(Bundle arguments) {
        super.onCreate(arguments);
        start();
    }

    @Override public void onStart() {
        Bundle result = new Bundle();
        if (!"com.eon.assistant".equals(Application.getProcessName())) {
            result.putString("selfCpu", "FAIL-process");
            finish(Activity.RESULT_CANCELED, result);
            return;
        }
        byte[] bytes = new byte[8192];
        CRC32 crc = new CRC32();
        OptionalLong delta = SelfProcessCpu.measure(true, () -> {
            long start = SystemClock.elapsedRealtime();
            // Both a work cap and a wall-clock cap. No sleeps, files, packets or other processes.
            int blocks = 0;
            do {
                crc.reset();
                crc.update(bytes);
            } while (++blocks < 131072 && SystemClock.elapsedRealtime() - start < 300);
        });
        if (crc.getValue() != 0xd8f49994L || !delta.isPresent() || delta.getAsLong() <= 0) {
            result.putString("selfCpu", "FAIL-local-work");
            finish(Activity.RESULT_CANCELED, result);
            return;
        }
        result.putLong("processCpuMs", delta.getAsLong());
        result.putString("selfCpu", "PASS");
        finish(Activity.RESULT_OK, result);
    }
}
