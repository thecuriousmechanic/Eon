import com.sun.jdi.Bootstrap;
import com.sun.jdi.Location;
import com.sun.jdi.ReferenceType;
import com.sun.jdi.VirtualMachine;
import com.sun.jdi.connect.spi.Connection;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.io.*;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.TimeUnit;

final class Inspect {
    // Both endpoints are private streams created by ProcessBuilder. JDI connectors
    // (including the socket launch/listen/attach connectors) are never selected.
    private static final class Pipes extends Connection {
        private final DataInputStream in;
        private final OutputStream out;
        private volatile boolean open = true;

        Pipes(Process child) throws IOException {
            in = new DataInputStream(new BufferedInputStream(child.getInputStream()));
            out = child.getOutputStream();
            byte[] handshake = "JDWP-Handshake".getBytes(StandardCharsets.US_ASCII);
            out.write(handshake);
            out.flush();
            if (!Arrays.equals(in.readNBytes(handshake.length), handshake))
                throw new IOException("fixture handshake failed");
        }

        @Override public byte[] readPacket() throws IOException {
            int first = in.read();
            if (first < 0) return new byte[0]; // JDI's EOF sentinel
            int size = (first << 24) | (in.readUnsignedByte() << 16)
                       | (in.readUnsignedByte() << 8) | in.readUnsignedByte();
            if (size < 11 || size > 256 * 1024) throw new IOException("invalid packet size");
            byte[] packet = new byte[size];
            ByteBuffer.wrap(packet).putInt(size);
            in.readFully(packet, 4, size - 4);
            return packet;
        }

        @Override public synchronized void writePacket(byte[] packet) throws IOException {
            if (packet.length < 11) throw new IOException("short packet");
            int size = ByteBuffer.wrap(packet).getInt();
            if (size < 11 || size > 256 * 1024 || size > packet.length)
                throw new IOException("invalid packet size");
            out.write(packet, 0, size);
            out.flush();
        }

        @Override public void close() throws IOException {
            open = false;
            try { out.close(); } finally { in.close(); }
        }

        @Override public boolean isOpen() { return open; }
    }

    private static void require(boolean condition, String reason) {
        if (!condition) throw new AssertionError(reason);
    }

    private record Observation(String stack, long owners, long markers) {}

    private static Observation inspect(boolean interruptAtStop) throws Exception {
        Path classes = Path.of(Inspect.class.getProtectionDomain().getCodeSource().getLocation()
                                      .toURI()).toRealPath();
        String dir = classes.toString();
        var launcher = new ProcessBuilder(
            "/usr/bin/setpriv", "--pdeathsig=KILL", "--", classes.resolve("jdk/bin/java").toString(),
            "-XX:+DisableAttachMechanism", "-XX:-UsePerfData", "-XX:-CreateCoredumpOnCrash",
            "-XX:ActiveProcessorCount=1", "-Xshare:off", "-Xint", "-Xmx48m", "-Djava.io.tmpdir=" + dir,
            "-agentlib:jdwp=transport=ownedpipe,server=n,suspend=y,address=owned-fixture",
            "-cp", dir, "OwnedFixture");
        launcher.environment().clear();
        launcher.environment().put("LD_LIBRARY_PATH", dir);
        launcher.directory(classes.toFile());
        launcher.redirectError(ProcessBuilder.Redirect.DISCARD);
        Process child = launcher.start();
        Observation observed = null;
        try {
            VirtualMachine vm = Bootstrap.virtualMachineManager()
                .createVirtualMachine(new Pipes(child), child);
            require(vm.canGetInstanceInfo(), "JDK cannot count instances");
            var requests = vm.eventRequestManager();
            var prepare = requests.createClassPrepareRequest();
            prepare.addClassFilter("OwnedFixture");
            prepare.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            prepare.enable();
            BreakpointRequest stop = null;
            for (int batch = 0; batch < 16; ++batch) {
                EventSet events = vm.eventQueue().remove(5000);
                require(events != null, "timed out waiting for fixture event");
                for (Event event : events) {
                    if (event instanceof ClassPrepareEvent e && e.request() == prepare) {
                        require(stop == null, "duplicate fixture preparation");
                        var mark = e.referenceType().methodsByName("mark", "()V").get(0);
                        stop = requests.createBreakpointRequest(mark.location());
                        stop.addCountFilter(2); // VM-side condition; do not evaluate an expression
                        stop.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                        stop.enable();
                    } else if (event instanceof BreakpointEvent e) {
                        require(observed == null && stop != null && e.request() == stop,
                                "unexpected stop");
                        require(e.thread().frameCount() == 3, "unexpected stack depth");
                        var frames = e.thread().frames(0, 3);
                        String[] methods = {"mark", "selected", "main"};
                        int[] lines = {7, 18, 23};
                        var stack = new ArrayList<String>(3);
                        for (int i = 0; i < frames.size(); ++i) {
                            Location loc = frames.get(i).location();
                            require(loc.declaringType().name().equals("OwnedFixture") &&
                                    loc.method().name().equals(methods[i]) &&
                                    loc.lineNumber() == lines[i], "unexpected fixture frame");
                            stack.add(loc.method().name() + ":" + loc.lineNumber());
                        }
                        List<ReferenceType> markers = vm.classesByName("OwnedFixture$Marker");
                        require(markers.size() == 1, "expected one marker class");
                        // Only aggregate counts of our two known classes. No instances or values.
                        long[] counts = vm.instanceCounts(
                            List.of(e.location().declaringType(), markers.get(0)));
                        require(counts.length == 2 && counts[0] == 0 && counts[1] == 3,
                                "unexpected per-class counts");
                        observed = new Observation(String.join(" ", stack), counts[0], counts[1]);
                        if (interruptAtStop) return observed; // do not resume before cleanup
                    } else if (event instanceof VMDeathEvent) {
                        require(observed != null && !interruptAtStop, "fixture missed the stop");
                        events.resume();
                        require(child.waitFor(5, TimeUnit.SECONDS) && child.exitValue() == 0,
                                "fixture did not exit cleanly");
                        return observed;
                    }
                }
                events.resume();
            }
            throw new AssertionError("event budget exhausted");
        } finally {
            if (child.isAlive()) child.destroyForcibly();
            require(child.waitFor(5, TimeUnit.SECONDS), "fixture not reaped");
            if (interruptAtStop && observed != null)
                require(child.exitValue() != 0, "suspended fixture unexpectedly finished");
            child.getOutputStream().close();
            child.getInputStream().close();
        }
    }

    static void report(boolean interruptAtStop) throws Exception {
        Observation r = inspect(interruptAtStop);
        System.out.println(r.stack() + " " + r.owners() + " " + r.markers());
    }

    public static void main(String[] args) throws Exception {
        require(args.length == 0, "no arguments accepted");
        report(false);
    }
}
