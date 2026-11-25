package com.eon.assistant;

import android.app.Activity;
import android.app.Instrumentation;
import android.app.role.RoleManager;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.ApplicationInfo;
import android.content.pm.ResolveInfo;
import android.os.Bundle;
import android.os.CancellationSignal;
import android.os.SystemClock;
import android.service.voice.VoiceInteractionService;
import android.view.inspector.WindowInspector;
import android.widget.Button;
import android.widget.TextView;
import java.util.concurrent.Callable;
import java.util.concurrent.FutureTask;

/** Unselected stock AVD only. Cancel our own real activity-for-result; never operate the picker UI. */
public final class AssistantActivityAcceptance extends Instrumentation {
    private AssistantActivity activity;

    @Override public void onCreate(Bundle args) { super.onCreate(args); start(); }

    private static void check(boolean ok) { if (!ok) throw new AssertionError(); }

    private <T> T main(Callable<T> read) throws Exception {
        FutureTask<T> task = new FutureTask<>(read);
        runOnMainSync(task);
        return task.get();
    }

    private void await(Callable<Boolean> read) throws Exception {
        long start = SystemClock.elapsedRealtime();
        while (!main(read)) {
            check(SystemClock.elapsedRealtime() - start < 8000);
            SystemClock.sleep(40);
        }
    }

    private int running() {
        OwnActivityTasks.Counts counts = OwnActivityTasks.INSTANCE.read(getTargetContext(), new CancellationSignal());
        check(counts != null);
        return counts.getRunning();
    }

    private boolean unselected() {
        Context context = getTargetContext();
        RoleManager roles = context.getSystemService(RoleManager.class);
        return roles.isRoleAvailable(RoleManager.ROLE_ASSISTANT) && !roles.isRoleHeld(RoleManager.ROLE_ASSISTANT)
                && !VoiceInteractionService.isActiveService(context, new ComponentName(context, EonVoiceInteractionService.class));
    }

    @Override public void onStart() {
        String step = "preflight";
        int code = Activity.RESULT_CANCELED;
        Bundle result = new Bundle();
        ActivityMonitor monitor = null;
        try {
            check(unselected());
            Context context = getTargetContext();
            int before = running();
            // ACTION_ASSIST resolved by Android to EON's *activity*, not a selected assistant session.
            step = "assist-activity";
            activity = (AssistantActivity) startActivitySync(new Intent(Intent.ACTION_ASSIST)
                    .setPackage(context.getPackageName())
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_MULTIPLE_TASK));
            await(() -> activity.hasWindowFocus() && running() == before + 1);
            step = "picker-launch";
            Intent picker = context.getSystemService(RoleManager.class).createRequestRoleIntent(RoleManager.ROLE_ASSISTANT);
            ResolveInfo resolver = context.getPackageManager().resolveActivity(picker, 0);
            check(resolver != null && (resolver.activityInfo.applicationInfo.flags & ApplicationInfo.FLAG_SYSTEM) != 0);
            monitor = addMonitor(new IntentFilter(picker.getAction()), null, false);
            ActivityMonitor observer = monitor;
            main(() -> {
                Button request = activity.findViewById(R.id.assistant_role_request);
                check(request.isEnabled() && request.performClick());
                return null;
            });
            await(() -> observer.getHits() == 1 && !activity.hasWindowFocus());
            step = "picker-cancel";
            runOnMainSync(() -> activity.finishActivity(AssistantActivityKt.ASSISTANT_ROLE_REQUEST));
            await(() -> activity.hasWindowFocus() && activity.getString(R.string.voice_role_canceled)
                    .contentEquals(((TextView) activity.findViewById(R.id.assistant_role_status)).getText()));
            check(unselected() && main(() -> WindowInspector.getGlobalWindowViews().stream()
                    .noneMatch(root -> root.findViewById(R.id.hud_panel) != null)));
            step = "task-removed";
            runOnMainSync(activity::finishAndRemoveTask);
            await(() -> running() == before);
            activity = null;
            result.putString("assistantActivity", "PASS-picker-canceled");
            code = Activity.RESULT_OK;
        } catch (Exception | AssertionError | LinkageError failure) {
            result.putString("assistantActivity", "FAIL-" + step);
        } finally {
            if (monitor != null) removeMonitor(monitor);
            if (activity != null) runOnMainSync(activity::finishAndRemoveTask);
            finish(code, result);
        }
    }
}
