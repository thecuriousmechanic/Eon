package org.eon.test.selfcpu;

import android.app.Activity;
import android.app.Instrumentation;
import android.os.Bundle;
import android.os.Process;
import java.io.File;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.TimeUnit;
import org.json.JSONObject;

/** Run the packaged production CLI twice as an ordinary app, not as adb shell. */
public final class StockAppTest extends Instrumentation {
    @Override public void onCreate(Bundle arguments) {
        super.onCreate(arguments);
        start();
    }

    private static void require(boolean ok, String message) {
        if (!ok) throw new AssertionError(message);
    }

    private JSONObject review(boolean optIn) throws Exception {
        File cli = new File(getContext().getApplicationInfo().nativeLibraryDir, "libeon_audit.so");
        List<String> args = new ArrayList<>(Arrays.asList(cli.getAbsolutePath(), "python"));
        if (optIn) args.add("--self-cpu-ebpf");
        java.lang.Process child = new ProcessBuilder(args).start();
        try {
            try (OutputStream input = child.getOutputStream()) {
                input.write("value = 7\n".getBytes(StandardCharsets.US_ASCII));
            }
            require(child.waitFor(10, TimeUnit.SECONDS), "audit timed out");
            require(child.exitValue() == 0, "audit exited " + child.exitValue());
            try (InputStream output = child.getInputStream()) {
                return new JSONObject(new String(output.readAllBytes(), StandardCharsets.UTF_8));
            }
        } finally {
            child.destroyForcibly();
        }
    }

    @Override public void onStart() {
        Bundle result = new Bundle();
        try {
            String label = new String(Files.readAllBytes(Paths.get("/proc/thread-self/attr/current")),
                                      StandardCharsets.US_ASCII).trim();
            String[] context = label.split(":", 4);
            require(Process.isApplicationUid(Process.myUid()) && context.length == 4
                    && context[2].equals("untrusted_app"), "not an ordinary app domain");
            List<String> status = Files.readAllLines(Paths.get("/proc/thread-self/status"));
            require(status.contains("CapEff:\t0000000000000000") && status.contains("Seccomp:\t2"),
                    "expected no effective capabilities and an active seccomp filter");

            String key = "self_thread_user_cpu_ebpf";
            JSONObject off = review(false);
            JSONObject on = review(true);
            require(!off.has(key), "unexpected default measurement");
            JSONObject measurement = on.getJSONObject(key);
            require(measurement.length() == 2 && "unverified".equals(measurement.getString("status"))
                    && "Unavailable".equals(measurement.getString("reason")), "expected explicit unavailability");
            on.remove(key);
            require("Python".equals(off.getString("language")) && off.getJSONArray("checks").length() > 0
                    && off.toString().equals(on.toString()), "review changed or did not run");
            result.putString("self_cpu", "PASS untrusted_app; seccomp=2; CapEff=0; opt-out absent; opt-in unavailable");
            finish(Activity.RESULT_OK, result);
        } catch (Exception | AssertionError e) {
            result.putString("self_cpu_error", e.toString());
            finish(Activity.RESULT_CANCELED, result);
        }
    }
}
