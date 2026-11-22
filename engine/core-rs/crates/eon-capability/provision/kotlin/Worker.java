package eon.kotlin;

import static eon.kotlin.Protocol.*;
import java.io.BufferedInputStream;
import java.io.FileDescriptor;
import java.io.FileInputStream;
import org.jetbrains.kotlin.js.parser.sourcemaps.*;

public final class Worker {
    /** JNI entry point. No argv, project root, classpath, config or command RPC. */
    public static void run() throws Exception {
        // A blocking pipe has no seek position. Avoid available()'s FIONREAD
        // ioctl entirely rather than adding an ioctl permission for HotSpot.
        var input = new BufferedInputStream(new FileInputStream(FileDescriptor.in) {
            @Override public int available() { return 0; }
        });
        var module = new Snapshots();
        try {
            for (JsonNode msg; (msg = read(input)) != null;) {
                var id = field(msg, "id");
                var p = field(msg, "params");
                var doc = p instanceof JsonObject ? field(p, "textDocument") : null;
                String method = string(msg, "method");
                try { switch (method) {
                case "initialize" -> reply(id, "result", obj("capabilities", obj("textDocumentSync", 1,
                    "positionEncoding", "utf-16", "definitionProvider", true, "referencesProvider", true,
                    "hoverProvider", true, "workspaceSymbolProvider", true, "diagnosticProvider", obj("interFileDependencies", true,
                    "workspaceDiagnostics", false))));
                case "initialized" -> {}
                case "textDocument/didOpen" -> module.put(string(doc, "uri"), string(doc, "text"), true);
                case "textDocument/didChange" -> module.put(string(doc, "uri"),
                    string(((JsonArray) field(p, "contentChanges")).getElements().get(0), "text"), false);
                case "textDocument/didClose" -> module.close(string(doc, "uri"));
                case "workspace/symbol" -> reply(id, "result", module.symbols(string(p, "query")));
                case "textDocument/definition", "textDocument/references", "textDocument/hover",
                     "textDocument/diagnostic" -> reply(id, "result", module.query(method, string(doc, "uri"), p));
                case "shutdown" -> reply(id, "result", JsonNull.INSTANCE);
                case "exit" -> { return; }
                default -> {
                    if (id != null) reply(id, "error", obj("code", -32601, "message", "Unsupported"));
                }
                } } catch (RequestError e) {
                    if (id == null) throw e;
                    reply(id, "error", obj("code", -32803, "message", e.getMessage()));
                }
            }
        } finally {
            module.dispose();
        }
    }
}
