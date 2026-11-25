#!/usr/bin/env python3
"""Measure end-of-utterance to speaker onset in paired, single-clock PCM captures."""

import argparse
import hashlib
import json
import sys
import wave
from dataclasses import dataclass
from pathlib import Path
from struct import iter_unpack

MIN_TRIALS = 100
P50_LIMIT_MS = 170
P99_LIMIT_MS = 280
SUSTAINED_OUTPUT_MS = 5


@dataclass(frozen=True)
class Capture:
    rate: int
    frames: int
    last_input_start: int
    last_input_end: int
    first_output_end: int | None
    any_output: bool
    fingerprint: bytes

    def latency_ms(self) -> int:
        if self.first_output_end is None:
            raise ValueError("no sustained speaker response")
        if self.first_output_end <= self.last_input_end:
            raise ValueError("channels overlap or the response preceded the end of the utterance")
        # Latest possible onset in the first output window minus earliest
        # possible endpoint in the last input window, in actual sample frames.
        n = (self.first_output_end - self.last_input_start) * 1000
        return (n + self.rate - 1) // self.rate


def _analyze(path: Path, threshold: int) -> Capture:
    if sys.byteorder != "little":
        raise ValueError("PCM analysis requires a little-endian host")
    if not 1 <= threshold <= 32767:
        raise ValueError("RMS threshold must be between 1 and 32767 PCM units")
    try:
        reader = wave.open(str(path), "rb")
    except RuntimeError as error:
        # wave raises RuntimeError when skipping a subchunk outside the enclosing RIFF.
        raise ValueError("invalid WAV chunk bounds") from error
    with reader as wav:
        rate = wav.getframerate()
        frames = wav.getnframes()
        if (wav.getnchannels(), wav.getsampwidth(), wav.getcomptype()) != (2, 2, "NONE"):
            raise ValueError("expected stereo uncompressed 16-bit PCM WAV")
        if not 8000 <= rate <= 192000 or not 0 < frames <= 30 * rate:
            raise ValueError("expected a capture of at most 30 seconds at 8 to 192 kHz")
        first_input = first_output = None
        last_start = last_end = onset_end = output_run = input_windows = pos = 0
        any_output = False
        fingerprint = hashlib.sha256()
        for ms in range((frames * 1000 + rate - 1) // rate):
            end = min(frames, (ms + 1) * rate // 1000)
            samples = wav.readframes(end - pos)
            if len(samples) != 4 * (end - pos):
                raise ValueError("truncated capture")
            fingerprint.update(samples)
            left = right = 0
            for a, b in iter_unpack("<hh", samples):
                left += a * a
                right += b * b
            cutoff = threshold * threshold * (end - pos)
            if left >= cutoff:
                first_input = ms if first_input is None else first_input
                last_start, last_end = pos, end
                input_windows += 1
            if right >= cutoff:
                any_output = True
                if ms < 50:
                    raise ValueError("speaker channel is not quiet during lead-in")
                if not output_run:
                    onset_end = end
                output_run += 1
                if output_run >= SUSTAINED_OUTPUT_MS and first_output is None and end == (ms + 1) * rate // 1000:
                    first_output = onset_end
            else:
                output_run = 0
            pos = end
        if first_input is None or first_input < 50 or input_windows < 20:
            raise ValueError("need a quiet lead-in and a measurable input utterance")
        return Capture(rate, frames, last_start, last_end, first_output, any_output, fingerprint.digest())


def latency_ms(path: Path, threshold: int) -> int:
    """Measure one trial, without claiming a percentile or a physical origin."""
    return _analyze(path, threshold).latency_ms()


def summarize(trials: list[int]) -> dict:
    if len(trials) < MIN_TRIALS:
        raise ValueError(f"need at least {MIN_TRIALS} trials to report P99")
    ordered = sorted(trials)
    p50, p99 = (ordered[(len(ordered) * p + 99) // 100 - 1] for p in (50, 99))
    return {"trials": len(trials), "p50_ms": p50, "p99_ms": p99,
            "passed": p50 < P50_LIMIT_MS and p99 < P99_LIMIT_MS}


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("captures", type=Path, help="directory with one .wav per trial")
    parser.add_argument("--threshold", required=True, type=int, help="calibrated RMS threshold in PCM units")
    parser.add_argument("--muted-control", type=Path, help="separate same-route recording with stimulus and speaker muted")
    args = parser.parse_args(argv)
    try:
        if not args.captures.is_dir():
            raise ValueError("capture directory does not exist")
        trials = args.captures.resolve()
        if trials.name == "trials" and (
                (trials.parent / "session.json").exists() or (trials.parent / "muted-control.wav").exists()):
            raise ValueError("rig session: run scripts/eval/physical_session.py on the session directory; "
                             "WAVs alone omit responder outcomes")
        control = None
        if args.muted_control is not None:
            if not args.muted_control.is_file():
                raise ValueError("missing muted control WAV")
            control = _analyze(args.muted_control, args.threshold)
            if control.any_output:
                raise ValueError("muted control exceeds the speaker threshold; check isolation and gains")
            if (control.frames - control.last_input_end) * 1000 < P99_LIMIT_MS * control.rate:
                raise ValueError("muted control needs at least 280 ms after the stimulus")
        paths = sorted(args.captures.glob("*.wav"))
        values = []
        seen = set()
        rate = control.rate if control is not None else None
        for path in paths:
            if not path.is_file():
                raise ValueError(f"not a regular capture file: {path.name}")
            try:
                sample = _analyze(path, args.threshold)
                if rate is None:
                    rate = sample.rate
                elif sample.rate != rate:
                    raise ValueError("capture rate differs from " + ("muted control" if control else "first trial"))
                latency = sample.latency_ms()
            except (ValueError, wave.Error, EOFError) as error:
                raise ValueError(f"{path.name}: {error}") from error
            if sample.fingerprint in seen:
                raise ValueError(f"duplicate PCM recording: {path.name}")
            seen.add(sample.fingerprint)
            values.append(latency)
        result = summarize(values)
        result["muted_control_checked"] = control is not None
        print(json.dumps(result))
        return 0 if result["passed"] else 1
    except (OSError, ValueError, wave.Error, EOFError) as error:
        print(f"TTFA benchmark failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
