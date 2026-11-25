package com.eon.assistant;

import android.app.Activity;
import android.app.Instrumentation;
import android.app.role.RoleManager;
import android.content.ComponentName;
import android.content.Context;
import android.os.Bundle;
import android.os.CancellationSignal;
import android.os.OperationCanceledException;
import android.os.SystemClock;
import android.service.voice.VoiceInteractionService;
import android.view.ContextThemeWrapper;
import android.view.View;
import android.view.inspector.WindowInspector;
import android.widget.TextView;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.FutureTask;

/** Stock decoder + denied authority, or a read-only observer of a human-invoked, real OS session. */
public final class VoiceHudAcceptance extends Instrumentation {
    private String mode;
    private String step = "mode";

    @Override public void onCreate(Bundle args) {
        super.onCreate(args);
        mode = args == null ? null : args.getString("mode");
        start();
    }

    private static void check(boolean ok) {
        if (!ok) throw new AssertionError();
    }

    private boolean active() {
        Context context = getTargetContext();
        return VoiceInteractionService.isActiveService(context, new ComponentName(context, EonVoiceInteractionService.class));
    }

    private boolean held() {
        return getTargetContext().getSystemService(RoleManager.class).isRoleHeld(RoleManager.ROLE_ASSISTANT);
    }

    private View panel() {
        for (View root : WindowInspector.getGlobalWindowViews()) {
            View panel = root.findViewById(R.id.hud_panel);
            if (panel != null && panel.isShown()) return panel;
        }
        return null;
    }

    private <T> T main(Callable<T> read) throws Exception {
        FutureTask<T> task = new FutureTask<>(read);
        runOnMainSync(task);
        return task.get();
    }

    private void await(Callable<Boolean> read) throws Exception {
        long start = SystemClock.elapsedRealtime();
        while (!main(read)) {
            check(SystemClock.elapsedRealtime() - start < 25_000);
            SystemClock.sleep(80);
        }
    }

    private void stock() throws Exception {
        step = "stock-unselected";
        check(!held() && !active() && main(() -> panel() == null));
        step = "stock-tone-decode";
        List<Integer> levels = new ArrayList<>();
        StockToneReader.INSTANCE.read(getTargetContext(), new CancellationSignal(), levels::add);
        check(!levels.isEmpty() && levels.size() <= 12 && Collections.max(levels) > 0);
        for (int level : levels) check(level >= 0 && level <= 100);
        step = "stock-presentation";
        main(() -> {
            // An unattached production view, NOT a voice session or a simulated OS binding.
            Context theme = new ContextThemeWrapper(getTargetContext(), android.R.style.Theme_Material_Light);
            Runnable unused = () -> { throw new AssertionError(); };
            VoiceHudView view = new VoiceHudView(theme, unused, unused, unused, unused, unused, unused, unused);
            ToneGraph graph = view.findViewById(R.id.hud_tone_graph);
            TextView status = view.findViewById(R.id.hud_tone_status);
            view.render(new VoiceHudFrame(true, ProfileCheck.Idle.INSTANCE,
                    new ToneScan.Reading(new ToneScan.Request(), levels.subList(0, 1)), null));
            check(graph.getLevels().size() == 1 && !view.findViewById(R.id.hud_tone_scan).isEnabled());
            check(theme.getString(R.string.voice_tone_progress, 1, 12).contentEquals(status.getText()));
            view.render(new VoiceHudFrame(true, ProfileCheck.Idle.INSTANCE, new ToneScan.Complete(levels), null));
            check(graph.getLevels().equals(levels) && graph.getVisibility() == View.VISIBLE);
            check(theme.getString(R.string.voice_tone_complete, levels.size(), Collections.max(levels))
                    .contentEquals(status.getText()));
            view.render(null);
            check(graph.getLevels().isEmpty() && view.findViewById(R.id.hud_panel).getVisibility() == View.GONE);
            return null;
        });
        step = "stock-tone-revocation";
        CancellationSignal stop = new CancellationSignal();
        int[] callbacks = {0};
        try {
            StockToneReader.INSTANCE.read(getTargetContext(), stop, level -> { callbacks[0]++; stop.cancel(); });
            throw new AssertionError();
        } catch (OperationCanceledException expected) {
            check(callbacks[0] == 1);
        }
        check(!held() && !active() && main(() -> panel() == null));
    }

    private boolean idle(View panel) {
        if (panel == null) return false;
        ToneGraph graph = panel.findViewById(R.id.hud_tone_graph);
        return !graph.isShown() && graph.getLevels().isEmpty() && !panel.findViewById(R.id.hud_details).isShown();
    }

    private void operatorSession() throws Exception {
        step = "operator-role";
        check(held() && active());
        step = "user-invocation";
        await(() -> idle(panel()));
        step = "user-authorized-completion";
        await(() -> {
            View panel = panel();
            if (panel == null) return false;
            ToneGraph graph = panel.findViewById(R.id.hud_tone_graph);
            List<Integer> levels = graph.getLevels();
            if (!graph.isShown() || levels.isEmpty() || levels.size() > 12) return false;
            String expected = getTargetContext().getString(R.string.voice_tone_complete,
                    levels.size(), Collections.max(levels));
            TextView status = panel.findViewById(R.id.hud_tone_status);
            return expected.contentEquals(status.getText()) && panel.findViewById(R.id.hud_tone_clear).isShown();
        });
        step = "user-dismissal";
        await(() -> panel() == null);
        step = "fresh-invocation";
        await(() -> idle(panel()));
        check(held() && active());
    }

    @Override public void onStart() {
        int code = Activity.RESULT_CANCELED;
        Bundle result = new Bundle();
        try {
            if ("stock".equals(mode)) stock();
            else if ("session".equals(mode)) operatorSession();
            else throw new AssertionError();
            result.putString("voiceHud", "PASS-" + mode);
            code = Activity.RESULT_OK;
        } catch (Exception | AssertionError | LinkageError failure) {
            result.putString("voiceHud", "FAIL-" + step);
        }
        finish(code, result);
    }
}
