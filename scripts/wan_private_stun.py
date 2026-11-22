"""Private IPv4 STUN Binding publisher for one explicitly provisioned pair."""

import hmac
import ipaddress
import json
import os
import re
import secrets
import signal
import socket
import stat
import struct
import sys
import time

COOKIE = 0x2112A442
HEADER = struct.Struct("!HHI12s")
MI = struct.pack("!HH", 0x001C, 32)
PROOF = struct.pack("!HH", 0xC0E5, 16)
WAIT = struct.pack("!HH", 0xC0E6, 4)
RFC1918 = tuple(map(ipaddress.IPv4Network, ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")))
MAX_COUNT = (1 << 64) - 1


def signed(kind, transaction, attributes, key):
    prefix = HEADER.pack(kind, len(attributes) + 36, COOKIE, transaction) + attributes
    return prefix + MI + hmac.digest(key, prefix, "sha256")


def verified(data, kind, key, expected_id=None):
    if not 56 <= len(data) <= 256:
        raise ValueError("bad STUN length")
    got, length, cookie, tx = HEADER.unpack_from(data)
    if (got, length, cookie) != (kind, len(data) - HEADER.size, COOKIE):
        raise ValueError("bad STUN header")
    if expected_id is not None and tx != expected_id:
        raise ValueError("wrong STUN transaction")
    if data[-36:-32] != MI or not hmac.compare_digest(data[-32:], hmac.digest(key, data[:-36], "sha256")):
        raise ValueError("bad STUN integrity")
    return tx, data[HEADER.size:-36]


def username(user):
    value = user.encode("ascii")
    return struct.pack("!HH", 0x0006, len(value)) + value + bytes(-len(value) % 4)


def mapped(addr):
    ip, port = addr
    value = int.from_bytes(socket.inet_pton(socket.AF_INET, ip), "big") ^ COOKIE
    return struct.pack("!HHBBHI", 0x0020, 8, 0, 1, port ^ (COOKIE >> 16), value)


def decode_mapped(attr):
    kind, length, _, family, port, value = struct.unpack("!HHBBHI", attr)
    if (kind, length, family) != (0x0020, 8, 1):
        raise ValueError("expected IPv4 XOR-MAPPED-ADDRESS")
    return socket.inet_ntop(socket.AF_INET, (value ^ COOKIE).to_bytes(4, "big")), port ^ (COOKIE >> 16)


def paired(pin, addr):
    ip, port = addr
    return struct.pack("!HH32s4sH2x", 0xC0E4, 38, pin, socket.inet_aton(ip), port)


def require(ok):
    if not ok:
        raise ValueError("invalid private publisher configuration")


def load_config(path):
    require(os.path.isabs(path))
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), "rb") as f:
        st = os.fstat(f.fileno())
        require(stat.S_ISREG(st.st_mode) and st.st_uid == os.geteuid()
                and st.st_nlink == 1 and stat.S_IMODE(st.st_mode) in (0o400, 0o600))
        data = f.read(4097)
    require(len(data) <= 4096)

    def unique_fields(items):
        obj = dict(items)
        require(len(obj) == len(items))
        return obj

    return Publisher(json.loads(data, object_pairs_hook=unique_fields))


class Member:
    def __init__(self, spec):
        require(isinstance(spec, dict) and set(spec) == {"user", "pin", "password"})
        require(re.fullmatch(r"[a-zA-Z0-9_-]{1,64}", spec["user"]))
        require(re.fullmatch("[0-9a-f]{64}", spec["pin"]) and int(spec["pin"], 16) != 0)
        require(re.fullmatch("[0-9a-f]{64}", spec["password"]))
        self.user, self.pin = username(spec["user"]), bytes.fromhex(spec["pin"])
        self.key = spec["password"].encode("ascii")
        self.pending = self.live = self.waiter = None
        self.credit, self.last = 8, 0
        self.observed = self.published = 0

    def expire(self, now):
        if self.pending and now >= self.pending[0] + 3:
            self.pending = None
        if self.live and now >= self.live[0]:
            self.live = None
        if self.waiter and now >= self.waiter[0]:
            self.waiter = None

    def take_reply(self, peer):
        # A single already-paid response to a return-verified source; consume
        # before send, even on UDP loss. Only a fresh proof can arm another.
        _, tx = self.waiter
        self.waiter = None
        src = self.live[1]
        self.published = min(MAX_COUNT, self.published + 1)
        return src, signed(0x0101, tx, mapped(src) + paired(peer.pin, peer.live[1]), self.key)

    def allow(self, now):
        self.credit = min(8, self.credit + 4 * (now - self.last))
        self.last = now
        if self.credit < 1:
            return False
        self.credit -= 1
        return True


class Publisher:
    def __init__(self, cfg):
        require(isinstance(cfg, dict) and set(cfg) == {"listen", "network", "ports", "members"})
        require(all(isinstance(cfg[k], list) and len(cfg[k]) == 2 for k in ("listen", "ports", "members")))
        host, port = cfg["listen"]
        require(isinstance(host, str) and type(port) is int and 1024 <= port <= 65535)
        ip = ipaddress.IPv4Address(host)
        self.net = ipaddress.IPv4Network(cfg["network"], strict=True)
        require(str(self.net) == cfg["network"] and self.net.prefixlen >= 24
                and any(self.net.subnet_of(n) for n in RFC1918))
        require(any(ip in n for n in RFC1918) and not self.reserved(ip))
        self.listen = (str(ip), port)
        self.lo, self.hi = cfg["ports"]
        require(type(self.lo) is type(self.hi) is int and 1024 <= self.lo <= self.hi <= 65535
                and self.hi - self.lo < 256)
        a, b = self.members = tuple(Member(m) for m in cfg["members"])
        require(a.user != b.user and a.pin != b.pin and a.key != b.key)

    def reserved(self, ip):
        return self.net.prefixlen < 31 and ip in (self.net.network_address, self.net.broadcast_address)

    def replies(self, data, source, now):
        for member in self.members:
            member.expire(now)
        ip = ipaddress.IPv4Address(source[0])
        if (ip not in self.net or source[0] == self.listen[0] or not self.lo <= source[1] <= self.hi
                or self.reserved(ip) or not 112 <= len(data) <= 256):
            return ()
        for i, member in enumerate(self.members):
            if not data[20:].startswith(member.user):
                continue
            try:
                tx, attrs = verified(data, 1, member.key)
            except ValueError:
                return ()
            rest = attrs[len(member.user):]
            proof = None
            wait_ms = 1000  # one bounded receive window for the original proof format
            if rest.startswith(PROOF) and len(rest) >= 20:
                proof, rest = rest[4:20], rest[20:]
                if rest.startswith(WAIT) and len(rest) >= 8:
                    wait_ms, = struct.unpack_from("!I", rest, 4)
                    if not 0 < wait_ms <= 1000:
                        return ()
                    rest = rest[8:]
            # Only zero PADDING is allowed after USERNAME and optional proof.
            if (len(rest) < 4 or rest[:4] != struct.pack("!HH", 0x0026, len(rest)-4)
                    or any(rest[4:]) or len(rest) % 4 or not member.allow(now)):
                return ()
            peer = self.members[1 - i]
            if proof is None:
                if not member.pending or member.pending[1:3] != (source, tx):
                    member.pending = (now, source, tx, secrets.token_bytes(16))
                attrs = mapped(source) + PROOF + member.pending[3]
                return ((source, signed(0x0101, tx, attrs, member.key)),)
            pending = member.pending
            if not pending or pending[1:3] != (source, tx) or not hmac.compare_digest(pending[3], proof):
                return ()
            member.pending = None
            # Anchor the grant conservatively to challenge issuance, not echo
            # arrival: a delayed echo cannot open another full receive window.
            until = pending[0] + wait_ms / 1000
            if now >= until:
                return ()
            member.live = (now + 5, source)
            member.waiter = (until, tx)
            member.observed = min(MAX_COUNT, member.observed + 1)
            if not peer.live or peer.live[1] == source:
                return ()
            return tuple(m.take_reply(p) for m, p in ((member, peer), (peer, member)) if m.waiter)
        return ()


def serve(service):
    running = True

    def stop(*_):
        nonlocal running
        running = False

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(service.listen)
        sock.settimeout(0.5)
        print("private pair publisher ready", flush=True)
        while running:
            try:
                data, source = sock.recvfrom(257)
            except TimeoutError:
                for member in service.members:
                    member.expire(time.monotonic())
                continue
            for dest, response in service.replies(data, source, time.monotonic()):
                try:
                    sock.sendto(response, dest)
                except OSError:
                    pass  # clients can start a fresh authenticated transaction
    for i, m in enumerate(service.members):
        print(f"member={i} confirmed={m.observed} paired_replies={m.published}")


if __name__ == "__main__":
    if len(sys.argv) != 3 or sys.argv[1] != "serve":
        raise SystemExit("usage: wan_private_stun.py serve /absolute/private-config.json")
    try:
        serve(load_config(sys.argv[2]))
    except (OSError, ValueError, TypeError, RecursionError):
        raise SystemExit("private pair publisher could not start or continue") from None
