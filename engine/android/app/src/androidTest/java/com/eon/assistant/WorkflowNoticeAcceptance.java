package com.eon.assistant;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.Activity;
import android.app.ActivityManager;
import android.app.Instrumentation;
import android.app.UiAutomation;
import android.os.Build;
import android.os.Bundle;
import android.os.SystemClock;
import android.text.TextUtils;
import android.view.View;
import android.view.accessibility.AccessibilityEvent;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;
import java.util.Collections;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.function.Predicate;

/** Observe the selected assistant from the test APK; do not start a session, click, or grant access. */
public final class WorkflowNoticeAcceptance extends Instrumentation {
    private static final String EON = "com.eon.assistant";
    private static final String ID = EON + ":id/";
    private UiAutomation ui;
    private static final class CardClicks {
        int window = -1;
        long expanded, checked;
        boolean invalid;

        synchronized void record(int id, boolean toggle, long time) {
            if (window == -1) window = id;
            if (window != id || checked != 0 || time <= 0) invalid = true;
            else if (toggle && expanded == 0) expanded = time;
            else if (!toggle && expanded != 0 && time >= expanded) checked = time;
            else invalid = true;
        }

        synchronized boolean expandedThenChecked(int card) {
            require(!invalid);
            return window == card && checked != 0;
        }
    }
    private String step = "preflight";

    @Override public void onCreate(Bundle args) {
        super.onCreate(args);
        start();
    }

    private static void require(boolean condition) {
        if (!condition) throw new AssertionError();
    }

    private static AccessibilityNodeInfo visible(AccessibilityNodeInfo root, String id) {
        if (root == null) return null;
        List<AccessibilityNodeInfo> nodes = root.findAccessibilityNodeInfosByViewId(ID + id);
        if (nodes.isEmpty()) return null;
        require(nodes.size() == 1);
        AccessibilityNodeInfo n = nodes.get(0);
        require(n.getWindowId() == root.getWindowId() && TextUtils.equals(EON, n.getPackageName()));
        return n.isVisibleToUser() ? n : null;
    }

    private static String text(AccessibilityNodeInfo root, String id) {
        AccessibilityNodeInfo n = visible(root, id);
        return n == null || n.getText() == null ? "" : n.getText().toString();
    }

    private AccessibilityNodeInfo await(String checkpoint, Predicate<AccessibilityNodeInfo> expected) {
        step = checkpoint;
        long end = SystemClock.elapsedRealtime() + 30_000;
        while (SystemClock.elapsedRealtime() < end) {
            ui.clearCache();
            AccessibilityNodeInfo root = ui.getRootInActiveWindow();
            if (root != null && TextUtils.equals(EON, root.getPackageName()) && expected.test(root)) return root;
            SystemClock.sleep(80);
        }
        throw new AssertionError();
    }

    private static boolean idle(AccessibilityNodeInfo r, boolean expanded) {
        AccessibilityNodeInfo toggle = visible(r, "hud_toggle");
        return toggle != null && TextUtils.equals(toggle.getContentDescription(),
                        expanded ? "Hide session details" : "Show session details")
                && "Session open".equals(text(r, "hud_status")) && visible(r, "hud_result") == null
                && (expanded ? "Check model profile".equalsIgnoreCase(text(r, "hud_check"))
                             : visible(r, "hud_check") == null);
    }

    private static boolean controls(AccessibilityNodeInfo r) {
        return "EON is the selected assistant.".equals(text(r, "assistant_role_status"))
                && visible(r, "hud_status") == null && visible(r, "hud_result") == null;
    }

    // Read IDs and active flags only, never the root, title or contents of other windows.
    private Set<Integer> windowIds(int active) {
        List<AccessibilityWindowInfo> windows = ui.getWindows();
        if (windows == null) return Collections.emptySet();
        Set<Integer> ids = new HashSet<>();
        boolean sawActive = false;
        for (AccessibilityWindowInfo w : windows) {
            if (w == null || w.getId() < 0 || !ids.add(w.getId())) return Collections.emptySet();
            if (w.isActive()) {
                if (sawActive || w.getId() != active) return Collections.emptySet();
                sawActive = true;
            }
        }
        return sawActive ? ids : Collections.emptySet();
    }

    private void observe() {
        // This fixture intentionally has <8 decimal GB, so the profile is known independently
        // of both the generated bindings and the text the application happens to show.
        ActivityManager.MemoryInfo memory = new ActivityManager.MemoryInfo();
        getTargetContext().getSystemService(ActivityManager.class).getMemoryInfo(memory);
        require(memory.totalMem > 0 && memory.totalMem < 8_000_000_000L);
        int controls = await("selected-controls", WorkflowNoticeAcceptance::controls).getWindowId();
        // Arm before observing the fresh card. The operator need not stop between expand and
        // Check; bind both ordered click events to the window we independently observe as idle.
        CardClicks clicks = new CardClicks();
        final long since = SystemClock.uptimeMillis();
        ui.setOnAccessibilityEventListener(event -> {
            if (event.getEventTime() < since || event.getEventType() != AccessibilityEvent.TYPE_VIEW_CLICKED
                    || !TextUtils.equals(EON, event.getPackageName())) return;
            AccessibilityNodeInfo n = event.getSource(0);
            int window = event.getWindowId();
            if (window < 0 || n == null || n.getWindowId() != window || !TextUtils.equals(EON, n.getPackageName())) return;
            String id = n.getViewIdResourceName();
            if ((ID + "hud_toggle").equals(id) || (ID + "hud_check").equals(id))
                clicks.record(window, (ID + "hud_toggle").equals(id), event.getEventTime());
        });
        int card = await("fresh-card", r -> r.getWindowId() != controls && idle(r, false)).getWindowId();
        await("explicit-check", r -> r.getWindowId() == card && clicks.expandedThenChecked(card));
        Set<Integer> otherWindows = new HashSet<>();
        await("completed-receipt", r -> {
            AccessibilityNodeInfo status = visible(r, "hud_status");
            if (r.getWindowId() != card || status == null
                    || status.getLiveRegion() != View.ACCESSIBILITY_LIVE_REGION_POLITE
                    || !TextUtils.equals("Profile workflow complete. Completed tasks: 1", status.getText())
                    || !"Suggested profile: p0_mobile_ultralight".equals(text(r, "hud_result"))) return false;
            Set<Integer> ids = windowIds(card);
            if (!ids.contains(controls) || ids.size() < 3) return false;
            otherWindows.addAll(ids);
            otherWindows.remove(card);
            return true;
        });
        ui.setOnAccessibilityEventListener(null);
        // Tapping through the non-modal card can make controls active without dismissing it.
        // Require the rest of the known inventory too; a list truncated to controls alone
        // would otherwise turn a missing window into false evidence of removal.
        await("dismissed-to-controls", r -> {
            if (r.getWindowId() != controls || !controls(r)) return false;
            Set<Integer> ids = windowIds(controls);
            return ids.containsAll(otherWindows) && !ids.contains(card);
        });
        int fresh = await("reshown-card", r -> idle(r, false)).getWindowId();
        await("reshown-details", r -> r.getWindowId() == fresh && idle(r, true));
    }

    @Override public void onStart() {
        Bundle result = new Bundle();
        int code = Activity.RESULT_CANCELED;
        try {
            require(Build.VERSION.SDK_INT >= 34 && ("ranchu".equals(Build.HARDWARE) || "goldfish".equals(Build.HARDWARE))
                    && (EON + ".keystoretest").equals(getTargetContext().getPackageName()));
            ui = getUiAutomation(UiAutomation.FLAG_DONT_SUPPRESS_ACCESSIBILITY_SERVICES);
            AccessibilityServiceInfo info = ui.getServiceInfo();
            info.packageNames = new String[]{EON};
            info.eventTypes = AccessibilityEvent.TYPE_VIEW_CLICKED;
            info.notificationTimeout = 0;
            info.flags |= AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS
                    | AccessibilityServiceInfo.FLAG_RETRIEVE_INTERACTIVE_WINDOWS;
            ui.setServiceInfo(info);
            observe();
            result.putString("workflowNotice", "PASS-session");
            code = Activity.RESULT_OK;
        } catch (RuntimeException | AssertionError | LinkageError failure) {
            result.putString("workflowNotice", "FAIL-" + step);
        } finally {
            if (ui != null) ui.setOnAccessibilityEventListener(null);
        }
        finish(code, result);
    }
}
