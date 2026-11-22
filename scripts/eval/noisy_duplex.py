"""Offline speaker decisions on two controls and three synthetic 0 dB scenes.

Requires the prepared speaker_e2e target, locked recordings, Silero and the
selected speaker model. The two speech beds also need the cached, locked SenseVoice release
archive. No downloads, devices, TTFA measurement or population error rates.
JSONL on stdout; native failure details on stderr. Exit 0 only if every
recorded speaker check passed; 1 for any measured failure, 2 for incomplete
evidence when no failure was measured.
"""

import argparse
import hashlib
import io
import json
import math
import os
from pathlib import Path
import random
import re
import struct
import subprocess
import sys
import tarfile
import tempfile
import wave

ROOT = Path(__file__).resolve().parents[2]
MODELS = ROOT / "perception/models"
RATE = 16_000
TEST = "recorded_capture_and_render_rings_make_one_auditable_owner_decision"
CLEAN, RUMBLE = "clean control", "0 dB rumble"
BABBLE, STREET, TV = "0 dB synthetic competing babble", "0 dB synthetic street traffic", "0 dB synthetic TV dialogue"
CONDITIONS = (CLEAN, RUMBLE, BABBLE, STREET, TV)
BASELINE_MODEL = "wespeaker-resnet34"
SPEAKER_MODELS = {
    BASELINE_MODEL: "speaker/wespeaker_resnet34.onnx",
    "camplusplus-zh-en-advanced": "speaker/camplusplus_zh_en_advanced.onnx",
}
AGREEMENT = "wespeaker-campp-agreement"
POLICIES = (*SPEAKER_MODELS, AGREEMENT)


def policy_models(policy):
    if policy == AGREEMENT:
        return BASELINE_MODEL, "camplusplus-zh-en-advanced"
    if policy not in SPEAKER_MODELS:
        raise ValueError("unknown speaker policy")
    return policy, "none"


def policy_env(policy):
    primary, confirmation = policy_models(policy)
    return {"EON_SPEAKER_MODEL": primary, "EON_SPEAKER_CONFIRMATION": confirmation}


INPUT_ERRORS = (OSError, EOFError, ValueError, tarfile.TarError, wave.Error, struct.error)


def verify(f, entry):
    if os.fstat(f.fileno()).st_size != entry["size_bytes"]:
        raise ValueError(f"{f.name}: size differs from model lock")
    if hashlib.file_digest(f, "sha256").hexdigest() != entry["sha256"]:
        raise ValueError(f"{f.name}: digest differs from model lock")


def locked_models():
    lock = json.loads((ROOT / "scripts/models.lock.json").read_text())
    return {entry["id"]: entry for entry in lock["models"]}


RECORDINGS = ("voice-test-sensevoice-en", "voice-test-zipformer-0")


def verified_paths(models, keys):
    paths = {}
    for key in keys:
        entry = models[key]
        path = ROOT / entry["dest"]
        with path.open("rb") as f:
            verify(f, entry)
        paths[key] = path
    return paths


def verify_native_models(models, speaker_model):
    encoders = tuple(k for k in policy_models(speaker_model) if k != "none")
    paths = verified_paths(models, ("silero-vad-v5", *encoders))
    if (paths["silero-vad-v5"] != MODELS / "vad/silero_vad.onnx"
            or any(paths[k] != MODELS / SPEAKER_MODELS[k] for k in encoders)):
        raise ValueError("pinned models differ from the paths opened by the voice target")


def pinned_inputs(speaker_model):
    models = locked_models()
    verify_native_models(models, speaker_model)
    return verified_paths(models, RECORDINGS), models["voice-test-sensevoice-en"]["archive"]


def read_pcm(source):
    with wave.open(str(source) if isinstance(source, Path) else source, "rb") as f:
        if (f.getframerate(), f.getnchannels(), f.getsampwidth(), f.getcomptype()) != (
            RATE, 1, 2, "NONE",
        ):
            raise ValueError("fixture must be mono 16 kHz PCM16")
        return struct.unpack(f"<{f.getnframes()}h", f.readframes(f.getnframes()))


def speech_tracks(archive):
    # Read only exact, distinct background recordings. The enrollment/query
    # (en.wav) and the near-end non-owner (Zipformer 0.wav) are never beds.
    path = MODELS / "downloads" / Path(archive["url"]).name
    prefix = path.name.removesuffix(".tar.bz2") + "/test_wavs/"
    wanted = {prefix + name + ".wav": name for name in ("ko", "zh", "ja")}
    tracks = {}
    with path.open("rb") as f:
        verify(f, archive)
        f.seek(0)
        with tarfile.open(fileobj=f, mode="r|bz2") as tar:
            for member in tar:
                if member.name not in wanted or not member.isfile():
                    continue
                with tar.extractfile(member) as src:
                    pcm = read_pcm(io.BytesIO(src.read()))
                rms = math.sqrt(sum(x * x for x in pcm) / len(pcm)) if pcm else 0
                if not rms:
                    raise ValueError(f"silent background speech: {member.name}")
                tracks[wanted.pop(member.name)] = tuple(x / rms for x in pcm)
                if not wanted:
                    break
    if wanted:
        raise ValueError(f"missing speech tracks: {sorted(wanted)}")
    return tracks


def rumble(length, seed):
    rng = random.Random(seed)
    state = 0.0
    noise = []
    for _ in range(length):
        state = 0.98 * state + 0.02 * (2 * rng.random() - 1)
        noise.append(state)
    return noise


def babble(length, tracks):
    # Three simultaneously playing speech clips, not three measured people.
    layers = [(tracks["ko"], 0), (tracks["zh"], RATE // 2), (tracks["ja"], RATE)]
    return [sum(track[(i + start) % len(track)] for track, start in layers) for i in range(length)]


def traffic(length):
    # Repeated pass-bys: an engine fundamental and harmonic that glide down
    # in pitch, plus modulated tire hiss over a quieter steady road bed.
    rng = random.Random(108)
    bed = []
    phase = hiss = 0.0
    for i in range(length):
        t = i % (RATE * 3) / (RATE * 3)
        envelope = math.sin(math.pi * t) ** 2
        phase += math.tau * (140 - 55 * t) / RATE
        hiss = 0.65 * hiss + 0.35 * (2 * rng.random() - 1)
        bed.append((0.3 + envelope) * hiss + envelope * (math.sin(phase) + 0.4 * math.sin(2 * phase)))
    return bed


def tv_dialogue(length, tracks):
    # Alternate two speech clips through a narrow TV-speaker approximation.
    voices = (tracks["ja"], tracks["zh"])
    turn, fade = RATE * 3 // 2, RATE // 100
    low = band = 0.0
    bed = []
    for i in range(length):
        k, pos = divmod(i, turn)
        voice = voices[k % 2]
        x = voice[((k // 2) * turn + pos) % len(voice)] * min(1, pos / fade, (turn - pos) / fade)
        low += 0.10 * (x - low)
        band += 0.65 * (x - low - band)
        bed.append(band)
    return bed


def mix_at_zero_db(samples, noise, *, interval=None):
    # Whole-clip energy by default, silences included. Measure from the final
    # PCM16 residual over the same interval used to set background gain.
    if len(samples) != len(noise):
        raise ValueError("foreground and background lengths differ")
    start, end = (0, len(samples)) if interval is None else interval
    if not 0 <= start < end <= len(samples):
        raise ValueError("invalid SNR measurement interval")
    signal_power = sum(x * x for x in samples[start:end])
    noise_power = sum(x * x for x in noise[start:end])
    if not signal_power or not noise_power:
        raise ValueError("0 dB SNR needs non-silent foreground and background")
    noise_gain = math.sqrt(signal_power / noise_power)
    mixed = [x + n * noise_gain for x, n in zip(samples, noise)]
    gain = min(1.0, 32_767 / max(1.0, max(map(abs, mixed))))
    output = tuple(round(gain * x) for x in mixed)
    residual_power = sum((y - gain * x) ** 2 for x, y in zip(samples[start:end], output[start:end]))
    snr_db = 10 * math.log10(gain * gain * signal_power / residual_power)
    if abs(snr_db) > 0.01:
        raise ValueError(f"quantized mixture misses 0 dB target: {snr_db:g}")
    return output, snr_db


def scene_inputs(condition, owner, other, tracks):
    half = len(owner) // 2  # Exactly the split made by the native recorded test.
    query = owner[half:]
    if condition == RUMBLE:
        query_bed, other_bed = rumble(len(query), 106), rumble(len(other), 107)
    else:
        length = len(other) + len(query)
        if condition == BABBLE:
            bed = babble(length, tracks)
        elif condition == STREET:
            bed = traffic(length)
        elif condition == TV:
            bed = tv_dialogue(length, tracks)
        else:
            raise ValueError(f"unknown scene: {condition}")
        other_bed, query_bed = bed[:len(other)], bed[len(other):]
    query, owner_snr = mix_at_zero_db(query, query_bed)
    other, other_snr = mix_at_zero_db(other, other_bed)
    return owner[:half] + query, other, owner_snr, other_snr


def write_pcm(path, samples):
    with wave.open(str(path), "wb") as f:
        f.setparams((1, 2, RATE, 0, "NONE", "not compressed"))
        f.writeframes(struct.pack(f"<{len(samples)}h", *samples))


def observations(output, returncode):
    # A renamed/filtered test can exit 0 without making a single inference.
    rejected = re.search(r"recorded non-owner: (\d+) candidate embeddings rejected", output)
    decision = re.search(r"recorded owner: verified after ([1-9]\d*) ms of fixture audio", output)
    missed = re.search(r"recorded owner: missed; (\d+) candidate embeddings rejected", output)
    false_interrupt = "recorded non-owner: interrupted playback" in output
    stale_interrupt = "recorded stale capture: interrupted playback" in output
    postcheck_failed = (returncode not in (None, 0) and "recorded owner: checking decision, mute and audit" in output
                        and f"test {TEST} ... FAILED" in output.splitlines())
    return {
        "native_exit_code": returncode,
        "duplex_check_passed": (returncode == 0 and rejected is not None and int(rejected[1]) > 0
                                and decision is not None and missed is None and not (false_interrupt or stale_interrupt)
                                and "test result: ok. 1 passed; 0 failed; 0 ignored;" in output),
        # This marker is emitted only after the entire negative interval's audit.
        "non_owner_embeddings_rejected": int(rejected[1]) if rejected else None,
        "non_owner_interruptions": 1 if false_interrupt else (0 if rejected else None),
        "stale_capture_interrupted": True if stale_interrupt else (False if rejected else None),
        "owner_missed": True if missed else (False if decision else None),
        "owner_postcheck_failed": True if postcheck_failed else (False if decision and returncode == 0 else None),
        "missed_owner_embeddings_rejected": int(missed[1]) if missed else None,
        "owner_decision_audio_ms": int(decision[1]) if decision and not missed else None,
        "ttfa_ms": None,
    }


def evaluate(label, owner_wav, other_wav, speaker_model):
    env = {
        **os.environ,
        "ORT_SKIP_DOWNLOAD": "1",
        "EON_VOICE_MODELS": str(MODELS),
        **policy_env(speaker_model),
        "EON_VOICE_TEST_OWNER_WAV": str(owner_wav),
        "EON_ZIPFORMER_TEST_WAV_0": str(other_wav),
    }
    run = subprocess.run(
        ["cargo", "test", "--offline", "--locked", "-j", "4", "-p", "eon-voice",
         "--test", "speaker_e2e", "--", "--exact", TEST, "--nocapture", "--format", "pretty"],
        cwd=ROOT / "engine/core-rs", env=env, text=True, capture_output=True, check=False,
    )
    # libtest may print an unterminated "test ... " prefix on stdout in serial mode.
    loaded = re.findall(r"^recorded speaker model: (\S+)$", run.stderr, re.M)
    output = run.stdout + "\n" + run.stderr
    if loaded == [speaker_model]:
        result = observations(output, run.returncode)
    else:
        result = {**observations("", run.returncode), "unavailable": "requested speaker model was not confirmed"}
    if not result["duplex_check_passed"]:
        print(f"{label}: recorded test not verified\n{output}", file=sys.stderr, flush=True)
    return result


def run_conditions(speaker_model=BASELINE_MODEL):
    try:
        paths, archive = pinned_inputs(speaker_model)
        owner_path, other_path = paths["voice-test-sensevoice-en"], paths["voice-test-zipformer-0"]
        owner, other = read_pcm(owner_path), read_pcm(other_path)
    except INPUT_ERRORS as error:
        for label in CONDITIONS:
            yield {"condition": label, "speaker_model": speaker_model, "unavailable": str(error), **observations("", None)}
        return
    try:
        tracks = speech_tracks(archive)
    except INPUT_ERRORS as error:
        tracks, speech_error = None, str(error)

    scratch = ROOT / "target"
    scratch.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="noisy-duplex-", dir=scratch) as work:
        for label in CONDITIONS:
            row = {
                "condition": label,
                "speaker_model": speaker_model,
                "owner_query_seconds": (len(owner) - len(owner) // 2) / RATE,
                "non_owner_seconds": len(other) / RATE,
                "mixed_owner_snr_db": None, "mixed_non_owner_snr_db": None,
            }
            try:
                owner_wav, other_wav = owner_path, other_path
                if label != CLEAN:
                    if label in (BABBLE, TV) and tracks is None:
                        raise ValueError(speech_error)
                    combined, negative, owner_snr, other_snr = scene_inputs(label, owner, other, tracks)
                    owner_wav, other_wav = Path(work) / "owner.wav", Path(work) / "non-owner.wav"
                    write_pcm(owner_wav, combined)
                    write_pcm(other_wav, negative)
                    row.update(mixed_owner_snr_db=round(owner_snr, 6), mixed_non_owner_snr_db=round(other_snr, 6))
                for role, wav in (("owner", owner_wav), ("non_owner", other_wav)):
                    row[f"{role}_wav_sha256"] = hashlib.sha256(wav.read_bytes()).hexdigest()
                row.update(evaluate(label, owner_wav, other_wav, speaker_model))
            except INPUT_ERRORS as error:
                row.update(unavailable=str(error), **observations("", None))
            yield row


def condition_status(row):
    if row["duplex_check_passed"]:
        return "passed"
    if (row["owner_missed"] or row["owner_postcheck_failed"] or row["non_owner_interruptions"]
            or row["stale_capture_interrupted"] or row["non_owner_embeddings_rejected"] == 0):
        return "failed"
    return "unavailable"


def speaker_exit(states, complete):
    if "failed" in states:
        return 1
    return 0 if complete and states == {"passed"} else 2


def main(argv=None):
    cli = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    cli.add_argument("--speaker-model", choices=POLICIES, default=BASELINE_MODEL)
    conditions, states = [], set()
    for row in run_conditions(cli.parse_args(argv).speaker_model):
        print(json.dumps(row), flush=True)
        conditions.append(row["condition"])
        states.add(condition_status(row))
    return speaker_exit(states, conditions == list(CONDITIONS))


if __name__ == "__main__":
    raise SystemExit(main())
