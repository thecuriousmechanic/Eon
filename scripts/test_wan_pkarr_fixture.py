"""Ephemeral HTTPS store for --isolated-n0. Not a public pkarr service."""

import base64
from http.server import BaseHTTPRequestHandler
import os
from pathlib import Path
from socketserver import TCPServer
import ssl
import sys


def run(parent, expected):
    if os.stat("/proc/self/ns/net").st_ino != expected or parent == expected or os.geteuid() == 0:
        raise RuntimeError("requires the unprivileged, isolated relay role")

    peers = {"10.77.0.2": "a", "10.77.0.3": "b"}
    paths, records, substituted, stale, reads = {}, {}, set(), set(), set()
    dialer = min(peers.values(), key=lambda s: Path(f"keys/{s}.pub").read_text().strip())
    non_relay_publication = False
    for side in peers.values():
        key = bytes.fromhex(Path(f"keys/{side}.pub").read_text().strip())
        z32 = base64.b32encode(key).decode().rstrip("=").translate(
            str.maketrans("ABCDEFGHIJKLMNOPQRSTUVWXYZ234567", "ybndrfg8ejkmcpqxot1uwisza345h769"))
        paths["/pkarr/" + z32] = side

    class Store(BaseHTTPRequestHandler):
        timeout = 2

        def log_message(self, *_):
            pass  # Do not retain keys, requests or records in logs.

        def do_PUT(self):
            nonlocal non_relay_publication
            side = paths.get(self.path)
            length = int(self.headers.get("Content-Length", "0"))
            if side is None or peers.get(self.client_address[0]) != side or not 72 <= length <= 1100:
                self.send_error(403)
                return
            record = self.rfile.read(length)
            if len(record) != length:
                self.send_error(400)
                return
            # The DNS payload follows the signature and timestamp. No IP or
            # application TXT data should reach the provider, even temporarily.
            if b"addr=" in record[72:] or b"user-data=" in record[72:]:
                non_relay_publication = True
                self.send_error(400)
                return
            if b"relay=https://use1-1.relay.n0.iroh.link." in record[72:]:
                records[side] = record
            self.send_response(200)
            self.end_headers()

        def do_GET(self):
            side, caller = paths.get(self.path), peers.get(self.client_address[0])
            if caller != dialer or side is None or caller == side or len(records) != 2:
                self.send_error(404)
                return
            # First return a valid record under the WRONG signing identity.
            # Next serve a correctly signed, stale default relay. Only a new
            # lookup after an unsuccessful dial can obtain the current record.
            if caller not in substituted:
                payload = records[caller]
                substituted.add(caller)
            elif caller not in stale:
                payload = Path(f"keys/stale-{side}.pkt").read_bytes()
                stale.add(caller)
            else:
                payload = records[side]
                reads.add(caller)
            self.send_response(200)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    context.load_cert_chain("keys/leaf.pem", "keys/leaf.key")
    # HTTPServer also calls getfqdn() at bind time; no system DNS in this role.
    with TCPServer(("10.77.0.4", 8445), Store) as server:
        server.socket = context.wrap_socket(server.socket, server_side=True)
        server.timeout = 0.2
        Path("pkarr-ready").touch()
        while not Path("finished").exists() and not Path("abort").exists():
            server.handle_request()
    if not Path("abort").exists():
        assert not non_relay_publication, "non-relay record published"
        assert set(records) == {"a", "b"} and reads == substituted == stale == {dialer}
        print("private HTTPS pkarr: elected dialer recovered after wrong signer and signed stale relay")


if __name__ == "__main__":
    run(*map(int, sys.argv[1:]))
