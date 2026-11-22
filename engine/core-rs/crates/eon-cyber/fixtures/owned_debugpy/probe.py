"""PID-namespace init for the fixed counting fixture. No command-line interface."""
import hashlib
import io
import json
import os
from pathlib import Path
import re
import resource
import socket
import stat
import subprocess
import sys
import zipfile


SOURCE = """\
# The visits share one instruction but have distinct callers.
def count(visit, marker_allocations, marker_live):
    return visit + 1

def first():
    return count(1, *record_marker(True))

def second():
    return count(2, *record_marker(False))

def third():
    return count(3, *record_marker(True))

def main():
    a = first()
    b = second()
    c = third()
    assert (a, b, c) == (2, 3, 4)

import weakref

class Marker:
    pass

created = 0
registry = weakref.WeakSet()
retained = []

def record_marker(keep):
    global created
    marker = Marker()
    created += 1
    registry.add(marker)
    if keep:
        retained.append(marker)
    del marker
    return created, len(registry)

main()
"""
WHEEL = "debugpy-1.8.22-py2.py3-none-any.whl"
SHA256 = "a9e9d3550e15ca479c59333e90845029190531f0cacfedab3b815a57bd913947"
SIZE = 5374857
PYTHON = "/usr/bin/python3"
LAUNCHER = """\
import sys
if not (sys.executable == "/usr/bin/python3" and sys.flags.isolated and
        sys.flags.no_site and sys.flags.dont_write_bytecode and "site" not in sys.modules):
    sys.exit(1)
import runpy
runpy.run_path("/proc/self/cwd/debugpy/launcher", run_name="__main__")
"""


class FixtureError(Exception):
    pass


class ProtocolError(Exception):
    pass


def prepare(work):
    status = dict(line.split(":", 1) for line in Path("/proc/self/status").read_text().splitlines())
    if not (
        os.getpid() == 1
        and os.stat("/proc/self/ns/net").st_ino != int(os.environ["OWNED_PARENT_NET"])
        and socket.if_nameindex() == [(1, "lo")]
        and int(status["NoNewPrivs"]) == 1
        and all(int(status[key], 16) == 0 for key in ("CapEff", "CapPrm", "CapBnd"))
        and os.path.ismount(work)
        and not any(work.iterdir())
    ):
        raise OSError("isolation unavailable")
    os.umask(0o077)
    for kind, limit in ((resource.RLIMIT_CORE, 0), (resource.RLIMIT_CPU, 10),
                        (resource.RLIMIT_NOFILE, 96), (resource.RLIMIT_AS, 768 << 20)):
        resource.setrlimit(kind, (limit, limit))
    wheel = work.parents[3] / "target" / WHEEL
    fd = os.open(wheel, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as f:
        info = os.fstat(f.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size != SIZE:
            raise OSError("wheel unavailable")
        data = f.read(SIZE + 1)
    if len(data) != SIZE or hashlib.sha256(data).hexdigest() != SHA256:
        raise OSError("wrong wheel")
    # Verify the exact bytes in memory before extracting into the private tmpfs.
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        archive.extractall(work, [n for n in archive.namelist() if n.startswith("debugpy/")])
    (work / "launcher.py").write_text(LAUNCHER, encoding="utf-8")
    target = work / "count.py"
    target.write_text(SOURCE, encoding="utf-8")
    return str(target)


class Session:
    def __init__(self, process):
        self.rx = process.stdout
        self.tx = process.stdin
        self.messages = 48
        self.seq = 0
        self.launch_seq = None
        self.launched = self.initialized = self.resumed = self.exited = self.terminated = False
        self.thread = None

    def send(self, command, arguments):
        self.seq += 1
        body = json.dumps(dict(type="request", seq=self.seq, command=command,
                               arguments=arguments)).encode()
        self.tx.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
        self.tx.flush()
        return self.seq

    def receive(self):
        self.messages -= 1
        if self.messages < 0:
            raise ProtocolError
        match = re.fullmatch(rb"Content-Length: ([1-9][0-9]{0,4})\r\n", self.rx.readline(64))
        if not match or self.rx.readline(4) != b"\r\n":
            raise ProtocolError
        size = int(match[1])
        if size > 65536:
            raise ProtocolError
        data = self.rx.read(size)
        if len(data) != size:
            raise ProtocolError
        return json.loads(data)

    def event(self, name, body):
        if name == "initialized" and not self.initialized:
            self.initialized = True
        elif name == "stopped" and self.thread is None and body["reason"] == "breakpoint":
            thread = body["threadId"]
            if type(thread) is not int or not 1 <= thread <= 2**31 - 1:
                raise ProtocolError
            self.thread = thread
        elif name == "exited" and self.resumed and not self.exited and body["exitCode"] == 0:
            self.exited = True
        elif name == "terminated" and self.exited and not self.terminated:
            self.terminated = True
        elif name not in ("output", "debugpySockets", "process", "thread", "continued", "module"):
            raise FixtureError

    def dispatch(self, seq=None, command=None):
        msg = self.receive()
        body = msg.get("body", {})
        if msg["type"] == "event":
            self.event(msg["event"], body)
        elif msg["type"] == "response" and msg["success"] is True:
            if (self.launch_seq is not None and msg["request_seq"] == self.launch_seq
                    and msg["command"] == "launch"):
                self.launch_seq = None
                self.launched = True
            elif seq is not None and (msg["request_seq"], msg["command"]) == (seq, command):
                return body
            else:
                raise ProtocolError
        else:
            # No reverse requests, including runInTerminal and startDebugging.
            raise ProtocolError
        return None

    def call(self, command, args):
        seq = self.send(command, args)
        while (body := self.dispatch(seq, command)) is None:
            pass
        return body

    def marker_counters(self, frame, visit):
        frame_id = frame["id"]
        if type(frame_id) is not int or not 1 <= frame_id <= 2**31 - 1:
            raise FixtureError
        scopes = self.call("scopes", dict(frameId=frame_id))["scopes"]
        local, = [s for s in scopes if s["name"] == "Locals" and s.get("presentationHint") == "locals"]
        ref = local["variablesReference"]
        if local["expensive"] is not False or type(ref) is not int or not 1 <= ref <= 2**31 - 1:
            raise FixtureError
        # This source frame contains exactly three integer arguments, no
        # objects. Never request globals, another frame, or a child reference.
        variables = self.call("variables", dict(variablesReference=ref, filter="named",
                                                start=0, count=3))["variables"]
        allowed = {"visit", "marker_allocations", "marker_live"}
        values = {}
        if len(variables) != 3:
            raise FixtureError
        for item in variables:
            name, value = item["name"], item["value"]
            if (name not in allowed or name in values or item["type"] != "int"
                    or type(item["variablesReference"]) is not int or item["variablesReference"] != 0
                    or value not in ("0", "1", "2", "3")):
                raise FixtureError
            values[name] = value
        if values["visit"] != visit:
            raise FixtureError
        return bytes((int(values["marker_allocations"]), int(values["marker_live"])))

    def inspect(self, visit, work, target):
        capabilities = self.call("initialize", dict(adapterID="debugpy", pathFormat="path",
                                 linesStartAt1=True, columnsStartAt1=True, supportsVariableType=True))
        if capabilities.get("supportsConditionalBreakpoints") is not True:
            raise ProtocolError
        # debugpy does not pass pythonArgs to the intermediate launcher. GNU
        # env splits one constant literal into argv, without invoking a shell.
        self.launch_seq = self.send("launch", dict(type="debugpy", request="launch", name="owned",
                                    console="internalConsole", program=target, cwd=str(work),
                                    python=PYTHON, pythonArgs=["-I", "-S", "-B"],
                                    debugLauncherPython="/usr/bin/env",
                                    debugLauncherPath="-S /usr/bin/python3 -ISB /proc/self/cwd/launcher.py",
                                    debugAdapterHost="127.0.0.1", subProcess=False,
                                    justMyCode=True, showReturnValue=False, redirectOutput=False))
        while not self.initialized:
            self.dispatch()
        condition = {"2": "visit == 2", "3": "visit == 3"}[visit]
        bp, = self.call("setBreakpoints", dict(source=dict(path=target),
                        breakpoints=[dict(line=3, condition=condition)]))["breakpoints"]
        if bp["verified"] is not True or bp["line"] != 3:
            raise FixtureError
        self.call("configurationDone", {})
        while not self.launched or self.thread is None:
            self.dispatch()
        stack = self.call("stackTrace", dict(threadId=self.thread, startFrame=0, levels=3))
        frames = stack["stackFrames"]
        if len(frames) != 3 or any(f["source"]["path"] != target for f in frames):
            raise FixtureError
        # Send only six bytes encoding the observed names/physical lines.
        # The host must recognize that visit's stack before it allows resume.
        names = {"count": 1, "second": 2, "third": 3, "main": 4}
        evidence = bytearray()
        for frame in frames:
            line = frame["line"]
            if frame["name"] not in names or type(line) is not int or not 1 <= line <= 20:
                raise FixtureError
            evidence.extend((names[frame["name"]], line))
        sys.stdout.buffer.write(evidence)
        sys.stdout.buffer.flush()
        action = sys.stdin.buffer.read(1)
        if action == b"M":
            # Only the host's verified-stack branch sends M. Recheck the
            # top location here before requesting the integer-only locals.
            if (frames[0]["name"], frames[0]["line"]) != ("count", 3):
                raise FixtureError
            sys.stdout.buffer.write(self.marker_counters(frames[0], visit))
            sys.stdout.buffer.flush()
            action = sys.stdin.buffer.read(1)
        if action != b"C":
            raise ProtocolError
        self.resumed = True
        self.call("continue", dict(threadId=self.thread))
        while not self.terminated:
            self.dispatch()
        self.call("disconnect", {})


def main():
    work = Path.cwd()
    target = prepare(work)
    visit = os.environ["OWNED_VISIT"]
    # Keep glibc's per-thread arenas within the address-space limit.
    env = dict(PATH="/usr/bin", HOME=str(work), TMPDIR=str(work), MALLOC_ARENA_MAX="2", PYTHONNOUSERSITE="1",
               PYTHONDONTWRITEBYTECODE="1", PYDEVD_USE_FRAME_EVAL="NO", PYDEVD_USE_CYTHON="NO")
    with subprocess.Popen([PYTHON, "-ISB", str(work / "debugpy/adapter")],
                          env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.DEVNULL) as process:
        try:
            Session(process).inspect(visit, work, target)
            process.stdin.close()
            if process.wait(timeout=2) != 0:
                raise FixtureError
        finally:
            if process.poll() is None:
                process.kill()


if __name__ == "__main__":
    try:
        main()
    except FixtureError:
        sys.exit(66)
    except (ProtocolError, KeyError, ValueError, TypeError, RecursionError, BrokenPipeError):
        sys.exit(65)
