"""Acquire an operator-authorized stereo TTFA session on a dedicated Linux measurement rig."""

import argparse
from array import array
from bisect import bisect_left
from contextlib import contextmanager, nullcontext
import hashlib
import json
import os
from pathlib import Path
import re
import select
import struct
import subprocess
import sys
import time
from typing import NamedTuple
import wave

import physical_session as session
from stereo_pcm import capture_geometry, endpoint


def quiet_prefix(prefix, rate, threshold):
    # Match the analyzer's first 50 one-millisecond RMS windows on both channels.
    pcm = memoryview(prefix).cast("h")
    for ms in range(50):
        begin, end = ms * rate // 1000, (ms + 1) * rate // 1000
        cutoff = threshold * threshold * (end - begin)
        if any(sum(s * s for s in pcm[2 * begin + ch:2 * end:2]) >= cutoff for ch in (0, 1)):
            return False
    return True


def record(path, take, rate, seconds, threshold):
    prefix = bytearray()
    needed = 4 * (rate // 20)
    quiet = False
    with open(path, "xb", opener=lambda p, flags: os.open(p, flags, 0o600)) as output:
        with wave.open(output, "wb") as wav:
            wav.setparams((2, 2, rate, 0, "NONE", "not compressed"))

            def receive(stereo, _available=None):
                nonlocal quiet
                wav.writeframesraw(stereo)
                if len(prefix) < needed:
                    prefix.extend(stereo[:needed - len(prefix)])
                    if len(prefix) == needed and quiet_prefix(prefix, rate, threshold):
                        quiet = True
                        print("Apply stimulus now.", flush=True)

            outcome = take(seconds, receive)
    digest = hashlib.sha256()
    frames = rate * seconds
    with wave.open(str(path), "rb") as wav:
        layout = (wav.getnchannels(), wav.getsampwidth(), wav.getframerate(), wav.getnframes(), wav.getcomptype())
        if layout != (2, 2, rate, frames, "NONE"):
            raise ValueError("recorder did not deliver the requested stereo PCM clock and frame count")
        while frames:
            count = min(frames, rate)
            data = wav.readframes(count)
            if len(data) != count * 4:
                raise ValueError("truncated recording")
            digest.update(data)
            frames -= count
    if not quiet:
        raise ValueError("recording lacks a quiet lead-in; original retained")
    return digest.hexdigest(), outcome


class ReferencePipe:
    """Nonblocking during capture. Retain the exact unwritten suffix of this take."""

    def __init__(self, fd, limit):
        self.fd = fd
        self.limit = limit
        self.pending = bytearray()

    def flush(self):
        while self.pending:
            try:
                n = os.write(self.fd, self.pending)
            except BlockingIOError:
                return
            if not n:
                raise BrokenPipeError("responder stopped reading")
            del self.pending[:n]

    def offer(self, data):
        if len(self.pending) + len(data) > self.limit:
            self.flush()
            if len(self.pending) + len(data) > self.limit:
                raise OSError("responder reference buffer overflow")
        self.pending.extend(data)
        self.flush()

    def finish(self):
        # Waiting is allowed only after ALSA has stopped, never in the capture callback.
        deadline = time.monotonic() + 5
        while self.pending:
            wait = deadline - time.monotonic()
            if wait <= 0 or not select.select([], [self.fd], [], wait)[1]:
                raise TimeoutError("responder reference drain stalled")
            self.flush()


class BridgeOutcome(NamedTuple):
    started: bool
    playback_frame: int | None  # high-water mark including frames already queued in ALSA
    reference_frames: int | None  # frames read by the responder before enqueue, not an onset
    captured_before_reference: int | None  # frames acquired before the decisive reference was sent


@contextmanager
def responding_take(args, take):
    """One speaker handle through the control and all turns. No replacement worker."""
    command = [str(args.responder), "--live-reference-session", str(args.rate), args.speaker, str(args.models)]
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0) as worker:
        fd = worker.stdin.fileno()
        os.set_blocking(fd, False)
        out = worker.stdout.fileno()
        os.set_blocking(out, False)

        def read_status(size, timeout):
            data = bytearray()
            deadline = time.monotonic() + timeout
            while len(data) < size:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not select.select([out], [], [], remaining)[0]:
                    raise TimeoutError("responder did not acknowledge take")
                chunk = os.read(out, size - len(data))
                if not chunk:
                    raise ValueError("invalid or missing responder status")
                data.extend(chunk)
            return bytes(data)

        def status(timeout, allowed):
            value = read_status(1, timeout)
            if value not in allowed:
                raise ValueError("invalid or missing responder status")
            return value

        def poll_status():
            try:
                return os.read(out, 1)
            except BlockingIOError:
                return None

        try:
            def run(seconds, write):
                frames = args.rate * seconds
                pipe = ReferencePipe(fd, args.rate)  # 500 ms of mono i16 at the selected rate
                pipe.offer(struct.pack("<I", frames))
                pipe.finish()
                status(90, b"3")  # older workers cannot bind the response to a reference prefix
                received = 0
                playback_frame = None
                sent_at_playback = None
                delivered, captured = array("I"), array("I")
                fault = None

                def note_delivery(cutoff):
                    # A short write can end halfway through a mono sample. Only
                    # index complete frames, including pending bytes from earlier reads.
                    complete = received - (len(pipe.pending) + 1) // 2
                    if complete > (delivered[-1] if delivered else 0):
                        delivered.append(complete)
                        captured.append(min(frames, cutoff))

                def receive(stereo, available=None):
                    nonlocal fault, received, playback_frame, sent_at_playback
                    write(stereo)
                    if fault is None:
                        try:
                            n = len(stereo) // 4
                            if len(stereo) % 4 or n == 0 or received + n > frames:
                                raise OSError("unexpected capture frame count")
                            # Poll before offering this block: no terminal outcome is legal yet.
                            value = poll_status()
                            if available is None:
                                raise ValueError("no live capture availability for reference delivery")
                            queued = available()
                            if type(queued) is not int or queued < 0:
                                raise ValueError("invalid capture availability")
                            # This block and the observed ADC backlog already exist
                            # before any pending or new reference bytes can be written.
                            acquired = received + n + queued
                            if value is not None:
                                if value != b"P" or playback_frame is not None or not received:
                                    raise ValueError("unexpected live responder status")
                                playback_frame = acquired
                                sent_at_playback = delivered[-1] if delivered else 0
                            received += n
                            pipe.offer(memoryview(stereo).cast("h")[::2].tobytes())
                            note_delivery(acquired)
                        except (OSError, ValueError) as error:
                            # Stop playback but finish writing this original stereo WAV.
                            worker.kill()
                            fault = error

                take(seconds, receive)
                if fault is not None:
                    raise fault
                if received != frames:
                    raise ValueError("short reference take; original retained")
                pipe.finish()
                note_delivery(frames)
                # A P first received during drain still needs B, but is not a live observation.
                played = playback_frame is not None or status(10, b"PN") == b"P"
                reference_frames = None
                cutoff = None
                if played:
                    status(10, b"B")
                    reference_frames, = struct.unpack("<I", read_status(4, 10))
                    if not 0 < reference_frames <= frames or (
                            sent_at_playback is not None and reference_frames > sent_at_playback):
                        raise ValueError("invalid pre-enqueue reference count")
                    cutoff = captured[bisect_left(delivered, reference_frames)]
                if poll_status() is not None:
                    raise ValueError("unexpected responder status after take")
                return BridgeOutcome(played, playback_frame, reference_frames, cutoff)

            yield run
            # The worker checks the held stream one last time before a manifest is published.
            end = ReferencePipe(fd, 4)
            end.offer(bytes(4))
            end.finish()
            status(5, b"D")
            if worker.wait(timeout=5) != 0 or os.read(out, 1) != b"":
                raise ValueError("responder session failed; originals retained")
        except subprocess.TimeoutExpired as error:
            raise TimeoutError("responder did not stop") from error
        finally:
            if worker.poll() is None:
                worker.kill()


def configuration(args):
    info = {"origin": "authorized-single-clock-physical", "route": args.route, "gains": args.gains,
            "mode": "driven" if args.responder else "passive", "threshold_pcm": args.threshold,
            "sample_rate_hz": args.rate, "trials": {}}
    path = args.route_calibration
    if args.responder:
        if path is None or not path.is_absolute():
            raise ValueError("driven acquisition requires an absolute --route-calibration record")
        with session.regular(path).open("rb") as f:
            raw = f.read(16_385)
        if len(raw) > 16_384:
            raise ValueError("oversized route calibration")
        try:
            cal = json.loads(raw, object_pairs_hook=session.distinct_keys)
        except RecursionError as error:
            raise ValueError("route calibration nested too deeply") from error
        info.update(capture_device=args.device, speaker=args.speaker, route_calibration=cal)
        session.route_bound(info)
        capture_geometry(cal, args.rate)
    elif path is not None:
        raise ValueError("--route-calibration is for driven acquisition only")
    return info


def acquire(args):
    if sys.platform != "linux" or sys.byteorder != "little":
        raise ValueError("supported acquisition hosts are little-endian Linux")
    if not sys.stdin.isatty():
        raise ValueError("operator terminal required; no unattended recording")
    if not re.fullmatch(r"hw:[0-9]{1,3},[0-9]{1,3}", args.device):
        raise ValueError("select one dedicated hardware capture PCM as hw:CARD_NUMBER,DEVICE_NUMBER")
    if args.responder or args.speaker or args.models:
        if not (args.responder and args.responder.is_absolute() and args.models and args.models.is_absolute()
                and re.fullmatch(r"alsa:hw:CARD=[A-Za-z0-9_-]+,DEV=[0-9]{1,3}", args.speaker or "")):
            raise ValueError("select an absolute local responder, model directory and explicit ALSA hardware speaker")
        print(f"The local responder {args.responder} will receive the LEFT channel only, and play on {args.speaker}.")
    if not args.route.strip() or not args.gains.strip() or not 1 <= args.threshold <= 32767:
        raise ValueError("provide the common route, fixed gains and calibrated threshold")
    info = configuration(args)
    if not args.directory.is_absolute():
        raise ValueError("use a new absolute session directory under your private directory")
    parent = args.directory.parent.stat()
    if parent.st_uid != os.geteuid() or parent.st_mode & 0o022:
        raise ValueError("session parent must be owned by you and not group/other writable")

    labels = ("control", *session.SCENES) if args.noisy else ("control",)
    turns = [scene for scene in labels for _ in range(session.acoustic.MIN_TRIALS // len(labels))]
    print(f"{args.device}: input reference on LEFT, isolated acoustic speaker on RIGHT, same clock.\n"
          f"This records a muted control and {len(turns)} turns ({', '.join(labels)}) to {args.directory}.\n"
          "Use only a dedicated authorized rig, not a host microphone, digital loopback or generated WAV.\n"
          "Keep both channels quiet until the recorder cues you. Ctrl-C stops the session; no automatic retakes.")
    if input("Type RECORD to confirm physical routing and permission: ") != "RECORD":
        raise ValueError("recording not authorized")

    parent_fd = os.open(args.directory.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        actual = os.fstat(parent_fd)
        if not os.path.samestat(actual, parent) or actual.st_uid != os.geteuid() or actual.st_mode & 0o022:
            raise ValueError("session parent changed")
        os.mkdir(args.directory.name, 0o700, dir_fd=parent_fd)
        fd = os.open(args.directory.name, os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent_fd)
        try:
            calibration = info.get("route_calibration")
            with endpoint(args.device, args.rate, calibration) as take:
                if args.responder:
                    info["verified_capture_controller"] = calibration["capture_controller"]
                    info["capture_geometry_verified"] = True
                collect(args, Path(f"/proc/self/fd/{fd}"), turns, take, info)
        finally:
            os.close(fd)
    finally:
        os.close(parent_fd)


def collect(args, directory, turns, take, info):
    # Use the pinned private directory, not a path that can be redirected between takes.
    trials = directory / "trials"
    trials.mkdir(mode=0o700)
    control = directory / "muted-control.wav"
    if input("Mute the speaker, prepare the included stimuli and type MUTED to record the 30 s control: ") != "MUTED":
        raise ValueError("control cancelled")
    with responding_take(args, take) if args.responder else nullcontext(take) as capture:
        control_hash, outcome = record(control, capture, args.rate, 30, args.threshold)
        info["control_pcm_sha256"] = control_hash
        if args.responder:
            info["responder_protocol"] = 3
            info["control_bridge_started"] = outcome.started
            info["control_playback_frame"] = outcome.playback_frame
            if outcome.started:
                info["control_reference_frames"] = outcome.reference_frames
                info["control_captured_before_reference"] = outcome.captured_before_reference
        session.check_control(session.checked_capture(control, args.threshold, control_hash), info)
        print("Unmute the speaker. Keep the same clock, channel routing, gains and threshold.")
        seen = {control_hash}
        for i, scene in enumerate(turns, 1):
            token = f"TAKE {i}"
            if input(f"Turn {i}/{len(turns)}, {scene}. Type {token} to record {args.seconds} s: ") != token:
                raise ValueError("turn cancelled")
            name = f"turn-{i:03d}.wav"
            digest, outcome = record(trials / name, capture, args.rate, args.seconds, args.threshold)
            # Validate the recorded stimulus, never condition inclusion on the latency or a reply.
            session.checked_capture(trials / name, args.threshold, digest)
            if digest in seen:
                raise ValueError("duplicate PCM recording")
            seen.add(digest)
            label = {"scene": scene, "pcm_sha256": digest}
            if args.responder:
                label["bridge_started"] = outcome.started
                if outcome.started:
                    label["reference_frames"] = outcome.reference_frames
                    label["captured_before_reference"] = outcome.captured_before_reference
            info["trials"][name] = label
    # Do not select trials by outcome. Even a turn with no speaker response is in the inventory.
    pending = directory / "session.json.partial"
    with pending.open("x") as f:
        json.dump(info, f, indent=2)
        f.write("\n")
    pending.rename(directory / "session.json")


def main(argv=None):
    cli = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    cli.add_argument("directory", type=Path, help="new private directory; original WAVs never leave this host")
    cli.add_argument("--device", required=True, help="dedicated stereo ALSA hw:CARD_NUMBER,DEVICE_NUMBER")
    cli.add_argument("--route", required=True, help="common capture clock and acoustic routing")
    cli.add_argument("--gains", required=True, help="fixed gains used for both channels")
    cli.add_argument("--threshold", required=True, type=int, help="calibrated RMS threshold in PCM units")
    cli.add_argument("--rate", type=int, choices=(16000, 44100, 48000, 96000), default=48000)
    cli.add_argument("--seconds", type=int, choices=range(2, 31), default=5, metavar="2..30")
    cli.add_argument("--noisy", action="store_true", help="25 turns each of control, babble, traffic and TV")
    cli.add_argument("--responder", type=Path, help="absolute path to your trusted local eon-rig-turn executable")
    cli.add_argument("--models", type=Path, help="digest-verified local model directory for --responder")
    cli.add_argument("--speaker", help="dedicated responder output ID: alsa:hw:CARD=NAME,DEV=N")
    cli.add_argument("--route-calibration", type=Path, help="driven only: absolute path to the same-route "
                     "latency calibration JSON (Manual 06 §3.3)")
    args = cli.parse_args(argv)
    try:
        acquire(args)
    except (EOFError, KeyboardInterrupt, OSError, ValueError, wave.Error) as error:
        print(f"Acquisition stopped ({type(error).__name__}): {error}", file=sys.stderr)
        return 2
    print("Acquisition complete, not a latency verdict. Supply this directory to the full evaluator.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
