package com.eon.assistant;

import android.app.*;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.media.projection.*;
import android.os.*;
import android.view.WindowManager;
import android.widget.Button;

/** Stock API 35 emulator only. A second real picker grant supersedes EON; neither app has an output. */
public final class ProjectionReplacementFixture extends Activity {
    private static ProjectionReplacementFixture owner;
    private Intent data;
    private long deadline;
    private boolean picking, dispatched;
    private final Handler main = new Handler(Looper.getMainLooper());

    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        if (Build.VERSION.SDK_INT != 35 || !"ranchu".equals(Build.HARDWARE)) { finish(); return; }
        windowProtection();
        Button button = new Button(this);
        button.setText("Open a second Android picker. Hold its grant for three seconds. No output.");
        button.setFilterTouchesWhenObscured(true);
        button.setOnClickListener(v -> {
            if (!hasWindowFocus() || owner != null) return;
            owner = this;
            picking = true;
            button.setEnabled(false);
            startActivityForResult(getSystemService(MediaProjectionManager.class).createScreenCaptureIntent(
                    MediaProjectionConfig.createConfigForDefaultDisplay()), 1);
        });
        setContentView(button);
    }
    private void windowProtection() {
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_SECURE);
        getWindow().setHideOverlayWindows(true);
    }
    @Override protected void onActivityResult(int request, int code, Intent result) {
        super.onActivityResult(request, code, result);
        if (!picking || owner != this || request != 1) return;
        picking = false;
        if (code != RESULT_OK || result == null) { finish(); return; }
        data = result;
        deadline = SystemClock.elapsedRealtime() + 1500;
        main.postDelayed(() -> { if (data != null) { data = null; finish(); } }, 1500);
        dispatch();
    }
    private void dispatch() {
        if (data == null || dispatched || !hasWindowFocus() || SystemClock.elapsedRealtime() >= deadline) return;
        dispatched = true;
        startService(new Intent(this, Holder.class));
    }
    @Override public void onWindowFocusChanged(boolean focus) {
        super.onWindowFocusChanged(focus);
        if (focus) dispatch(); else if (dispatched) data = null;
    }
    @Override protected void onDestroy() {
        if (owner == this) owner = null;
        data = null;
        main.removeCallbacksAndMessages(null);
        super.onDestroy();
    }
    public static final class Holder extends Service {
        private MediaProjection grant;
        private final Handler main = new Handler(Looper.getMainLooper());
        @Override public IBinder onBind(Intent intent) { return null; }
        @Override public int onStartCommand(Intent ignored, int flags, int id) {
            ProjectionReplacementFixture source = owner;
            if (source == null || source.data == null || !source.hasWindowFocus()
                    || SystemClock.elapsedRealtime() >= source.deadline) { stopSelf(); return START_NOT_STICKY; }
            Intent result = source.data;
            source.data = null;
            try {
                getSystemService(NotificationManager.class).createNotificationChannel(new NotificationChannel(
                        "replacement", "No-output grant test", NotificationManager.IMPORTANCE_LOW));
                startForeground(952, new Notification.Builder(this, "replacement")
                        .setSmallIcon(android.R.drawable.ic_dialog_info).setContentTitle("Three-second grant test")
                        .build(), ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION);
                grant = getSystemService(MediaProjectionManager.class).getMediaProjection(Activity.RESULT_OK, result);
                main.postDelayed(this::stopSelf, 3000);
            } catch (RuntimeException failure) { stopSelf(); }
            source.finish();
            return START_NOT_STICKY;
        }
        @Override public void onDestroy() {
            main.removeCallbacksAndMessages(null);
            try { if (grant != null) grant.stop(); } finally { stopForeground(STOP_FOREGROUND_REMOVE); }
            super.onDestroy();
        }
    }
}
