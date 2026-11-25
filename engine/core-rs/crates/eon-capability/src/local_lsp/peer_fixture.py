# Trusted test-only stdio fixture. No project imports, subprocesses or discovery.
import json
import os
import sys
import time


def send(packet):
    b = json.dumps(dict(jsonrpc="2.0", **packet)).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(b)}\r\n\r\n".encode() + b)
    sys.stdout.buffer.flush()


def recv():
    length = sys.stdin.buffer.readline()
    assert length.startswith(b"Content-Length: ")
    assert sys.stdin.buffer.readline() == b"\r\n"
    return json.loads(sys.stdin.buffer.read(int(length.split(b":")[1])))


mode, sentinel = sys.argv[1:]
assert os.getcwd() == "/" and "HOME" not in os.environ and "PATH" not in os.environ
try:
    open(sentinel, "rb").close()
except PermissionError:
    pass
else:
    raise AssertionError("workspace was mounted")
init = recv()
assert init["method"] == "initialize"
assert init["params"]["rootUri"] is None and init["params"]["workspaceFolders"] is None
caps = {
    "hoverProvider": True, "referencesProvider": True, "definitionProvider": True,
    "textDocumentSync": 1 if mode == "full" else {"openClose": True, "change": 2},
    "workspaceSymbolProvider": {},
}
if mode in ("pull", "pull-with-pushes"):
    caps["diagnosticProvider"] = {"identifier": "lint"}
if mode == "immutable":
    caps = {"textDocumentSync": {"openClose": True}}
if mode == "no-sync":
    caps = {}
if mode == "bad-capabilities":
    caps = []
if mode == "exit-before-ready":
    sys.exit(2)
if mode == "stall-before-ready":
    time.sleep(60)
send({"id": init["id"], "result": {"capabilities": caps}})
assert recv()["method"] == "initialized"
if mode == "blocked-stdin":
    time.sleep(60)
if mode == "fail-during-open":
    def created_marker():
        event = recv()
        assert event["method"] == "workspace/didChangeWatchedFiles"
        change, = event["params"]["changes"]
        assert change["type"] == 1
        return change["uri"]

    first = created_marker()
    opened = recv()
    assert opened["method"] == "textDocument/didOpen"
    assert opened["params"]["textDocument"]["uri"] == first
    second = created_marker()
    assert int(sys.stdin.buffer.readline().split(b":")[1]) > 300_000
    assert sys.stdin.buffer.readline() == b"\r\n"
    assert b'"method":"textDocument/didOpen"' in sys.stdin.buffer.read(100)
    # Confirm the marker notification and the partial didOpen reached the real
    # pipe before disconnecting. No workspace or marker-directory access.
    send({"method": "test/partialDidOpen", "params": {"uri": second}})
    sys.exit(0)

position = {"line": 0, "character": 0}
span = {"start": position, "end": position}
items = [{"message": "fixture message", "range": span}]
docs = []
version = 0


def publish(uri, diagnostics, v=None):
    p = {"uri": uri, "diagnostics": diagnostics}
    if v is not None:
        p["version"] = v
    send({"method": "textDocument/publishDiagnostics", "params": p})


while True:
    m = recv()
    method = m["method"]
    if method == "textDocument/didOpen":
        doc = m["params"]["textDocument"]
        assert doc["uri"].startswith("file:///eon-lsp-") and doc["version"] > version
        version = doc["version"]
        assert doc["languageId"] == "python"
        docs.append(doc["uri"])
        if mode == "push":
            publish(docs[-1], items)  # allowed for a first open
        if mode in ("full", "incremental") or (mode == "stale" and version == 1):
            publish(docs[-1], items, version)
    elif method == "textDocument/didChange":
        doc = m["params"]["textDocument"]
        uri = doc["uri"]
        assert uri in docs and doc["version"] > version
        delta, = m["params"]["contentChanges"]
        assert delta["text"] == "updated"
        if mode == "full":
            assert "range" not in delta
        else:
            assert delta["range"] == {"start": position, "end": {"line": 2, "character": 3}}
        publish(uri, items)  # ambiguous unversioned report
        publish(uri, items, version)  # prior snapshot
        version = doc["version"]
        if mode != "stale":
            publish(uri, [], version)
    elif method == "textDocument/didClose":
        uri = m["params"]["textDocument"]["uri"]
        docs.remove(uri)
        if mode in ("full", "incremental", "stale"):
            publish(uri, [])  # may be consumed after reopening this URI
            publish(uri, [], version)
    elif method == "textDocument/definition":
        uri = "file:///outside.py" if mode == "outside" else docs[-1]
        if mode == "link":
            send({"id": m["id"], "result": [{"targetUri": uri, "targetRange": span,
                                               "targetSelectionRange": span}]})
        else:
            send({"id": m["id"], "result": {"uri": uri, "range": span}})
    elif method == "workspace/symbol":
        uri = "file:///outside.py" if mode == "outside" else docs[-1]
        send({"id": m["id"], "result": [{"name": "f", "kind": 12,
                                         "location": {"uri": uri, "range": span}}]})
    elif method == "textDocument/references":
        assert m["params"]["context"]["includeDeclaration"]
        send({"id": m["id"], "result": [{"uri": u, "range": span} for u in docs]})
    elif method == "textDocument/diagnostic":
        assert m["params"]["identifier"] == "lint"
        report = items * 257 if mode == "pull-with-pushes" else items
        send({"id": m["id"], "result": {"kind": "full", "items": report}})
    elif method == "textDocument/hover":
        if mode == "pull-with-pushes":
            # Both pushes are within the wire budget. The second exceeds the
            # item budget for a requested report, not for the pending hover.
            publish(docs[-1], items, version)
            publish(docs[-1], items * 257, version)
        if mode == "timeout":
            sys.stdout.buffer.write(b"Content-Length: 15\r\n\r\n{}")
            sys.stdout.buffer.flush()
            time.sleep(60)
        if mode == "flood":
            for n in range(50):
                send({"method": "window/logMessage", "params": {"message": "status"}})
        if mode == "mismatch":
            send({"id": m["id"] + 1, "result": None})
        # The client must refuse even a server with its own bidirectional request ID.
        send({"id": "test/edit", "method": "workspace/applyEdit", "params": {"edit": {}}})
        refusal = recv()
        assert refusal["id"] == "test/edit" and refusal["error"]["code"] == -32601
        contents = ["first", {"language": "python", "value": "int"}] if mode == "link" else {"kind": "plaintext", "value": "int"}
        send({"id": m["id"], "result": {"contents": contents}})
    elif method == "shutdown":
        assert "params" not in m
        send({"id": m["id"], "result": None})
    elif method == "exit":
        assert "params" not in m
        if mode == "ignore-exit":
            time.sleep(60)
        break
    else:
        raise AssertionError("unexpected client operation")
