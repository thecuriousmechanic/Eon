"""Linux process limits for the fixed, trusted compiler/fixture invocations."""

import contextlib
import os
from pathlib import Path
import resource
import shutil
import signal
import subprocess
import time
import uuid


ROOT = Path(__file__).resolve().parents[3]
TARGET = ROOT / "engine/core-rs/target"
MAX_LOG = 64 * 1024


class BenchError(Exception):
    pass


@contextlib.contextmanager
def private_workspace():
    TARGET.mkdir(exist_ok=True)
    work = TARGET / ("benign-repair-" + uuid.uuid4().hex)
    work.mkdir(mode=0o700)
    try:
        yield work
    finally:
        shutil.rmtree(work)


def executable(name):
    path = shutil.which(name)
    if not path or not Path(path).is_absolute():
        raise BenchError(f"missing installed tool: {name}")
    return path


def limits():
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    resource.setrlimit(resource.RLIMIT_CPU, (35, 40))
    resource.setrlimit(resource.RLIMIT_FSIZE, (32 << 20, 32 << 20))
    resource.setrlimit(resource.RLIMIT_NOFILE, (256, 256))
    os.umask(0o077)


class Toolchain:
    def __init__(self, languages):
        self.bins = {}
        self.deadline = time.monotonic() + 900
        requirements = {"rust": ["rustc"], "typescript": ["tsc", "node"],
                        "kotlin": ["kotlinc", "java"], "python": ["python3"]}
        for language in languages:
            for name in requirements[language]:
                self.bins[name] = executable(name)
        if any(lang in languages for lang in ("rust", "typescript", "kotlin")):
            bridge = TARGET / "debug/eon-repair-edit"
            if not os.access(bridge, os.X_OK):
                raise BenchError("build eon-repair-edit first (see verification Manual 06 §3.6)")
            self.bins["edit"] = str(bridge)
        if "rust" in languages:
            # A rustup proxy needs the caller's toolchain installation, but not
            # their HOME/config/overrides in the child compiler environment.
            rustup = shutil.which("rustup")
            if rustup and os.path.samefile(self.bins["rustc"], rustup):
                resolved = subprocess.check_output(
                    [rustup, "which", "rustc"], cwd=ROOT, timeout=5, text=True
                ).strip()
                if not Path(resolved).is_absolute() or not os.access(resolved, os.X_OK):
                    raise BenchError("rustup did not resolve an installed compiler")
                self.bins["rustc"] = resolved
        self.path = os.pathsep.join(dict.fromkeys(
            [str(Path(p).parent) for p in self.bins.values()] + ["/usr/bin", "/bin"]
        ))

    def run(self, cwd, args, stdin=b""):
        remaining = min(45, self.deadline - time.monotonic())
        if remaining <= 0:
            raise BenchError("suite deadline")
        # Spool to bounded files, not unbounded PIPE buffers. Children inherit
        # the file limit (including grandchildren of compiler launchers).
        env = {
            "PATH": self.path, "HOME": str(cwd), "TMPDIR": str(cwd), "LC_ALL": "C.UTF-8",
            "JAVA_OPTS": f"-Xmx384m -XX:ActiveProcessorCount=2 -XX:-UsePerfData -Djava.io.tmpdir={cwd}",
            "GOMEMLIMIT": "512MiB", "GOMAXPROCS": "2",
        }
        with (cwd / "output").open("w+b") as output, (cwd / "input").open("w+b") as input_file:
            input_file.write(stdin)
            input_file.seek(0)
            proc = subprocess.Popen(
                args, cwd=cwd, stdin=input_file, stdout=output, stderr=subprocess.STDOUT,
                env=env, preexec_fn=limits, start_new_session=True,
            )
            try:
                status = proc.wait(timeout=remaining)
            except subprocess.TimeoutExpired as exc:
                raise BenchError("tool deadline") from exc
            finally:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                proc.wait()
            output.seek(0)
            data = output.read(MAX_LOG + 1)
            if len(data) > MAX_LOG or status < 0:
                raise BenchError("tool exceeded limits or terminated by signal")
            return status, data.decode("utf-8", errors="strict")
