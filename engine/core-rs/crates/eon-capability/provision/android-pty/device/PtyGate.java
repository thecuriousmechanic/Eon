package dev.eon.ptygate;

import android.app.Activity;
import android.app.Instrumentation;
import android.os.Bundle;
import java.io.BufferedReader;
import java.io.File;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;

/** Runs the Rust provider tests as this permissionless app, not the adb shell. */
public final class PtyGate extends Instrumentation {
    private boolean wrongImage;

    @Override public void onCreate(Bundle args) {
        super.onCreate(args);
        wrongImage = "wrong-image".equals(args.getString("fixture"));
        start();
    }

    @Override public void onStart() {
        Bundle result = new Bundle();
        try {
            String exe = new File(getTargetContext().getApplicationInfo().nativeLibraryDir,
                                  "libeon_pty_tests.so").getAbsolutePath();
            ProcessBuilder tests = wrongImage
                ? new ProcessBuilder(exe, "--exact", "--ignored",
                    "pty::native::android::wrong_apk_helper_is_rejected_before_launch")
                : new ProcessBuilder(exe, "pty::", "--test-threads=4");
            Process proc = tests.redirectErrorStream(true).start();
            String summary = "";
            StringBuilder output = new StringBuilder();
            try (BufferedReader r = new BufferedReader(
                    new InputStreamReader(proc.getInputStream(), StandardCharsets.UTF_8))) {
                for (String line; (line = r.readLine()) != null;) {
                    if (line.startsWith("test result:")) summary = line;
                    if (output.length() < 16000) output.append(line).append('\n');
                }
            }
            String expected = wrongImage
                ? "test result: ok\\. 1 passed; 0 failed; 0 ignored;.*"
                : "test result: ok\\. [1-9][0-9]* passed; 0 failed;.*";
            if (proc.waitFor() != 0 || !summary.matches(expected)) {
                throw new AssertionError(output.toString());
            }
            result.putString("pty", summary);
            finish(Activity.RESULT_OK, result);
        } catch (Throwable e) {
            result.putString("failure", e.toString());
            finish(Activity.RESULT_CANCELED, result);
        }
    }
}
