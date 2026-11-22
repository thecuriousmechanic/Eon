"""An already-authorized ALSA capture endpoint. Never reopen it between takes."""

from contextlib import contextmanager
import ctypes as c
import errno
import os
from pathlib import Path
import re
import stat
import time


def library():
    alsa = c.CDLL("libasound.so.2")
    ptr = c.c_void_p
    signatures = {
        "open": (c.c_int, c.POINTER(ptr), c.c_char_p, c.c_int, c.c_int),
        "close": (c.c_int, ptr),
        "info_malloc": (c.c_int, c.POINTER(ptr)),
        "info_free": (None, ptr),
        "info": (c.c_int, ptr, ptr),
        "info_get_card": (c.c_int, ptr),
        "info_get_device": (c.c_uint, ptr),
        "hw_params_malloc": (c.c_int, c.POINTER(ptr)),
        "hw_params_free": (None, ptr),
        "hw_params_any": (c.c_int, ptr, ptr),
        "hw_params_set_access": (c.c_int, ptr, ptr, c.c_int),
        "hw_params_set_format": (c.c_int, ptr, ptr, c.c_int),
        "hw_params_set_channels": (c.c_int, ptr, ptr, c.c_uint),
        "hw_params_set_rate": (c.c_int, ptr, ptr, c.c_uint, c.c_int),
        "hw_params_set_period_size": (c.c_int, ptr, ptr, c.c_ulong, c.c_int),
        "hw_params_set_buffer_size": (c.c_int, ptr, ptr, c.c_ulong),
        "hw_params": (c.c_int, ptr, ptr),
        "get_params": (c.c_int, ptr, c.POINTER(c.c_ulong), c.POINTER(c.c_ulong)),
        "prepare": (c.c_int, ptr),
        "readi": (c.c_long, ptr, ptr, c.c_ulong),
        "avail": (c.c_long, ptr),
        "wait": (c.c_int, ptr, c.c_int),
        "drop": (c.c_int, ptr),
    }
    for name, (result, *args) in signatures.items():
        fn = getattr(alsa, "snd_pcm_" + name)
        fn.argtypes, fn.restype = args, result
    return alsa


def check(result):
    if result < 0:
        raise OSError(-result, "ALSA capture failed")
    return result


def controller_id(value):
    if not isinstance(value, str) or not re.fullmatch(r"/dev/snd/by-id/usb-[A-Za-z0-9][A-Za-z0-9_.-]{0,250}", value):
        raise ValueError("expected a direct USB capture_controller under /dev/snd/by-id")
    return value


def capture_geometry(calibration, rate):
    period = calibration.get("capture_period_frames")
    buffer = calibration.get("capture_buffer_frames")
    if type(period) is not int or type(buffer) is not int or not 0 < period < buffer <= 30 * rate:
        raise ValueError("route calibration requires capture_period_frames < capture_buffer_frames "
                         "in positive frames within the 30 s control")
    return period, buffer


def verify_controller(alsa, pcm, device, identity):
    alias = Path(controller_id(identity))
    selected = re.fullmatch(r"hw:([0-9]{1,3}),([0-9]{1,3})", device)
    if not selected:
        raise ValueError("expected explicit capture PCM")
    card, number = map(int, selected.groups())
    info = c.c_void_p()
    check(alsa.snd_pcm_info_malloc(c.byref(info)))
    try:
        check(alsa.snd_pcm_info(pcm, info))
        if (alsa.snd_pcm_info_get_card(info), alsa.snd_pcm_info_get_device(info)) != (card, number):
            raise ValueError("opened PCM differs from selected capture device")
    finally:
        alsa.snd_pcm_info_free(info)
    control = Path(f"/dev/snd/controlC{card}")
    if os.path.realpath(alias.parent, strict=True) != str(alias.parent) or os.readlink(alias) != f"../{control.name}":
        raise ValueError("capture controller is not a direct link to the selected card control")
    node, target = control.lstat(), alias.stat()
    if not stat.S_ISCHR(node.st_mode) or not stat.S_ISCHR(target.st_mode) or node.st_rdev != target.st_rdev:
        raise ValueError("capture controller character device mismatch")

    # A model-only USB by-id link may be shared by multiple serial-less units.
    # Require the canonical serial-backed link in the udev record for this dev_t.
    dev = node.st_rdev
    with Path(f"/run/udev/data/c{os.major(dev)}:{os.minor(dev)}").open("rb") as f:
        raw = f.read(65_537)
    if len(raw) > 65_536:
        raise ValueError("oversized capture controller udev record")
    props = dict(line[2:].split("=", 1) for line in raw.decode().splitlines() if line.startswith("E:") and "=" in line)
    short, serial = props.get("ID_SERIAL_SHORT", ""), props.get("ID_SERIAL", "")
    interface = props.get("ID_USB_INTERFACE_NUM", "")
    if (props.get("ID_BUS") != "usb" or not short or not serial.endswith("_" + short)
            or alias.name != "usb-" + serial + ("-" + interface if interface else "")):
        raise ValueError("capture controller lacks matching USB serial evidence")


@contextmanager
def endpoint(device, rate, calibration=None):
    alsa = library()
    pcm = c.c_void_p()
    check(alsa.snd_pcm_open(c.byref(pcm), device.encode("ascii"), 1, 1))  # CAPTURE, NONBLOCK
    try:
        if calibration is not None:
            verify_controller(alsa, pcm, device, calibration["capture_controller"])
            period, buffer = capture_geometry(calibration, rate)
        params = c.c_void_p()
        check(alsa.snd_pcm_hw_params_malloc(c.byref(params)))
        try:
            check(alsa.snd_pcm_hw_params_any(pcm, params))
            check(alsa.snd_pcm_hw_params_set_access(pcm, params, 3))  # RW_INTERLEAVED
            check(alsa.snd_pcm_hw_params_set_format(pcm, params, 2))  # S16_LE
            check(alsa.snd_pcm_hw_params_set_channels(pcm, params, 2))
            # Exact, not set_rate_near. Reject an incompatible device before recording.
            check(alsa.snd_pcm_hw_params_set_rate(pcm, params, rate, 0))
            if calibration is not None:
                check(alsa.snd_pcm_hw_params_set_period_size(pcm, params, period, 0))
                check(alsa.snd_pcm_hw_params_set_buffer_size(pcm, params, buffer))
            check(alsa.snd_pcm_hw_params(pcm, params))
            if calibration is not None:
                actual_buffer, actual_period = c.c_ulong(), c.c_ulong()
                check(alsa.snd_pcm_get_params(pcm, c.byref(actual_buffer), c.byref(actual_period)))
                if (actual_period.value, actual_buffer.value) != (period, buffer):
                    raise ValueError("installed capture period or buffer differs from route calibration")
        finally:
            alsa.snd_pcm_hw_params_free(params)

        def available():
            # Sync the hardware pointer, without waiting for any new samples.
            return check(alsa.snd_pcm_avail(pcm))

        def take(seconds, write):
            check(alsa.snd_pcm_prepare(pcm))
            try:
                remaining = seconds * rate
                deadline = time.monotonic() + seconds + 1
                buf = c.create_string_buffer(4 * 1024)
                while remaining:
                    if time.monotonic() >= deadline:
                        raise TimeoutError("PCM capture stalled")
                    requested = min(1024, remaining)
                    n = alsa.snd_pcm_readi(pcm, buf, requested)
                    if n == -errno.EAGAIN:
                        check(alsa.snd_pcm_wait(pcm, 100))
                        continue
                    if not 0 < check(n) <= requested:
                        raise OSError("invalid PCM read size")
                    write(buf.raw[:4 * n], available)
                    remaining -= n
            finally:
                # No recovery, draining, or automatic restart after loss or overrun.
                check(alsa.snd_pcm_drop(pcm))

        yield take
    finally:
        check(alsa.snd_pcm_close(pcm))
