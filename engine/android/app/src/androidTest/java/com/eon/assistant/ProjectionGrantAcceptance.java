package com.eon.assistant;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.Activity;
import android.app.Instrumentation;
import android.app.UiAutomation;
import android.content.ComponentName;
import android.os.Build;
import android.os.Bundle;
import android.os.ParcelFileDescriptor;
import android.os.SystemClock;
import android.service.voice.VoiceInteractionService;
import android.text.TextUtils;
import android.view.accessibility.AccessibilityNodeInfo;
import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;

/** External observer: no actions, screen dumps, active-window reads or projection APIs. */
public final class ProjectionGrantAcceptance extends Instrumentation {
    private static final String EON = "com.eon.assistant";
    private String step = "preflight";
    private boolean restart;
    private volatile String observed = "";
    private UiAutomation ui;

    private String matching(String command, String pattern) {
        try (BufferedReader reader = new BufferedReader(new InputStreamReader(
                new ParcelFileDescriptor.AutoCloseInputStream(ui.executeShellCommand(command))))) {
            String found = "";
            for (String value; (value = reader.readLine()) != null; ) {
                if (value.trim().matches(pattern)) {
                    if (!found.isEmpty()) throw new AssertionError();
                    found = value.trim();
                }
            }
            return found;
        } catch (IOException e) { throw new AssertionError(); }
    }

    private void platform(boolean active) {
        if (matching("dumpsys media_projection", active ? "\\(com\\.eon\\.assistant, uid=[0-9]+\\): TYPE_SCREEN_CAPTURE"
                : "null").isEmpty()) throw new AssertionError();
        String service = "dumpsys activity services com.eon.assistant/.ProjectionGrantService";
        if (matching(service, active ? "isForeground=true .*types=0x00000020 .*" : "\\(nothing\\)").isEmpty()) {
            throw new AssertionError();
        }
    }

    @Override public void onCreate(Bundle args) {
        super.onCreate(args);
        restart = args != null && "restart".equals(args.getString("phase"));
        start();
    }

    private void await(String checkpoint, String label) {
        step = checkpoint;
        long end = SystemClock.elapsedRealtime() + 90_000;
        while (SystemClock.elapsedRealtime() < end) {
            if (label.equals(observed)) {
                Bundle status = new Bundle();
                status.putString("checkpoint", checkpoint);
                sendStatus(1, status);
                return;
            }
            SystemClock.sleep(100);
        }
        throw new AssertionError();
    }

    @Override public void onStart() {
        int code = Activity.RESULT_CANCELED;
        Bundle result = new Bundle();
        try {
            if (Build.VERSION.SDK_INT != 35 || !"ranchu".equals(Build.HARDWARE)
                    || !(EON + ".keystoretest").equals(getTargetContext().getPackageName())
                    || !VoiceInteractionService.isActiveService(getTargetContext(),
                    new ComponentName(EON, EON + ".EonVoiceInteractionService"))) throw new AssertionError();
            ui = getUiAutomation(UiAutomation.FLAG_DONT_SUPPRESS_ACCESSIBILITY_SERVICES);
            platform(false);
            AccessibilityServiceInfo info = ui.getServiceInfo();
            info.packageNames = new String[]{EON};
            info.flags |= AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS;
            ui.setServiceInfo(info);
            ui.setOnAccessibilityEventListener(event -> {
                if (!TextUtils.equals(EON, event.getPackageName())) return;
                // Anchor to an EON event, never to whichever window happens to have focus.
                AccessibilityNodeInfo node = event.getSource();
                for (int depth = 0; depth < 16 && node != null && TextUtils.equals(EON, node.getPackageName());
                        depth++, node = node.getParent()) {
                    for (AccessibilityNodeInfo match : node.findAccessibilityNodeInfosByViewId(
                            EON + ":id/projection_grant_status")) {
                        if (TextUtils.equals(EON, match.getPackageName()) && match.isVisibleToUser()
                                && match.getText() != null) {
                            observed = match.getText().toString();
                            return;
                        }
                    }
                }
            });
            await("idle", "No grant. Each session requires a new system approval.");
            await("active", "Grant active. No image or audio output.");
            platform(true);
            String original = matching("pidof com.eon.assistant", "[1-9][0-9]*");
            if (!original.matches("[1-9][0-9]*")) throw new AssertionError();
            if (restart) {
                step = "original-process-dead";
                long until = SystemClock.elapsedRealtime() + 60_000;
                while (original.equals(matching("pidof com.eon.assistant", "[1-9][0-9]*"))) {
                    if (SystemClock.elapsedRealtime() >= until) throw new AssertionError();
                    SystemClock.sleep(100);
                }
                platform(false);
                await("new-idle", "No grant. Each session requires a new system approval.");
                String next = matching("pidof com.eon.assistant", "[1-9][0-9]*");
                if (!next.matches("[1-9][0-9]*") || next.equals(original)) throw new AssertionError();
            } else {
                await("external-stop-callback", "Android stopped the grant (onStop callback).");
                // A second, independently approved no-output owner may still be releasing its grant.
                long until = SystemClock.elapsedRealtime() + 6_000;
                while (matching("dumpsys media_projection", "null").isEmpty()) {
                    if (SystemClock.elapsedRealtime() >= until) throw new AssertionError();
                    SystemClock.sleep(100);
                }
            }
            platform(false);
            await("picker-denied", "Picker canceled. No grant started.");
            platform(false);
            code = Activity.RESULT_OK;
            result.putString("projectionGrant", restart ? "PASS-restart-denied" : "PASS-external-stop-denied");
        } catch (RuntimeException | AssertionError e) {
            result.putString("projectionGrant", "FAIL-" + step);
        }
        finish(code, result);
    }
}
