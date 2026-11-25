"""Click real workbench controls and check their visible text in a private Wayland display."""
import csv
import io
import itertools
import os
from pathlib import Path
import re
import socket
import struct
import subprocess as sp
import time


def run(args, **kw):
    return sp.run(args, check=True, stdout=sp.PIPE, stderr=sp.DEVNULL, timeout=8, **kw).stdout


def screen(region=None, psm="11", scale=1):
    x, y = 0, 0
    args = ["grim", "-s", str(scale)]
    if region:
        x, y, w, h = region
        args += ["-g", f"{x},{y} {w}x{h}"]
    png = run([*args, "-"])
    data = run(["tesseract", "stdin", "stdout", "--psm", psm, "tsv"], input=png).decode()
    words = [r for r in csv.DictReader(io.StringIO(data), delimiter="\t", quoting=csv.QUOTE_NONE)
             if r["level"] == "5" and r["text"]]
    lines = []
    for _, group in itertools.groupby(words, lambda r: (r["block_num"], r["par_num"], r["line_num"])):
        row = list(group)
        lines.append((" ".join(w["text"] for w in row), x + int(row[0]["left"]) // scale,
                      y + (int(row[0]["top"]) + int(row[0]["height"]) // 2) // scale))
    return lines


def find(pattern, stage, **capture):
    end = time.monotonic() + 30
    while time.monotonic() < end:
        rows = screen(**capture)
        found = [r for r in rows if re.search(pattern, r[0], re.I)]
        if found:
            return found[0], rows
        time.sleep(0.3)
    raise RuntimeError(stage)


def advertise_pointer(conn, display):
    # Keep one idle pointer on the private headless seat *before* Electron
    # connects. wlrctl's one-shot pointer disappears before Electron binds it.
    conn.settimeout(3)
    conn.connect(str(display))

    def request(obj, opcode, args):
        conn.sendall(struct.pack("=IHH", obj, opcode, 8 + len(args)) + args)

    request(1, 1, struct.pack("=I", 2))  # wl_display.get_registry
    with conn.makefile("rb") as events:
        for _ in range(128):
            obj, opcode, size = struct.unpack("=IHH", events.read(8))
            assert 8 <= size <= 1024
            data = events.read(size - 8)
            if obj == 2 and opcode == 0 and data[8:-4].rstrip(b"\0") == b"zwlr_virtual_pointer_manager_v1":
                # wl_registry.bind: reuse the advertised name and padded interface string.
                request(2, 0, data[:-4] + struct.pack("=II", 1, 3))
                request(3, 0, struct.pack("=II", 0, 4))  # create_virtual_pointer(default seat)
                return
    raise RuntimeError("no private virtual pointer manager")


def click(row):
    _, x, y = row
    assert 0 < x < 1440 and 0 < y < 1000
    run(["swaymsg", "seat", "seat0", "cursor", "set", str(x + 5), str(y)])
    run(["wlrctl", "pointer", "click"])
    time.sleep(0.3)


def title_patch():
    return run(["grim", "-t", "ppm", "-g", "540,454 144x48", "-"])


def check_dialog(stage, backdrop):
    # On this fixed 1440x1000 display at zoom level 1, crop the entire message
    # column. The surrounding editor/Call Stack confuses sparse full-screen OCR.
    _, rows = find(r"^Paused project locations$", f"{stage}: no result dialog",
                   region=(510, 435, 505, 140), psm="6", scale=2)
    expected = ["Paused project locations", "src/pre_cognition.rs:22"]
    if stage == "both":
        expected.insert(1, "src/endpoint.rs:28 [conditional breakpoint configured]")
    assert [text for text, _, _ in rows] == expected, f"{stage}: wrong dialog rows"
    assert title_patch() != backdrop, f"{stage}: modal did not cover editor"
    # Read the button face, excluding the focus outline. Its vertical position
    # follows the last detail line (and changes for the shorter caller dialog).
    button, _ = find(r"^OK$", f"{stage}: no dialog dismissal",
                     region=(956, rows[-1][2] + 45, 44, 21), psm="7", scale=2)
    click(button)
    for _ in range(10):
        if title_patch() == backdrop:
            return
        time.sleep(0.1)
    raise RuntimeError(f"{stage}: dialog did not dismiss")


def allocation_dialogs():
    def confirm(expected):
        _, rows = find(r"^Project allocation counts$", "missing consent dialog",
                       region=(510, 430, 505, 140), psm="6", scale=2)
        text = " ".join(row[0] for row in rows)
        assert expected in text, text
        # Rightmost primary button in the fixed Linux workbench. Downstream
        # assertions require its action; choosing Cancel cannot pass the gate.
        click(("", 960, rows[-1][2] + 57))

    confirm("Enable four aggregate allocator-call counters for this local launch only?")
    for stage, values in [("first", [1, 1, 0, 0]), ("second", [1, 1, 1, 2]), ("negative", None)]:
        find(r"^EON:\s*Show Project Allocation Counts", f"{stage}: no registered palette command")
        backdrop = title_patch()
        Path(f"runtime/{stage}.palette").touch()
        if values:
            confirm("Show the four aggregate counters for this paused launch?")
        # Text lives above the action bar. Do not accept occurrences from the editor.
        _, rows = find(r"^Project allocation counts$", f"{stage}: no result modal",
                       region=(510, 380, 505, 205), psm="6", scale=2)
        texts = [text for text, _, _ in rows]
        first = texts.index("Project allocation counts")
        if values:
            expected = [f"{label}: {n}" for label, n in zip(
                ["Successful malloc calls", "Successful calloc calls",
                 "Successful positive-size realloc calls", "Non-null free calls"], values)]
            assert texts[first+1:] == expected, (stage, texts)
        else:
            assert " ".join(texts[first+1:]) == "No counts shown. Use an opted-in, linked local launch and select a paused call frame.", texts
        assert title_patch() != backdrop
        last = rows[first+4] if values else next(row for row in rows if "paused call frame." in row[0])
        ok, _ = find(r"^OK$", f"{stage}: no dismissal", region=(956, last[2] + 45, 44, 21), psm="7", scale=2)
        click(ok)
        for _ in range(20):
            if title_patch() == backdrop:
                break
            time.sleep(0.1)
        assert title_patch() == backdrop, f"{stage}: modal still covers editor"
        Path(f"runtime/{stage}.seen").touch()
    for _ in range(60):
        if Path("runtime/done").is_file():
            print("Stock lldb-dap resolved keys:", Path("runtime/profile").read_text())
            print("VS Code allocation dialogs: malloc/calloc/realloc/free 1/1/0/0 then 1/1/1/2; opted-out launch: no counts.")
            return
        time.sleep(0.2)
    raise RuntimeError("allocation session did not finish")


def verify():
    os.chdir("/home/ui")
    os.umask(0o077)
    assert os.geteuid() == 1001 and not Path("/home/runner").exists()
    assert re.search(r"^CapEff:\s*0+$", Path("/proc/self/status").read_text(), re.M)
    assert len(Path("/proc/net/dev").read_text().splitlines()) == 3
    with sp.Popen(["sway", "-c", "/home/ui/sway.conf"], stderr=sp.DEVNULL, stdout=sp.DEVNULL) as sway, \
            socket.socket(socket.AF_UNIX) as pointer:
        try:
            for _ in range(60):
                xs = [p for p in Path("runtime").glob("wayland-*") if p.is_socket()]
                ipc = list(Path("runtime").glob("sway-ipc.*.sock"))
                if xs and ipc:
                    break
                assert sway.poll() is None, "no private compositor"
                time.sleep(0.1)
            os.environ["WAYLAND_DISPLAY"] = xs[0].name
            os.environ["SWAYSOCK"] = str(ipc[0].resolve())
            advertise_pointer(pointer, xs[0])
            with sp.Popen([
                "/opt/code/code", "--ozone-platform=wayland", "--enable-features=UseOzonePlatform",
                "--no-sandbox", "--disable-gpu", "--disable-crash-reporter", "--skip-welcome",
                "--skip-release-notes", "--disable-updates", "--disable-telemetry", "--locale=en",
                "--user-data-dir=/home/ui/profile", "--extensions-dir=/home/ui/extensions",
                "/home/ui/project",
            ], stdout=sp.DEVNULL, stderr=sp.DEVNULL) as code:
                try:
                    if Path("project/allocation_subject.c").is_file():
                        allocation_dialogs()
                        return
                    for stage in ["both", "caller"]:
                        find(r"^EON:\s*Show Paused Project Locations", "no palette on real pause")
                        Path(f"runtime/{stage}.palette").touch()
                        _, rows = find(r"Show locations once for my local debug session", f"{stage}: file picker not visible")
                        picker = "\n".join(r[0] for r in rows)
                        assert "endpoint.rs" in picker and "pre_cognition.rs" in picker
                        backdrop = title_patch()  # Unobscured editor pixels below the picker.
                        Path(f"runtime/{stage}.picker").touch()
                        check_dialog(stage, backdrop)
                        Path(f"runtime/{stage}.seen").touch()
                    for _ in range(60):
                        if Path("runtime/done").is_file():
                            break
                        time.sleep(0.2)
                    assert Path("runtime/done").is_file(), "debug session did not finish"
                    print("VS Code: real DebugStackFrame; both: src/endpoint.rs:28 [conditional breakpoint configured], src/pre_cognition.rs:22;"
                          " caller only: src/pre_cognition.rs:22.")
                finally:
                    if not Path("runtime/done").exists():
                        phase = Path("runtime/phase")
                        print("Test phase:", phase.read_text() if phase.exists() else "before extension host")
                    if code.poll() is None:
                        code.terminate()
        finally:
            sway.terminate()


if __name__ == "__main__":
    verify()
