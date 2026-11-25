"""Optional Linux acceptance: real Kitty and DEC sixel pixels on a new private Xvfb."""

import argparse
import os
from pathlib import Path
import secrets
import select
import socket
import struct
import subprocess
import tempfile
import time

from PIL import Image, ImageGrab


def stop(process):
    process.terminate()
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=3)


def eventually(probe, reason):
    end = time.monotonic() + 8
    while time.monotonic() < end:
        result = probe()
        if result:
            return result
        time.sleep(0.08)
    raise AssertionError(reason() if callable(reason) else reason)


def verify(binary, scratch, name, env):
    # All screen reads address only the new, authenticated Xvfb below. No screenshots saved.
    colors = [(224, 32, 64), (32, 208, 96)]
    source = Image.new("RGB", (120, 60), colors[0])
    source.paste(colors[1], (60, 0, 120, 60))
    path = scratch / "synthetic-still.jpg"
    source.save(path, quality=98, subsampling=0, progressive=True)
    args = [str(binary), "watch", str(scratch), "--image", str(path)]
    title = f"eon-pixels-{name}"
    if name == "kitty":
        args = ["kitty", "--config", "NONE", "--title", title, *args]
    else:
        # Some ncurses installations don't ship xterm-sixel; xterm then resets -tn.
        # Declare the confirmed VT340 mode to the child, not an ordinary xterm.
        args = ["xterm", "-ti", "vt340", "-fa", "monospace", "-title", title,
                "-e", "env", "TERM=xterm-sixel", *args]

    def xdo(*args):
        return subprocess.run(
            ["xdotool", *args], env=env, capture_output=True, timeout=3
        )

    def counts():
        bitmap = ImageGrab.grab(xdisplay=env["DISPLAY"]).convert("RGB")
        histogram = bitmap.getcolors(bitmap.width * bitmap.height)
        return [
            sum(n for n, rgb in histogram if max(abs(x - y) for x, y in zip(rgb, c)) < 14)
            for c in colors
        ]

    child = subprocess.Popen(args, env=env, stdin=subprocess.DEVNULL)
    try:
        win = eventually(lambda: xdo("search", "--onlyvisible", "--name", f"^{title}$").stdout.strip(), title)
        xdo("windowfocus", win).check_returncode()
        time.sleep(0.6)
        assert counts() == [0, 0]
        path.unlink()  # The source can't be reopened after this point.
        xdo("type", "/vision").check_returncode()
        xdo("key", "Return").check_returncode()
        def exact_pixels():
            values = counts()
            # Kitty's virtual placement rounds the raster up to whole cells.
            return values if all(3300 <= n <= 4600 for n in values) else None
        values = eventually(exact_pixels, lambda: f"{name}: no 120x60 two-color raster, got {counts()}, exit={child.poll()}")
        xdo("key", "Escape").check_returncode()
        eventually(lambda: counts() == [0, 0], f"{name}: pixels retained after close")
        xdo("type", "/vision").check_returncode()
        xdo("key", "Return").check_returncode()
        time.sleep(0.5)
        assert counts() == [0, 0], "the released image reappeared"
        xdo("key", "q").check_returncode()
        assert child.wait(timeout=3) == 0
        print(f"{name}: {values} source-color pixels; zero before open and after release/re-entry")
    finally:
        if child.poll() is None:
            stop(child)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("scratch", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    if [name for _, name in socket.if_nameindex()] != ["lo"]:
        raise SystemExit("Run in a private network namespace with only loopback up.")
    with tempfile.TemporaryDirectory(dir=args.scratch.resolve(strict=True)) as root:
        scratch = Path(root)
        authority = scratch / "Xauthority"
        # A private Xauthority for this throwaway display. No ambient X11/Wayland access.
        record = struct.pack(">H", 65535)
        for field in [b"", b"", b"MIT-MAGIC-COOKIE-1", secrets.token_bytes(16)]:
            record += struct.pack(">H", len(field)) + field
        authority.write_bytes(record)
        os.environ["XAUTHORITY"] = str(authority)  # libxcb inside Pillow uses the process env.
        env = dict(os.environ, XAUTHORITY=str(authority), HOME=root, TMPDIR=root,
                   XDG_RUNTIME_DIR=root, LIBGL_ALWAYS_SOFTWARE="1", KITTY_DISABLE_WAYLAND="1")
        for key in ["DISPLAY", "WAYLAND_DISPLAY", "TMUX", "STY", "XDG_CACHE_HOME", "DBUS_SESSION_BUS_ADDRESS"]:
            env.pop(key, None)
        server = subprocess.Popen(
            ["Xvfb", "-displayfd", "1", "-nolisten", "unix", "-nolisten", "local", "-listen", "tcp",
             "-nolock", "-noreset", "-auth", str(authority), "-screen", "0", "1024x768x24"],
            env=env, stdout=subprocess.PIPE,
        )
        try:
            if not select.select([server.stdout], [], [], 5)[0]:
                raise TimeoutError("Xvfb did not start")
            number = int(server.stdout.readline())
            env["DISPLAY"] = f"127.0.0.1:{number}"
            for name in ["kitty", "sixel"]:
                verify(binary, scratch, name, env)
        finally:
            stop(server)


if __name__ == "__main__":
    main()
