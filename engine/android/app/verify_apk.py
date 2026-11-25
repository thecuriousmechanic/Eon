"""Checks the signed APK, not the cargo output directory. Requires GNU-compatible readelf."""

from pathlib import Path
import re
import struct
import subprocess
from zipfile import ZIP_STORED, ZipFile


# ELF EI_CLASS and e_machine; refuse a host/x86 placeholder packaged under an ARM name.
ABIS = {"arm64-v8a": (2, 183), "armeabi-v7a": (1, 40), "x86_64": (2, 62)}
FUNCTIONS = {
    "generate_ulid", "get_monotonic_time_nanos", "select_model_profile", "endpoint_silence_ms",
    "inject_audio_pcm_for_testing", "execute_task_sync", "validate_task_dag", "run_profile_workflow",
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def check_elf(path, abi, library):
    with path.open("rb") as f:
        header = f.read(20)
    require(header[:4] == b"\x7fELF" and header[5] == 1, f"{path}: not a little-endian ELF")
    elf_type, machine = struct.unpack_from("<HH", header, 16)
    require(elf_type == 3 and (header[4], machine) == ABIS[abi], f"{path}: wrong ELF type/class/machine")
    headers = subprocess.check_output(["readelf", "-lW", path], text=True)
    alignments = [int(line.split()[-1], 16) for line in headers.splitlines() if line.lstrip().startswith("LOAD ")]
    require(alignments and min(alignments) >= 16384, f"{path}: incompatible ELF load alignment")
    dynamic = subprocess.check_output(["readelf", "-dW", path], text=True)
    needed = set(re.findall(r"\(NEEDED\).*?\[(.*?)\]", dynamic))
    require(needed and needed <= {"libc.so", "libm.so", "libdl.so", "liblog.so", "libandroid.so"},
            f"{path}: unexpected or missing Android dependencies: {needed}")
    symbols = subprocess.check_output(["readelf", "--dyn-syms", "-W", path], text=True)
    check_exports(path, library, symbols)


def check_exports(path, library, symbols):
    exports = {fields[-1] for line in symbols.splitlines() if len(fields := line.split()) >= 8
               and fields[3:5] == ["FUNC", "GLOBAL"] and fields[5] in ("DEFAULT", "PROTECTED")
               and fields[6] != "UND"}
    expected = {"JNI_OnLoad"} if library == "libjnidispatch.so" else {
        f"uniffi_eon_uniffi_{kind}_func_{func}" for kind in ("fn", "checksum") for func in FUNCTIONS
    }
    require(not (expected - exports), f"{path}: missing dynamic exports {expected - exports}")


def verify(apk: Path, tools: Path, work: Path):
    subprocess.run([tools / "apksigner", "verify", "--min-sdk-version", "28", apk], check=True)
    subprocess.run([tools / "zipalign", "-c", "-P", "16", "4", apk], check=True)
    with ZipFile(apk) as archive:
        expected = {f"lib/{abi}/{lib}" for abi in ABIS for lib in ("libeon_uniffi.so", "libjnidispatch.so")}
        entries = [e for e in archive.infolist() if e.filename.startswith("lib/")]
        require(len(entries) == len(expected) and {e.filename for e in entries} == expected,
                "missing, extraneous or duplicate native APK entries")
        require(archive.read("classes.dex").startswith(b"dex\n"), "missing app dex")
        for entry in entries:
            _, abi, name = entry.filename.split("/")
            require(entry.compress_type == ZIP_STORED, f"{entry.filename} must be mmap-able")
            path = work / f"{abi}-{name}"
            path.write_bytes(archive.read(entry))
            try:
                check_elf(path, abi, name)
            finally:
                path.unlink()
    print(f"Verified API 28 signature, 16 KiB alignment, DEX, six native libraries and bridge exports: {', '.join(ABIS)}")
