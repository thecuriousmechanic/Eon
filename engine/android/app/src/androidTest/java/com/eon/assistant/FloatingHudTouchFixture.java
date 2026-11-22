package com.eon.assistant;

import android.app.Activity;
import android.os.Bundle;
import android.view.Gravity;
import android.view.MotionEvent;
import android.widget.Button;
import android.widget.FrameLayout;

/** Separate UID, public test-only screen with no real content. Counts physical taps, not coordinates. */
public final class FloatingHudTouchFixture extends Activity {
    private int taps;
    private boolean obscured;

    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        FrameLayout screen = new FrameLayout(this);
        Button pad = new Button(this);
        pad.setText("Fixture taps: 0");
        pad.setOnTouchListener((v, event) -> {
            if (event.getActionMasked() == MotionEvent.ACTION_DOWN)
                obscured = (event.getFlags() & MotionEvent.FLAG_WINDOW_IS_OBSCURED) != 0;
            return false;
        });
        pad.setOnClickListener(v -> pad.setText("Fixture taps: " + ++taps + (obscured ? " (obscured)" : " (clear)")));
        screen.addView(pad, new FrameLayout.LayoutParams(-1, -1));
        Button protect = new Button(this);
        protect.setText("Protect fixture");
        protect.setOnClickListener(v -> {
            getWindow().setHideOverlayWindows(true);
            pad.setFilterTouchesWhenObscured(true);
            protect.setEnabled(false);
            protect.setText("Protected fixture");
        });
        screen.addView(protect, new FrameLayout.LayoutParams(-2, -2, Gravity.CENTER_VERTICAL | Gravity.START));
        setContentView(screen);
    }
}
