"""Run the native active-switch regression on the locked pair and 12 LibriSpeech pairs."""

import argparse
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import wave

import noisy_duplex as duplex

TEST = "switch::active_speaker_switches_do_not_credit_a_departed_owner"
# Six lowest reader IDs per group in the pinned dev-clean split (Manual 06 §3.7).
GROUPS = ((84, 1462, 1673, 1919, 1988, 1993), (174, 251, 422, 652, 777, 1272))
PAIRS = tuple(pair for group in GROUPS for pair in zip(group, (*group[1:], group[0])))
EXPECTED = ["locked", *(f"{owner}:{other}" for owner, other in PAIRS)]
INPUT_ERRORS = duplex.INPUT_ERRORS + (subprocess.SubprocessError,)
CROP = 4 * duplex.RATE


def crop(flac):
    decoded = subprocess.run(["flac", "--silent", "--decode", "--stdout", "-"],
                             input=flac, capture_output=True, check=True)
    with wave.open(io.BytesIO(decoded.stdout), "rb") as wav:
        if (wav.getframerate(), wav.getnchannels(), wav.getsampwidth()) != (duplex.RATE, 1, 2):
            raise ValueError("expected 16 kHz mono PCM16 from LibriSpeech")
        pcm = wav.readframes(CROP)
    return pcm if len(pcm) == 2 * CROP else None


def reader_clips():
    entry = duplex.locked_models()["voice-test-librispeech-dev-clean"]
    clips = {reader: [] for group in GROUPS for reader in group}
    with (duplex.ROOT / entry["dest"]).open("rb") as f:
        duplex.verify(f, entry)
        f.seek(0)
        with tarfile.open(fileobj=f, mode="r|gz") as archive:
            for member in archive:
                match = re.fullmatch(r"LibriSpeech/dev-clean/(\d+)/(\d+)/(\1-\2-\d+)\.flac", member.name)
                if not (member.isfile() and match and int(match[1]) in clips):
                    continue
                candidates, name = clips[int(match[1])], match[3]
                # Select by utterance ID, not archive order or verification score.
                if len(candidates) == 2 and name >= candidates[-1][0]:
                    continue
                with archive.extractfile(member) as src:
                    pcm = crop(src.read())
                if pcm is not None:
                    candidates.append((name, pcm))
                    candidates.sort(key=lambda item: item[0])
                    del candidates[2:]
    if any(len(pair) != 2 or pair[0][0] == pair[1][0] for pair in clips.values()):
        raise ValueError("missing distinct 4 s enrollment and query for a selected reader")
    return clips


def write_clip(path, pcm):
    with wave.open(str(path), "wb") as wav:
        wav.setparams((1, 2, duplex.RATE, 0, "NONE", "not compressed"))
        wav.writeframes(pcm)


def observation(run, speaker_model=duplex.BASELINE_MODEL):
    model = re.findall(r"^handoff speaker model: (\S+)$", run.stderr, re.M)
    if model != [speaker_model]:
        return "unavailable", "requested speaker policy not confirmed"
    lines = run.stdout.splitlines()
    if (run.returncode == 0 and f"test {TEST} ... ok" in lines
            and any(s.startswith("test result: ok. 1 passed; 0 failed; 0 ignored;") for s in lines)):
        return "passed", None
    if run.returncode and f"test {TEST} ... FAILED" in lines:
        for line in run.stderr.splitlines():
            label, _, reason = line.partition(": ")
            if reason and label in ("handoff failed", "handoff unexercised"):
                return ("failed" if label == "handoff failed" else "unavailable"), reason
    return "unavailable", "native handoff result not confirmed"


def evaluate(pair, owner, stranger, speaker_model):
    env = {**os.environ, "ORT_SKIP_DOWNLOAD": "1", "EON_VOICE_MODELS": str(duplex.MODELS),
           **duplex.policy_env(speaker_model), "EON_HANDOFF_OWNER_WAV": str(owner),
           "EON_HANDOFF_STRANGER_WAV": str(stranger)}
    run = subprocess.run(["cargo", "test", "--offline", "--locked", "-p", "eon-voice", "--test", "speaker_e2e",
                          "--", "--exact", TEST, "--nocapture", "--format", "pretty"],
                         cwd=duplex.ROOT / "engine/core-rs", env=env, text=True, capture_output=True)
    status, reason = observation(run, speaker_model)
    if status != "passed":
        print(f"{pair}: {reason}\n{run.stdout}\n{run.stderr}", file=sys.stderr, flush=True)
    return {"pair": pair, "speaker_model": speaker_model, "status": status, "reason": reason, "native_exit_code": run.returncode}


def run_pairs(speaker_model=duplex.BASELINE_MODEL):
    models = duplex.locked_models()
    duplex.verify_native_models(models, speaker_model)
    try:
        paths = duplex.verified_paths(models, duplex.RECORDINGS)
    except duplex.INPUT_ERRORS as error:
        yield {"pair": "locked", "speaker_model": speaker_model, "status": "unavailable", "reason": str(error), "native_exit_code": None}
    else:
        yield evaluate("locked", paths["voice-test-sensevoice-en"], paths["voice-test-zipformer-0"], speaker_model)
    clips = reader_clips()
    with tempfile.TemporaryDirectory(prefix="eon-handoff-") as tmp:
        work = Path(tmp).resolve()
        owner, stranger = work / "owner.wav", work / "stranger.wav"
        for reader, negative in PAIRS:
            (enroll_id, enroll), (query_id, query) = clips[reader]
            negative_id, other = clips[negative][1]
            write_clip(owner, enroll + query)
            write_clip(stranger, other)
            yield {**evaluate(f"{reader}:{negative}", owner, stranger, speaker_model),
                   "utterances": [enroll_id, query_id, negative_id]}


def main(argv=None):
    cli = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    cli.add_argument("--speaker-model", choices=duplex.POLICIES, default=duplex.BASELINE_MODEL)
    pairs, states = [], set()
    try:
        for row in run_pairs(cli.parse_args(argv).speaker_model):
            print(json.dumps(row), flush=True)
            pairs.append(row["pair"])
            states.add(row["status"])
    except INPUT_ERRORS as error:
        print(error, file=sys.stderr)
        states.add("unavailable")
    return duplex.speaker_exit(states, pairs == EXPECTED)


if __name__ == "__main__":
    raise SystemExit(main())
