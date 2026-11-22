package com.eon.assistant;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.Activity;
import android.app.Instrumentation;
import android.app.UiAutomation;
import android.content.ComponentName;
import android.os.Build;
import android.os.Bundle;
import android.os.SystemClock;
import android.service.voice.VoiceInteractionService;
import android.text.TextUtils;
import android.view.accessibility.AccessibilityNodeInfo;
import java.util.List;
import java.util.function.Predicate;

/** Separate observer process. No session injection, screen dumps, permission grants or UI actions. */
public final class ControlsAssistAcceptance extends Instrumentation {
    private static final String EON = "com.eon.assistant";
    private UiAutomation ui;
    private String step = "preflight";
    private boolean denied;
    private static final String REVIEW = "Review sharing the CPU timing setting";
    private boolean selected() {
        return VoiceInteractionService.isActiveService(getTargetContext(),
                new ComponentName(EON, EON + ".EonVoiceInteractionService"));
    }

    @Override public void onCreate(Bundle args) {
        super.onCreate(args);
        denied = args != null && "denied".equals(args.getString("policy"));
        start();
    }

    private static void require(boolean value) {
        if (!value) throw new AssertionError();
    }

    private static String label(AccessibilityNodeInfo root, String id) {
        List<AccessibilityNodeInfo> nodes = root.findAccessibilityNodeInfosByViewId(EON + ":id/" + id);
        if (nodes.isEmpty()) return "";
        require(nodes.size() == 1);
        AccessibilityNodeInfo n = nodes.get(0);
        require(n.getWindowId() == root.getWindowId() && TextUtils.equals(EON, n.getPackageName()));
        return n.isVisibleToUser() && n.getText() != null ? n.getText().toString() : "";
    }

    private int await(String name, Predicate<AccessibilityNodeInfo> check) {
        step = name;
        long end = SystemClock.elapsedRealtime() + 40_000;
        while (SystemClock.elapsedRealtime() < end) {
            ui.clearCache();
            AccessibilityNodeInfo root = ui.getRootInActiveWindow();
            if (root != null && TextUtils.equals(EON, root.getPackageName()) && check.test(root)) {
                return root.getWindowId();
            }
            SystemClock.sleep(80);
        }
        throw new AssertionError();
    }

    private void review(int controls, boolean timing) {
        String setting = timing ? "ON" : "OFF";
        String confirm = "Share once: CPU timing is " + setting + ". Send only this EON setting to the assistant card? "
                + "Tap again to confirm. No measurements or screen contents.";
        await("explicit-review", r -> r.getWindowId() == controls &&
                confirm.equalsIgnoreCase(label(r, "assistant_context_review")));
        String expected = denied ? "Shared setting unavailable. Screen layout and screenshot collection are always off."
                : timing ? "EON controls shared: CPU timing ON. Check model profile in controls to measure this process. No screen contents collected."
                : "EON controls shared: CPU timing OFF. Check model profile in controls without measuring this process. No screen contents collected.";
        int reviewed = await(denied ? "policy-denied" : "volunteered-setting", r -> r.getWindowId() != controls
                && "Session open".equals(label(r, "hud_status")) && expected.equals(label(r, "hud_context")));
        await("dismissed", r -> r.getWindowId() == controls && ui.getWindows().stream().anyMatch(w -> w.getId() == controls)
                && ui.getWindows().stream().noneMatch(w -> w.getId() == reviewed)
                && REVIEW.equalsIgnoreCase(label(r, "assistant_context_review")));
    }

    @Override public void onStart() {
        Bundle result = new Bundle();
        int code = Activity.RESULT_CANCELED;
        try {
            require(Build.VERSION.SDK_INT >= 34 && ("ranchu".equals(Build.HARDWARE) || "goldfish".equals(Build.HARDWARE))
                    && (EON + ".keystoretest").equals(getTargetContext().getPackageName()) && selected());
            ui = getUiAutomation(UiAutomation.FLAG_DONT_SUPPRESS_ACCESSIBILITY_SERVICES);
            AccessibilityServiceInfo info = ui.getServiceInfo();
            info.packageNames = new String[]{EON};
            info.flags |= AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS
                    | AccessibilityServiceInfo.FLAG_RETRIEVE_INTERACTIVE_WINDOWS;
            ui.setServiceInfo(info);
            int controls = await("selected-controls", r -> "EON is the selected assistant.".equals(label(r, "assistant_role_status"))
                    && REVIEW.equalsIgnoreCase(label(r, "assistant_context_review")));
            review(controls, false);
            if (!denied) review(controls, true);
            await("fresh-private-session", r -> r.getWindowId() != controls && "Session open".equals(label(r, "hud_status"))
                    && "EON · Mic off".equals(label(r, "hud_title")) && label(r, "hud_context").isEmpty());
            require(selected());
            result.putString("controlsMetadata", denied ? "PASS-policy-denied" : "PASS-volunteered");
            code = Activity.RESULT_OK;
        } catch (RuntimeException | AssertionError | LinkageError failure) {
            result.putString("controlsMetadata", "FAIL-" + step);
        }
        finish(code, result);
    }
}
