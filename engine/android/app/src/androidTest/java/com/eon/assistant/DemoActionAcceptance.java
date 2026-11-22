package com.eon.assistant;

import android.app.Activity;
import android.app.Instrumentation;
import android.content.Intent;
import android.os.Build;
import android.os.Bundle;
import android.os.SystemClock;
import android.view.View;
import android.view.ViewGroup;
import android.view.accessibility.AccessibilityNodeInfo;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.FutureTask;

/**
 * Same-signer device test, built by keystore_device_test.py --build-only. On a disposable,
 * unlocked API 34+ device install both APKs, then run in full screen:
 * adb shell am instrument -w -e mode denied com.eon.assistant.keystoretest/com.eon.assistant.DemoActionAcceptance
 * Repeat with "allowed" after the operator selects EON as assistant and enables its service
 * and shortcut. The test drives only the demo's own buttons. It never grants a role, injects
 * input coordinates, opens settings, or substitutes test-only eligibility in the app.
 */
public final class DemoActionAcceptance extends Instrumentation {
    private String mode;
    private DemoActionActivity activity;
    private View cover;
    private int nativeCalls;

    @Override public void onCreate(Bundle args) {
        super.onCreate(args);
        mode = args == null ? null : args.getString("mode");
        start();
    }

    private static void check(boolean ok) {
        if (!ok) throw new AssertionError("demo assertion failed");
    }

    private <T> T main(Callable<T> read) throws Exception {
        FutureTask<T> task = new FutureTask<>(read);
        runOnMainSync(task);
        return task.get();
    }

    private void await(Callable<Boolean> condition) throws Exception {
        long start = SystemClock.elapsedRealtime();
        while (!main(condition)) {
            check(SystemClock.elapsedRealtime() - start < 5000L);
            SystemClock.sleep(25L);
        }
    }

    private Button button(int id) { return activity.findViewById(id); }
    private void tap(int id) {
        Button button = button(id);
        check(button.isShown() && button.isEnabled() && button.performClick());
    }
    private String count() { return button(R.id.demo_counter).getText().toString(); }
    private ScrollView samples() { return activity.findViewById(R.id.demo_samples); }
    private String status() { return ((TextView) activity.findViewById(R.id.demo_status)).getText().toString(); }
    private boolean reviewing() { return button(R.id.demo_confirm).getVisibility() == View.VISIBLE; }
    private void checkFieldLookup() throws Exception {
        DemoSampleField field = main(() -> activity.findViewById(R.id.demo_field));
        View label = main(() -> activity.findViewById(R.id.demo_field_label));
        LinearLayout group = main(() -> (LinearLayout) field.getParent());
        DemoFieldLookup lookup = new DemoFieldLookup(group, label, field);
        main(() -> {
            check(!field.isSaveEnabled() && !field.isFocusable());
            check(field.onCreateInputConnection(new android.view.inputmethod.EditorInfo()) == null);
            check(field.createAccessibilityNodeInfo().getActionList().stream().noneMatch(
                    a -> a.getId() == AccessibilityNodeInfo.ACTION_SET_TEXT));
            check(!field.performAccessibilityAction(AccessibilityNodeInfo.ACTION_SET_TEXT, null));
            field.requestRectangleOnScreen(new android.graphics.Rect(0, 0, field.getWidth(), field.getHeight()), true);
            return null;
        });
        await(lookup::present);
        main(() -> {
            group.removeView(field);
            group.addView(field, 0);
            return null;
        });
        await(lookup::present); // Previously last, now first in the real attached hierarchy.
        main(() -> {
            field.setAccessibilityDataSensitive(View.ACCESSIBILITY_DATA_SENSITIVE_YES);
            check(!lookup.present());
            field.setAccessibilityDataSensitive(View.ACCESSIBILITY_DATA_SENSITIVE_AUTO);
            field.setAccessibilityDelegate(new View.AccessibilityDelegate() {
                @Override public void onInitializeAccessibilityNodeInfo(View host, AccessibilityNodeInfo info) {
                    super.onInitializeAccessibilityNodeInfo(host, info);
                    info.setClassName("android.widget.TextView");
                }
            });
            check(!lookup.present());
            field.setAccessibilityDelegate(null);
            check(lookup.present());
            group.removeView(field);
            group.addView(new DemoSampleField(activity), 0); // Same class/ID, not the retained field.
            check(!lookup.present());
            return null;
        });
    }
    private Integer visibleOffset() {
        LinearLayout list = (LinearLayout) samples().getChildAt(0);
        List<TextView> rows = new ArrayList<>();
        for (int i = 0; i < list.getChildCount(); i++) rows.add((TextView) list.getChildAt(i));
        return DemoSamplePosition.INSTANCE.read(samples(), list, rows);
    }
    private void reviewLocal(int id) {
        tap(id);
        check(reviewing() && button(R.id.demo_confirm).getText().toString().contains("local hit fallback"));
    }

    @Override public void onStart() {
        String step = "startup";
        Bundle result = new Bundle();
        int code = Activity.RESULT_CANCELED;
        try {
            check(Build.VERSION.SDK_INT >= 34 && ("denied".equals(mode) || "allowed".equals(mode)));
            activity = (DemoActionActivity) startActivitySync(new Intent(getTargetContext(), DemoActionActivity.class)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
            step = "foreground";
            await(activity::hasWindowFocus);
            if ("denied".equals(mode)) {
                step = "authority-denied";
                main(() -> {
                    check(!DemoShortcut.INSTANCE.eligible(activity));
                    for (int i = 0; i < 2; i++) {
                        if (i == 1) tap(R.id.demo_sample_mode);
                        for (int review : new int[]{R.id.demo_review_click, R.id.demo_review_scroll}) {
                            tap(review);
                            check(!reviewing());
                            button(R.id.demo_confirm).performClick();
                            check("Demo count: 0".equals(count()) && samples().getScrollY() == 0);
                        }
                    }
                    return null;
                });
                // Ordinary local widget input in reduced-provider mode, NOT an approved action.
                // Verify the read-only postcondition witness on a real, visible Android hierarchy.
                step = "visible-samples";
                main(() -> {
                    check(Integer.valueOf(0).equals(visibleOffset()));
                    tap(R.id.demo_counter);
                    check("Demo count: 1".equals(count()));
                    check(samples().arrowScroll(View.FOCUS_DOWN));
                    return null;
                });
                await(() -> visibleOffset() != null && visibleOffset() > 0);
                main(() -> {
                    samples().getChildAt(0).setVisibility(View.INVISIBLE);
                    check(visibleOffset() == null && !reviewing() && !DemoShortcut.INSTANCE.eligible(activity));
                    return null;
                });
                step = "local-node-discovery";
                main(() -> {
                    // This is a read-only test on the demo's own two widgets, not an action grant.
                    ScrollView scroll = samples();
                    Button counter = button(R.id.demo_counter);
                    LinearLayout group = (LinearLayout) counter.getParent();
                    group.removeView(counter);
                    group.addView(counter); // counter is now last; neither lookup may rely on a slot.
                    check(DemoNodeLookup.INSTANCE.findView(group, DemoAction.CLICK_COUNTER) == counter);
                    check(DemoNodeLookup.INSTANCE.findView(group, DemoAction.SCROLL_SAMPLES_FORWARD) == scroll);
                    scroll.setId(R.id.demo_counter); // mismatched class/ambiguous ID
                    check(DemoNodeLookup.INSTANCE.findView(group, DemoAction.CLICK_COUNTER) == null);
                    scroll.setId(R.id.demo_samples);
                    return null;
                });
                step = "field-lookup";
                main(() -> {
                    tap(R.id.demo_locate_field);
                    check(status().startsWith("Sample field unavailable.") && !reviewing());
                    return null;
                });
                checkFieldLookup();
            } else {
                step = "operator-prerequisites";
                await(() -> DemoShortcut.INSTANCE.eligible(activity));
                step = "cancel-and-click";
                main(() -> {
                    tap(R.id.demo_review_click);
                    check(reviewing() && "Demo count: 0".equals(count()));
                    tap(R.id.demo_cancel);
                    check(!reviewing() && "Demo count: 0".equals(count()));
                    tap(R.id.demo_review_click);
                    tap(R.id.demo_confirm);
                    check(!reviewing() && "Demo count: 1".equals(count()));
                    check(status().startsWith("Verified:") && status().contains("native action"));
                    button(R.id.demo_confirm).performClick();
                    check("Demo count: 1".equals(count()));
                    return null;
                });
                step = "changed-target";
                main(() -> {
                    tap(R.id.demo_review_click);
                    button(R.id.demo_counter).setEnabled(false);
                    tap(R.id.demo_confirm);
                    check(!reviewing() && "Demo count: 1".equals(count()));
                    button(R.id.demo_counter).setEnabled(true);
                    return null;
                });
                step = "native-scroll";
                main(() -> {
                    tap(R.id.demo_review_scroll);
                    check(reviewing() && samples().getScrollY() == 0);
                    tap(R.id.demo_confirm);
                    return null;
                });
                await(() -> visibleOffset() != null && visibleOffset() > 0 && status().startsWith("Verified:") && status().contains("native action"));
                step = "local-cancel-and-state-change";
                main(() -> {
                    tap(R.id.demo_review_click);
                    check(reviewing());
                    tap(R.id.demo_sample_mode);
                    check(!reviewing() && "Demo count: 1".equals(count()));
                    reviewLocal(R.id.demo_review_click);
                    tap(R.id.demo_cancel);
                    button(R.id.demo_confirm).performClick();
                    check(!reviewing() && "Demo count: 1".equals(count()));
                    reviewLocal(R.id.demo_review_scroll);
                    samples().scrollBy(0, 7); // Still the same ScrollView; the reviewed state is stale.
                    int moved = samples().getScrollY();
                    tap(R.id.demo_confirm);
                    check(!reviewing() && samples().getScrollY() == moved && !status().startsWith("Verified:"));
                    return null;
                });
                step = "new-overlapping-sibling";
                main(() -> {
                    reviewLocal(R.id.demo_review_click);
                    cover = new View(activity);
                    ((ViewGroup) activity.findViewById(android.R.id.content)).addView(cover, -1, -1);
                    return null;
                });
                await(() -> cover.isLaidOut() && cover.getHeight() > 0);
                main(() -> {
                    tap(R.id.demo_confirm);
                    check(!reviewing() && "Demo count: 1".equals(count()) && !status().startsWith("Verified:"));
                    ((ViewGroup) cover.getParent()).removeView(cover);
                    return null;
                });
                step = "local-click";
                main(() -> {
                    reviewLocal(R.id.demo_review_click);
                    tap(R.id.demo_confirm);
                    check(status().startsWith("Verified:") && status().contains("local hit fallback"));
                    button(R.id.demo_confirm).performClick();
                    check(!reviewing() && "Demo count: 2".equals(count()));
                    return null;
                });
                step = "local-scroll";
                main(() -> {
                    int before = samples().getScrollY();
                    reviewLocal(R.id.demo_review_scroll);
                    tap(R.id.demo_confirm);
                    int after = samples().getScrollY();
                    check(after > before && after - before <= samples().getHeight() && Integer.valueOf(after).equals(visibleOffset()));
                    check(status().startsWith("Verified:") && status().contains("local hit fallback"));
                    button(R.id.demo_confirm).performClick();
                    check(!reviewing() && samples().getScrollY() == after);
                    return null;
                });
                step = "ambiguous-native-result";
                main(() -> {
                    tap(R.id.demo_sample_mode);
                    button(R.id.demo_counter).setAccessibilityDelegate(new View.AccessibilityDelegate() {
                        @Override public boolean performAccessibilityAction(View host, int id, Bundle args) {
                            if (id != AccessibilityNodeInfo.ACTION_CLICK) return super.performAccessibilityAction(host, id, args);
                            nativeCalls++;
                            host.performClick(); // Effect occurred despite a false provider return.
                            return false;
                        }
                    });
                    tap(R.id.demo_review_click);
                    check(reviewing() && button(R.id.demo_confirm).getText().toString().contains("native action"));
                    tap(R.id.demo_confirm);
                    button(R.id.demo_confirm).performClick();
                    check(!reviewing() && !status().startsWith("Verified:") && "Demo count: 3".equals(count()) && nativeCalls == 1);
                    return null;
                });
            }
            result.putString("demo", "PASS-" + mode);
            code = Activity.RESULT_OK;
        } catch (Exception | AssertionError | LinkageError failure) {
            result.putString("demo", "FAIL-" + step);
        } finally {
            if (activity != null) runOnMainSync(activity::finish);
            finish(code, result);
        }
    }
}
