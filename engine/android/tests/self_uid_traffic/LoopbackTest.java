package org.eon.test.uidtraffic;

import android.app.Activity;
import android.app.Application;
import android.app.Instrumentation;
import android.content.Context;
import android.content.pm.PackageManager;
import android.os.Bundle;
import android.os.Process;
import android.os.SystemClock;

import com.eon.assistant.SelfUidTraffic;

import java.io.IOException;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.util.Arrays;
import java.util.List;
import java.util.Optional;

/** Self-targeted disposable fixture. No test instrumentation or traffic generator in :app. */
public final class LoopbackTest extends Instrumentation {
    private static final String PACKAGE = "org.eon.test.uidtraffic";
    private static final int COUNT = 32, SIZE = 512;

    @Override public void onCreate(Bundle arguments) {
        super.onCreate(arguments);
        start();
    }

    private static void require(boolean ok, String message) {
        if (!ok) throw new AssertionError(message);
    }

    private void checkIdentity() throws Exception {
        Context app = getTargetContext();
        require(PACKAGE.equals(app.getPackageName()) && PACKAGE.equals(Application.getProcessName())
                && Process.isApplicationUid(Process.myUid()) && Process.myUid() == app.getApplicationInfo().uid,
                "not the fixture app UID");
        require(Arrays.equals(new String[]{android.Manifest.permission.INTERNET},
                app.getPackageManager().getPackageInfo(PACKAGE, PackageManager.GET_PERMISSIONS).requestedPermissions),
                "unexpected fixture permissions");
        String domain = new String(Files.readAllBytes(Paths.get("/proc/thread-self/attr/current")),
                StandardCharsets.US_ASCII);
        List<String> status = Files.readAllLines(Paths.get("/proc/thread-self/status"));
        require(domain.startsWith("u:r:untrusted_app:s0")
                && status.contains("CapEff:\t0000000000000000") && status.contains("Seccomp:\t2"),
                "not the unprivileged app domain");
    }

    private static void exchange(DatagramSocket tx, DatagramSocket rx, int count) {
        byte[] data = new byte[SIZE];
        DatagramPacket out = new DatagramPacket(data, data.length, tx.getRemoteSocketAddress());
        DatagramPacket in = new DatagramPacket(new byte[SIZE], SIZE);
        try {
            for (int i = 0; i < count; ++i) {
                data[0] = (byte) i;
                tx.send(out);
                rx.receive(in);
                require(in.getLength() == SIZE && Arrays.equals(data, in.getData()), "loopback receive failed");
            }
        } catch (IOException failed) {
            throw new AssertionError("loopback exchange failed", failed);
        }
    }

    @Override public void onStart() {
        Bundle result = new Bundle();
        try {
            checkIdentity();
            InetAddress lo = InetAddress.getByAddress(new byte[]{127, 0, 0, 1});
            try (DatagramSocket tx = new DatagramSocket(0, lo); DatagramSocket rx = new DatagramSocket(0, lo)) {
                tx.connect(lo, rx.getLocalPort());
                rx.connect(lo, tx.getLocalPort());
                rx.setSoTimeout(500);
                int[] calls = {0};
                require(!SelfUidTraffic.measure(false, () -> {
                    calls[0]++;
                    exchange(tx, rx, 1); // Establish a real bidirectional baseline.
                }).isPresent() && calls[0] == 1, "opt-out");
                SystemClock.sleep(1250); // Only in this fixture; accommodate the platform's 1 s cache.
                Optional<SelfUidTraffic.Delta> measured = SelfUidTraffic.measure(true, () -> {
                    calls[0]++;
                    exchange(tx, rx, COUNT);
                    SystemClock.sleep(1250);
                });
                require(calls[0] == 2 && measured.isPresent(), "missing positive measurement");
                SelfUidTraffic.Delta delta = measured.get();
                // IPv4 + UDP overhead, no fragmentation. Reject incidental unrelated increments.
                long expected = COUNT * (SIZE + 20 + 8);
                require(delta.rxBytes == expected && delta.txBytes == expected, "loopback byte totals differ");
                result.putString("uid_traffic", "PASS untrusted_app; CapEff=0; seccomp=2; rx="
                        + delta.rxBytes + "; tx=" + delta.txBytes);
            }
            finish(Activity.RESULT_OK, result);
        } catch (Exception | AssertionError failed) {
            result.putString("uid_traffic_error", failed.toString());
            finish(Activity.RESULT_CANCELED, result);
        }
    }
}
