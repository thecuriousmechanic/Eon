package com.eon.assistant;

import android.app.Application;
import android.os.Build;

/** Load through ART so the packaged Rust broker can register its host VM before native work. */
public final class FoundationVaultApplication extends Application {
    // Providers and instrumentation can start before Application.onCreate().
    static {
        System.loadLibrary("eon_uniffi");
    }

    @Override public void onCreate() {
        super.onCreate();
        if (Build.VERSION.SDK_INT >= 31) registerActivityLifecycleCallbacks(FloatingHudActivities.INSTANCE);
    }
}
