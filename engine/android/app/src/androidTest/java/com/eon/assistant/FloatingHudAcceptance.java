package com.eon.assistant;

import android.app.Activity;
import android.app.AppOpsManager;
import android.app.Instrumentation;
import android.app.Notification;
import android.app.NotificationManager;
import android.content.Context;
import android.hardware.input.InputManager;
import android.os.Build;
import android.os.Bundle;
import android.os.Process;
import android.os.SystemClock;
import android.provider.Settings;
import android.service.notification.StatusBarNotification;
import android.view.View;
import android.view.WindowManager;
import android.view.inspector.WindowInspector;
import android.widget.TextView;
import java.util.concurrent.Callable;
import java.util.concurrent.FutureTask;

/** Observe actual app windows and the ongoing notification. Never grant, click, or send an Intent. */
public final class FloatingHudAcceptance extends Instrumentation {
    private String step = "preflight";
    private Context app;
    private int idleWidth;
    private boolean micPhase;
    private int peak;
    private int liveSamples;

    @Override public void onCreate(Bundle args) {
        super.onCreate(args);
        micPhase = args != null && "mic".equals(args.getString("mode"));
        start();
    }

    private static void require(boolean passed) { if (!passed) throw new AssertionError(); }

    private View card() {
        View found = null;
        for (View root : WindowInspector.getGlobalWindowViews()) {
            if (root.findViewById(R.id.floating_status) == null) continue;
            require(found == null);
            found = root;
        }
        return found;
    }

    private boolean notice() {
        for (StatusBarNotification n : app.getSystemService(NotificationManager.class).getActiveNotifications()) {
            Notification note = n.getNotification();
            if (!"eon.floating.status".equals(note.getChannelId())) continue;
            require((note.flags & Notification.FLAG_FOREGROUND_SERVICE) != 0 && note.actions.length >= 1);
            return true;
        }
        return false;
    }

    private void await(String checkpoint, Callable<Boolean> condition) throws Exception {
        step = checkpoint;
        Bundle waiting = new Bundle();
        waiting.putString("awaiting", checkpoint);
        sendStatus(1, waiting);
        long end = SystemClock.elapsedRealtime() + 90_000;
        while (SystemClock.elapsedRealtime() < end) {
            FutureTask<Boolean> test = new FutureTask<>(condition);
            runOnMainSync(test);
            if (!micPhase) require(!recording());
            if (test.get()) return;
            SystemClock.sleep(120);
        }
        throw new AssertionError();
    }

    private boolean recording() {
        return app.getSystemService(AppOpsManager.class).isOpActive(
                AppOpsManager.OPSTR_RECORD_AUDIO, Process.myUid(), app.getPackageName());
    }

    private boolean liveLevel() {
        View root = card();
        if (root == null || !root.isShown() || !notice() || !recording()) return false;
        ToneGraph graph = root.findViewById(R.id.floating_meter);
        if (!graph.isShown() || graph.getLevels().isEmpty()) return false;
        int last = graph.getLevels().get(graph.getLevels().size() - 1);
        liveSamples++;
        if (liveSamples == 1) {
            Bundle stats = new Bundle();
            stats.putInt("firstObservedLiveRms", last);
            sendStatus(1, stats);
        }
        return last > 0 && last <= 100 && app.getString(R.string.floating_mic_level, last)
                .contentEquals(((TextView)root.findViewById(R.id.floating_status)).getText());
    }

    private void observeMic() throws Exception {
        await("mic-unreviewed", () -> !recording() && !MicPermission.INSTANCE.allowed(app));
        await("mic-reviewed-off", () -> MicPermission.INSTANCE.allowed(app) && !recording());
        await("mic-live", this::liveLevel);
        await("mic-finished", () -> {
            View root = card();
            if (root == null || !root.isShown() || !notice() || recording()) return false;
            ToneGraph graph = root.findViewById(R.id.floating_meter);
            if (!graph.isShown() || graph.getLevels().size() != 12) return false;
            peak = java.util.Collections.max(graph.getLevels());
            return peak > 0 && peak <= 100 && app.getString(R.string.floating_mic_done, peak)
                    .contentEquals(((TextView)root.findViewById(R.id.floating_status)).getText());
        });
        await("mic-live-again", () -> liveLevel() &&
                ((ToneGraph)card().findViewById(R.id.floating_meter)).getLevels().size() <= 2);
        long stopBy = SystemClock.elapsedRealtime() + 4550; // before the sixth second
        await("mic-early-stop", () -> SystemClock.elapsedRealtime() < stopBy && !recording() && !notice() && card() == null &&
                app.getSystemService(android.os.PowerManager.class).isInteractive());
        await("mic-live-third", () -> liveLevel() &&
                ((ToneGraph)card().findViewById(R.id.floating_meter)).getLevels().size() <= 2);
        long lockBy = SystemClock.elapsedRealtime() + 4550;
        await("mic-screen-off", () -> SystemClock.elapsedRealtime() < lockBy && !recording() && !notice() && card() == null &&
                !app.getSystemService(android.os.PowerManager.class).isInteractive());
    }

    private boolean idle() {
        View root = card();
        if (root == null || !root.isShown() || !root.isLaidOut() || !notice()) return false;
        WindowManager.LayoutParams p = (WindowManager.LayoutParams) root.getLayoutParams();
        int flags = WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE | WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE
                | WindowManager.LayoutParams.FLAG_SECURE;
        float dp = app.getResources().getDisplayMetrics().density;
        require(p.type == WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY && (p.flags & flags) == flags
                && p.width == -2 && p.height == -2 && p.alpha > 0 && p.alpha <= app.getSystemService(InputManager.class)
                .getMaximumObscuringOpacityForTouch() && root.getWidth() > 0 && root.getWidth() < dp * 210
                && root.getHeight() > 0 && root.getHeight() < dp * 100 && !root.hasWindowFocus());
        idleWidth = root.getWidth();
        return app.getString(R.string.floating_idle).contentEquals(((TextView)root.findViewById(R.id.floating_status)).getText())
                && !root.findViewById(R.id.floating_meter).isShown();
    }

    private void observe() throws Exception {
        await("granted-but-off", () -> Settings.canDrawOverlays(app) &&
                app.getSystemService(NotificationManager.class).areNotificationsEnabled() && !notice() && card() == null);
        await("started-hidden-in-controls", () -> notice() && card() == null && WindowInspector.getGlobalWindowViews().stream()
                .anyMatch(root -> root.hasWindowFocus() && root.getContext() != null && root.isShown()));
        await("idle-outside-activity", this::idle);
        await("acknowledged-task", () -> {
            View root = card();
            return root != null && root.isShown() && notice() && app.getString(R.string.voice_hud_checked, 1)
                    .contentEquals(((TextView)root.findViewById(R.id.floating_status)).getText());
        });
        await("real-tone-levels", () -> {
            View root = card();
            if (root == null || !root.isShown() || !notice()) return false;
            ToneGraph graph = root.findViewById(R.id.floating_meter);
            if (!graph.isShown() || graph.getLevels().isEmpty()) return false;
            int peak = java.util.Collections.max(graph.getLevels());
            return peak > 0 && peak <= 100 && app.getString(R.string.floating_tone_levels, peak)
                    .contentEquals(((TextView)root.findViewById(R.id.floating_status)).getText());
        });
        await("stopped", () -> !notice() && card() == null);
        await("fresh-idle", this::idle);
        await("off-on-screen-off", () -> !app.getSystemService(android.os.PowerManager.class).isInteractive()
                && !notice() && card() == null);
    }

    @Override public void onStart() {
        Bundle result = new Bundle();
        int code = Activity.RESULT_CANCELED;
        try {
            app = getTargetContext();
            require(Build.VERSION.SDK_INT == 35 && ("ranchu".equals(Build.HARDWARE) || "goldfish".equals(Build.HARDWARE))
                    && "com.eon.assistant".equals(app.getPackageName()));
            if (micPhase) {
                observeMic();
                result.putString("floatingHud", "PASS-mic");
                result.putInt("peakRmsPercent", peak);
            } else {
                observe();
                result.putString("floatingHud", "PASS-operator");
                result.putInt("idleWidthPx", idleWidth);
            }
            code = Activity.RESULT_OK;
        } catch (Exception | AssertionError | LinkageError failure) {
            result.putString("floatingHud", "FAIL-" + step);
        }
        finish(code, result);
    }
}
