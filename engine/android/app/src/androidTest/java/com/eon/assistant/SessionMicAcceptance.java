package com.eon.assistant;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.Activity;
import android.app.AppOpsManager;
import android.app.Instrumentation;
import android.app.UiAutomation;
import android.content.Context;
import android.content.pm.PackageManager;
import android.os.Build;
import android.os.Bundle;
import android.os.ParcelFileDescriptor;
import android.os.Process;
import android.os.SystemClock;
import android.text.TextUtils;
import android.view.accessibility.AccessibilityEvent;
import android.view.accessibility.AccessibilityNodeInfo;
import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.util.List;
import java.util.function.BooleanSupplier;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * Manual, read-only observer in the test APK's process. The operator supplies virtual input;
 * this test cannot identify its source. Targeted AppOps snapshots say nothing about sub-poll
 * activity, PCM, recorder object lifetime, or power to a physical microphone.
 */
public final class SessionMicAcceptance extends Instrumentation {
    private static final String EON = "com.eon.assistant";
    private static final String WATCH = "android.permission.WATCH_APPOPS";
    private static final String OFF = "EON · Mic off";
    private static final String START = "Start 6-second microphone meter";
    private static final String STOP = "Stop microphone now";
    private static final String DENIED =
            "Mic off. Use Open EON controls to review microphone permission, then invoke a fresh card.";
    private static final String STOPPED = "Mic off · stopped · levels cleared";
    private static final String NONZERO = "([1-9][0-9]?|100)%";
    private static final Pattern PROGRESS = Pattern.compile(
            "Measuring microphone · ([1-9]|1[0-2])/12 half-second levels · RMS (0|[1-9][0-9]?|100)%");
    private static final Pattern COMPLETE = Pattern.compile(
            "Mic off · 12 levels complete · highest RMS " + NONZERO);
    private static final long MANUAL_MS = 40_000;
    private static final long RELEASE_MS = 1_000;
    private static final long QUIET_MS = 600;
    private static final long MAX_SAMPLE_GAP_MS = 800;

    private Bundle arguments;
    private String mode;
    private String step = "mode";
    private UiAutomation ui;
    private AppOpsManager ops;
    private int uid;
    private int cardWindow;
    private int observedRms;
    private long lastSample, lastInactive, progressUptime;
    private boolean armed;
    private volatile long startClick, stopClick;
    private volatile boolean toggleClicked;

    @Override public void onCreate(Bundle args) {
        super.onCreate(args);
        arguments = args;
        start();
    }

    private static void require(boolean ok) {
        if (!ok) throw new AssertionError();
    }

    private static long now() { return SystemClock.elapsedRealtime(); }

    private void until(String checkpoint, long deadline, BooleanSupplier predicate) {
        step = checkpoint;
        while (now() < deadline) {
            if (predicate.getAsBoolean()) {
                require(now() <= deadline); // A stalled IPC must not extend a release deadline.
                return;
            }
            SystemClock.sleep(50);
        }
        throw new AssertionError();
    }

    private boolean active() {
        // Unlike startWatchingActive, this does not subscribe to other packages' mic activity.
        long before = now();
        boolean running = ops.isOpActive(AppOpsManager.OPSTR_RECORD_AUDIO, uid, EON);
        if (armed) require(now() - lastSample <= MAX_SAMPLE_GAP_MS);
        lastSample = before;
        if (!running) lastInactive = before;
        return running;
    }

    private void watchCardClicks() {
        final int window = cardWindow;
        final long since = SystemClock.uptimeMillis();
        ui.setOnAccessibilityEventListener(event -> {
            long t = event.getEventTime(); // uptime, not elapsedRealtime
            if (t <= since || event.getEventType() != AccessibilityEvent.TYPE_VIEW_CLICKED
                    || event.getWindowId() != window || !TextUtils.equals(EON, event.getPackageName())) return;
            AccessibilityNodeInfo source = event.getSource(0); // no descendant prefetch or event text
            if (source == null || source.getWindowId() != window || !TextUtils.equals(EON, source.getPackageName())) return;
            String id = source.getViewIdResourceName();
            if ((EON + ":id/hud_toggle").equals(id)) toggleClicked = true;
            if (!(EON + ":id/hud_mic").equals(id)) return;
            // View.performClick emits this *after* the click handler. The label may already
            // have changed. Match the fixed control, then corroborate its observed transition.
            if (startClick == 0) startClick = t;
            else stopClick = stopClick == 0 ? t : -1;
        });
    }

    private AccessibilityNodeInfo root() {
        ui.clearCache();
        AccessibilityNodeInfo r = ui.getRootInActiveWindow();
        return r != null && TextUtils.equals(EON, r.getPackageName()) ? r : null;
    }

    private static AccessibilityNodeInfo node(AccessibilityNodeInfo root, String id) {
        if (root == null) return null;
        List<AccessibilityNodeInfo> nodes = root.findAccessibilityNodeInfosByViewId(EON + ":id/" + id);
        if (nodes.isEmpty()) return null;
        require(nodes.size() == 1);
        AccessibilityNodeInfo n = nodes.get(0);
        require(TextUtils.equals(EON, n.getPackageName()) && n.getWindowId() == root.getWindowId());
        return n.isVisibleToUser() ? n : null;
    }

    private static String text(AccessibilityNodeInfo root, String id) {
        AccessibilityNodeInfo n = node(root, id);
        return n == null || n.getText() == null ? "" : n.getText().toString();
    }

    private static boolean idleControls(AccessibilityNodeInfo r) {
        return OFF.equals(text(r, "hud_title")) && "Session open".equals(text(r, "hud_status"))
                && START.equalsIgnoreCase(text(r, "hud_mic"));
    }

    private static boolean cleanExpanded(AccessibilityNodeInfo r) {
        return node(r, "hud_mic_status") == null && node(r, "hud_mic_graph") == null && idleControls(r);
    }

    private boolean sameCard(AccessibilityNodeInfo r) {
        return r != null && r.getWindowId() == cardWindow;
    }

    private boolean terminal(AccessibilityNodeInfo r) {
        String s = sameCard(r) ? text(r, "hud_mic_status") : "";
        if (!"complete".equals(mode)) {
            Matcher level = PROGRESS.matcher(s);
            if (level.matches()) require(Integer.parseInt(level.group(1)) <= 6);
        }
        if ("hide".equals(mode)) {
            // Invoke over the EON controls activity. Null, launcher, a collapsed card, or even
            // another EON window with no mic text is NOT evidence that the card was dismissed.
            if (sameCard(r)) {
                AccessibilityNodeInfo toggle = node(r, "hud_toggle");
                require(!toggleClicked && (toggle == null || !TextUtils.equals("Show session details", toggle.getContentDescription()))
                        && !OFF.equals(text(r, "hud_title")) && !START.equalsIgnoreCase(text(r, "hud_mic"))
                        && !s.startsWith("Mic off"));
                return false;
            }
            return node(r, "hud_title") == null && node(r, "assistant_role_status") != null;
        }
        if (!sameCard(r) || !idleControls(r)) return false;
        require(!s.startsWith("Mic off · measurement unavailable"));
        if ("complete".equals(mode)) {
            Matcher peak = COMPLETE.matcher(s);
            return peak.matches() && Integer.parseInt(peak.group(1)) >= observedRms
                    && node(r, "hud_mic_graph") != null;
        }
        return ("stop".equals(mode) ? STOPPED : DENIED).equals(s) && node(r, "hud_mic_graph") == null;
    }

    private void finishTake(long start, long deadline) {
        step = mode;
        long lastActive = now();
        boolean ended = false;
        while (now() < deadline) {
            if (active()) {
                require(!ended); // Don't join evidence from two separate operations.
                lastActive = now();
            } else ended = true;
            boolean done = terminal(root());
            require(now() <= deadline && now() - lastActive <= RELEASE_MS);
            if (done) {
                // Allow sampling jitter, but not one short capture followed by a UI-only timer.
                if ("complete".equals(mode)) require(lastActive - start >= 5_000);
                if (!ended) until("record-op-release", Math.min(deadline, now() + RELEASE_MS), () -> {
                    require(terminal(root()));
                    return !active();
                });
                return;
            }
            SystemClock.sleep(50);
        }
        throw new AssertionError();
    }

    private void fresh() {
        until("fresh-collapsed", now() + MANUAL_MS, () -> {
            require(!active());
            AccessibilityNodeInfo r = root();
            AccessibilityNodeInfo toggle = node(r, "hud_toggle");
            if (toggle == null) return false;
            require(TextUtils.equals("Show session details", toggle.getContentDescription()));
            if (!OFF.equals(text(r, "hud_title")) || !"Session open".equals(text(r, "hud_status"))
                    || node(r, "hud_mic") != null) return false;
            cardWindow = r.getWindowId();
            return true;
        });
        until("fresh-expanded", now() + MANUAL_MS, () -> {
            require(!active());
            AccessibilityNodeInfo r = root();
            require(sameCard(r));
            if (node(r, "hud_mic") == null) return false;
            require(cleanExpanded(r) && !active());
            return true;
        });
    }

    private int eonPid() {
        // The shipping components share EON's default process. Query no other process's data.
        try (BufferedReader r = new BufferedReader(new InputStreamReader(
                new ParcelFileDescriptor.AutoCloseInputStream(ui.executeShellCommand("pidof " + EON))))) {
            String pid = r.readLine();
            if (pid == null) return 0;
            require(pid.matches("[1-9][0-9]*") && r.readLine() == null);
            return Integer.parseInt(pid);
        } catch (IOException e) { throw new AssertionError(e); }
    }

    private void processDeath(int original, long deadline) {
        step = "active-original-process";
        require(eonPid() == original && active());
        int oldWindow = cardWindow;
        // The operator sends SIGKILL as the debug app's UID. Stop, hide, or natural completion
        // in a still-live process is not evidence that death stopped a running take.
        until("active-process-death", deadline, () -> {
            boolean running = active();
            if (eonPid() != original) return true;
            require(running);
            return false;
        });
        until("death-record-op-release", now() + RELEASE_MS, () -> !active());
        require(startClick > 0 && !toggleClicked && stopClick == 0);
        ui.setOnAccessibilityEventListener(null);
        until("old-card-gone", now() + RELEASE_MS, () -> {
            require(!active());
            return !sameCard(root());
        });
        fresh(); // Poll for inactivity through the operator's next invocation and expansion.
        step = "replacement-process";
        int replacement = eonPid();
        require(replacement > 0 && replacement != original && cardWindow != oldWindow);
        require(getTargetContext().getPackageManager().checkPermission(android.Manifest.permission.RECORD_AUDIO,
                EON) == PackageManager.PERMISSION_GRANTED);
        long quiet = now() + QUIET_MS;
        until("replacement-idle", quiet + RELEASE_MS, () -> {
            require(!active());
            AccessibilityNodeInfo r = root();
            require(sameCard(r) && cleanExpanded(r));
            return now() >= quiet;
        });
        startClick = stopClick = 0;
        toggleClicked = false;
        watchCardClicks();
        until("replacement-start-denied", now() + MANUAL_MS, () -> {
            require(!active());
            AccessibilityNodeInfo r = root();
            require(sameCard(r));
            return startClick > 0 && terminal(r); // Denied despite retained OS permission.
        });
        require(eonPid() == replacement);
    }

    private void observe() {
        until("idle-card", now() + MANUAL_MS, () -> {
            require(!active());
            AccessibilityNodeInfo r = root();
            if (!cleanExpanded(r)) return false;
            cardWindow = r.getWindowId();
            return true;
        });
        boolean death = "process-death".equals(mode);
        int original = death ? eonPid() : 0;
        if (death) require(original > 0);
        require(!active()); // start timing from a verified clean, inactive card
        watchCardClicks();
        armed = true;
        if ("denied".equals(mode)) {
            until("denied", now() + MANUAL_MS, () -> {
                require(!active());
                AccessibilityNodeInfo r = root();
                require(sameCard(r));
                return terminal(r);
            });
        } else {
            until("record-op-active", now() + MANUAL_MS, () -> {
                if (active()) return true;
                AccessibilityNodeInfo r = root();
                require(sameCard(r) && !text(r, "hud_mic_status").startsWith("Mic off"));
                return false;
            });
            // For Stop/hide, finish well before a six-second take could end naturally. Neither
            // waiting out a leak nor observing a completed card and THEN dismissing should pass.
            long start = now(), activeUptime = SystemClock.uptimeMillis();
            long end = lastInactive + ("complete".equals(mode) ? 8_000 : 3_800);
            until("nonzero-progress", end, () -> {
                require(active());
                if (startClick == 0) {
                    require(now() - start <= MAX_SAMPLE_GAP_MS);
                    return false;
                }
                require(stopClick == 0 && Math.abs(startClick - activeUptime) <= MAX_SAMPLE_GAP_MS);
                AccessibilityNodeInfo r = root();
                require(sameCard(r));
                Matcher rms = PROGRESS.matcher(text(r, "hud_mic_status"));
                if (!rms.matches() || "0".equals(rms.group(2)) || !"EON · Mic on".equals(text(r, "hud_title"))
                        || !STOP.equalsIgnoreCase(text(r, "hud_mic")) || node(r, "hud_mic_graph") == null) return false;
                require(Integer.parseInt(rms.group(1)) <= ("complete".equals(mode) ? 11 : 3));
                require(active());
                observedRms = Integer.parseInt(rms.group(2));
                progressUptime = SystemClock.uptimeMillis();
                return true;
            });
            if (death) processDeath(original, end);
            else finishTake(start, end);
        }
        long terminalUptime = SystemClock.uptimeMillis();
        long quiet = now() + QUIET_MS;
        until("inactive-hold", quiet + RELEASE_MS, () -> {
            require(!active() && terminal(root()) && !active());
            return now() >= quiet;
        });
        step = "card-click";
        require(startClick > 0 && startClick <= terminalUptime && !toggleClicked);
        require("stop".equals(mode) ? stopClick >= progressUptime && stopClick <= terminalUptime : stopClick == 0);
        armed = false;
        ui.setOnAccessibilityEventListener(null);
        if ("hide".equals(mode)) fresh();
    }

    @Override public void onStart() {
        Bundle result = new Bundle();
        int code = Activity.RESULT_CANCELED;
        try {
            require(arguments != null && arguments.size() == 1);
            mode = arguments.getString("mode");
            require("denied".equals(mode) || "complete".equals(mode) || "stop".equals(mode) || "hide".equals(mode)
                    || ("process-death".equals(mode) && Build.VERSION.SDK_INT >= 35));
            step = "preflight";
            Context test = getTargetContext();
            require(Build.VERSION.SDK_INT >= 34 && ("ranchu".equals(Build.HARDWARE) || "goldfish".equals(Build.HARDWARE))
                    && (EON + ".keystoretest").equals(test.getPackageName()));
            uid = test.getPackageManager().getPackageUid(EON, 0);
            require(uid >= 0 && uid != Process.myUid());
            ops = test.getSystemService(AppOpsManager.class);
            require(ops != null);
            ui = getUiAutomation(UiAutomation.FLAG_DONT_SUPPRESS_ACCESSIBILITY_SERVICES);
            AccessibilityServiceInfo info = ui.getServiceInfo();
            info.packageNames = new String[]{EON};
            info.eventTypes = AccessibilityEvent.TYPE_VIEW_CLICKED;
            info.notificationTimeout = 0;
            info.flags |= AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS
                    | AccessibilityServiceInfo.FLAG_INCLUDE_NOT_IMPORTANT_VIEWS; // the level graph
            ui.setServiceInfo(info);
            step = "watch-permission";
            ui.adoptShellPermissionIdentity(WATCH);
            try {
                // AOSP isOperationActive silently returns false for unauthorized cross-UID reads.
                require(test.checkSelfPermission(WATCH) == PackageManager.PERMISSION_GRANTED);
                observe();
            } finally {
                try { ui.setOnAccessibilityEventListener(null); }
                finally { ui.dropShellPermissionIdentity(); }
            }
            result.putString("micHud", "PASS-" + mode);
            code = Activity.RESULT_OK;
        } catch (PackageManager.NameNotFoundException | RuntimeException | AssertionError | LinkageError failure) {
            result.putString("micHud", "FAIL-" + step);
        }
        finish(code, result);
    }
}
