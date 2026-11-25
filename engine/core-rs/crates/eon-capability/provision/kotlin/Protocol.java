package eon.kotlin;

import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.*;
import org.jetbrains.kotlin.js.parser.sourcemaps.*;

/** The private stdio protocol spoken by LocalLsp (one request outstanding). */
final class Protocol {
    static JsonObject obj(Object... fields) {
        var members = new LinkedHashMap<String, JsonNode>();
        for (int i = 0; i < fields.length; i += 2) {
            Object v = fields[i + 1];
            members.put((String) fields[i], v instanceof String s ? new JsonString(s)
                : v instanceof Integer n ? new JsonNumber(n)
                : v instanceof Boolean b ? (b ? JsonBoolean.Companion.getTRUE() : JsonBoolean.Companion.getFALSE())
                : (JsonNode) v);
        }
        return new JsonObject(members);
    }

    static JsonNode field(JsonNode n, String key) {
        return ((JsonObject) n).getProperties().get(key);
    }

    static String string(JsonNode n, String key) {
        return ((JsonString) field(n, key)).getValue();
    }

    static int integer(JsonNode n, String key) {
        return Math.toIntExact((long) ((JsonNumber) field(n, key)).getValue());
    }

    private static final int LIMIT = 512 * 1024;
    static final class RequestError extends RuntimeException {
        RequestError(String reason) { super(reason); }
    }

    static String boundedText(String s, int limit) {
        if (s.length() > limit || s.getBytes(StandardCharsets.UTF_8).length > limit)
            throw new RequestError("Result text exceeds limit");
        return s;
    }

    // Conservative upper bound before serializing: even an escaped UTF-16
    // unit needs at most six UTF-8 bytes. All result shapes are worker-owned.
    private static long bytes(JsonNode v) {
        if (v instanceof JsonString s) return 2 + 6L * s.getValue().length();
        if (v instanceof JsonObject o) {
            long n = 2;
            for (var e : o.getProperties().entrySet()) n += 4 + 6L * e.getKey().length() + bytes(e.getValue());
            return n;
        }
        if (v instanceof JsonArray a) {
            long n = 2;
            for (var e : a.getElements()) n += 1 + bytes(e);
            return n;
        }
        return 32;
    }

    static final class Results {
        private final List<JsonNode> items = new ArrayList<>();
        private long size = 512; // envelope
        void add(JsonNode v) {
            size += bytes(v) + 1;
            if (full() || size > LIMIT) throw new RequestError("Result set exceeds limit");
            items.add(v);
        }
        boolean full() { return items.size() == 256; }
        JsonArray array() { return new JsonArray(items); }
    }

    static JsonNode read(InputStream in) throws IOException {
        var header = new StringBuilder();
        while (header.length() < 64 && !header.toString().endsWith("\r\n\r\n")) {
            int c = in.read();
            if (c < 0 && header.isEmpty()) return null;
            if (c < 0) throw new EOFException();
            header.append((char) c);
        }
        var m = java.util.regex.Pattern.compile("Content-Length: ([0-9]{1,6})\r\n\r\n").matcher(header);
        if (!m.matches()) throw new IOException("invalid frame");
        int len = Integer.parseInt(m.group(1));
        if (len == 0 || len > LIMIT) throw new IOException("frame size");
        var body = in.readNBytes(len);
        if (body.length != len) throw new EOFException();
        return JSONKt.parseJson(new String(body, StandardCharsets.UTF_8));
    }

    static void reply(JsonNode id, String key, JsonNode value) throws IOException {
        var packet = obj("jsonrpc", "2.0", "id", id, key, value);
        if (bytes(packet) > LIMIT) throw new RequestError("Response exceeds limit");
        var out = new StringWriter();
        packet.write(out);
        var body = out.toString().getBytes(StandardCharsets.UTF_8);
        if (body.length > LIMIT) throw new IOException("response size");
        System.out.write(("Content-Length: " + body.length + "\r\n\r\n").getBytes(StandardCharsets.US_ASCII));
        System.out.write(body);
        System.out.flush();
    }
}
