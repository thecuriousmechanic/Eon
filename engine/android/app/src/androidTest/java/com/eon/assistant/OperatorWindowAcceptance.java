package com.eon.assistant;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.Activity;
import android.app.Instrumentation;
import android.app.UiAutomation;
import android.os.Bundle;
import android.os.SystemClock;
import android.view.accessibility.AccessibilityNodeInfo;
import java.util.List;
import java.util.function.Predicate;

/**
 * Read-only observer in the TEST package's process. Instrumenting the selected assistant itself
 * force-stops its OS bindings. Do not suppress accessibility services or inject a single input.
 */
public final class OperatorWindowAcceptance extends Instrumentation {
    private static final String EON = "com.eon.assistant";
    private UiAutomation ui;
    private String mode;
    private String step = "mode";

    @Override public void onCreate(Bundle args) {
        super.onCreate(args);
        mode = args.getString("mode");
        start();
    }

    private AccessibilityNodeInfo node(AccessibilityNodeInfo root, String id) {
        if (root == null || !EON.equals(root.getPackageName())) return null;
        List<AccessibilityNodeInfo> nodes = root.findAccessibilityNodeInfosByViewId(EON + ":id/" + id);
        if (nodes.size() != 1 || !nodes.get(0).isVisibleToUser()) return null;
        return nodes.get(0);
    }

    private String text(AccessibilityNodeInfo root, String id) {
        AccessibilityNodeInfo n = node(root, id);
        return n == null || n.getText() == null ? "" : n.getText().toString();
    }

    private void await(String checkpoint, Predicate<AccessibilityNodeInfo> expected) {
        step = checkpoint;
        long start = SystemClock.elapsedRealtime();
        while (SystemClock.elapsedRealtime() - start < 30_000) {
            if (expected.test(ui.getRootInActiveWindow())) return;
            SystemClock.sleep(120);
        }
        throw new AssertionError(checkpoint);
    }

    private boolean count(AccessibilityNodeInfo root, int n) {
        return ("Demo count: " + n).equalsIgnoreCase(text(root, "demo_counter"));
    }

    private void verified(String checkpoint, int count, String effect, String route) {
        String expected = "Verified: " + effect + " (" + route + ").";
        await(checkpoint, root -> count(root, count) && expected.equals(text(root, "demo_status")));
    }

    private void demo() {
        await("initial-demo", root -> count(root, 0) && "No action is approved.".equals(text(root, "demo_status")));
        await("pending-review", root -> count(root, 0) && text(root, "demo_status").startsWith("Review expires"));
        await("canceled", root -> count(root, 0) && text(root, "demo_status").startsWith("Canceled."));
        verified("native-click", 1, "increment demo count once", "native action");
        verified("native-scroll", 1, "scroll sample list forward once", "native action");
        await("reduced-samples", root -> count(root, 1) && text(root, "demo_status").startsWith("Sample mode changed."));
        verified("local-click", 2, "increment demo count once", "local hit fallback");
        verified("local-scroll", 2, "scroll sample list forward once", "local hit fallback");
    }

    private boolean fresh(AccessibilityNodeInfo r) {
        return "Session open".equals(text(r, "hud_status")) && node(r, "hud_tone_scan") == null &&
                node(r, "hud_task_status") == null && node(r, "hud_result") == null;
    }

    private void hud() {
        await("user-invocation", this::fresh);
        await("tone-complete", root -> {
            String s = text(root, "hud_tone_status");
            return s.matches("Tone complete · (?:[1-9]|1[0-2]) levels · highest RMS (?:[1-9][0-9]?|100)%") &&
                    node(root, "hud_tone_clear") != null;
        });
        await("cleared", root -> node(root, "hud_tone_scan") != null && node(root, "hud_tone_status") == null);
        await("user-dismissal", root -> root != null && node(root, "hud_status") == null);
        await("fresh-invocation", this::fresh);
    }

    @Override public void onStart() {
        Bundle result = new Bundle();
        String key = "demo".equals(mode) ? "demo" : "voiceHud";
        int code = Activity.RESULT_CANCELED;
        try {
            ui = getUiAutomation(UiAutomation.FLAG_DONT_SUPPRESS_ACCESSIBILITY_SERVICES);
            AccessibilityServiceInfo info = ui.getServiceInfo();
            info.packageNames = new String[]{EON};
            info.flags |= AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS;
            ui.setServiceInfo(info);
            if ("demo".equals(mode)) demo();
            else if ("session".equals(mode)) hud();
            else throw new AssertionError();
            result.putString(key, "demo".equals(mode) ? "PASS-allowed" : "PASS-session");
            code = Activity.RESULT_OK;
        } catch (RuntimeException | AssertionError | LinkageError failure) {
            result.putString(key, "FAIL-" + step);
        }
        finish(code, result);
    }
}
