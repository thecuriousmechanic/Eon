"""Read a declared physical session, never audio devices. See verification Manual 06 §3.3."""

import argparse
import hashlib
import json
from pathlib import Path
import sys
import wave
from stereo_pcm import capture_geometry, controller_id

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import measure_ttfa as acoustic


SCENES = ("babble-0db", "traffic-0db", "tv-0db")
INPUT_ERRORS = (ValueError, OSError, EOFError, wave.Error)


def distinct_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate manifest key: {key}")
        result[key] = value
    return result


def regular(path):
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"expected regular file: {path.name}")
    return path


def checked_capture(path, threshold, digest):
    capture = acoustic._analyze(regular(path), threshold)
    if capture.fingerprint.hex() != digest:
        raise ValueError(f"{path.name}: PCM differs from declared recording")
    return capture


def route_bound(info):
    cal = info.get("route_calibration")
    if not isinstance(cal, dict) or not isinstance(cal.get("reference"), str) or not cal["reference"].strip():
        raise ValueError("driven recording requires a route calibration reference")
    controller_id(cal.get("capture_controller"))
    for key in ("capture_device", "speaker", "route", "gains"):
        if not isinstance(cal.get(key), str) or not cal[key].strip() or cal[key] != info.get(key):
            raise ValueError(f"route calibration differs from acquisition {key}")
    if type(cal.get("sample_rate_hz")) is not int or cal["sample_rate_hz"] != info["sample_rate_hz"]:
        raise ValueError("route calibration differs from acquisition sample_rate_hz")
    # Older recorders copied operator-supplied calibration keys verbatim. A
    # size in those notes does not mean ALSA was configured or read back.
    if info.get("responder_protocol") == 3 or "capture_geometry_verified" in info:
        if info.get("capture_geometry_verified") is not True:
            raise ValueError("invalid capture_geometry_verified marker")
        capture_geometry(cal, info["sample_rate_hz"])
    bound = cal.get("upper_bound_frames")
    if type(bound) is not int or not 0 < bound < 30 * info["sample_rate_hz"]:
        raise ValueError("route calibration requires a positive integer upper_bound_frames within the control")
    return bound


def check_control(control, info):
    if control.any_output or (control.frames - control.last_input_end) * 1000 < acoustic.P99_LIMIT_MS * control.rate:
        raise ValueError("muted control exceeds threshold or lacks post-stimulus tail")
    started = bridge_started(info, "control_bridge_started", info["mode"])
    before = reference_bound(info, info, "control_", started, control.frames)
    if started is False:
        raise ValueError("driven muted control did not start a bridge")
    if started is True:
        frame = info.get("control_playback_frame")
        if type(frame) is not int or not 0 < frame <= control.frames:
            raise ValueError("driven muted control requires a live control_playback_frame")
        if before is not None and before > frame:
            raise ValueError("reference delivery exceeds the control playback observation")
        cutoff = frame + route_bound(info)
        if info.get("verified_capture_controller") != info["route_calibration"]["capture_controller"]:
            raise ValueError("verified capture controller differs from route calibration")
        if (control.frames - cutoff) * 1000 < acoustic.P99_LIMIT_MS * control.rate:
            raise ValueError("muted control lacks 280 ms of captured tail after playback")
    elif "control_playback_frame" in info:
        raise ValueError("passive recording cannot declare control_playback_frame")
    elif "route_calibration" in info:
        raise ValueError("passive recording cannot declare route_calibration")
    elif "verified_capture_controller" in info:
        raise ValueError("passive recording cannot declare verified_capture_controller")
    elif "capture_geometry_verified" in info:
        raise ValueError("passive recording cannot declare capture_geometry_verified")


def bridge_started(label, key, mode):
    if mode == "driven":
        if type(label.get(key)) is not bool:
            raise ValueError(f"driven recording requires boolean {key}")
        return label[key]
    if key in label:
        raise ValueError(f"passive recording cannot declare {key}")
    return None


def reference_bound(info, label, prefix, started, frames):
    count_key, bound_key = prefix + "reference_frames", prefix + "captured_before_reference"
    if "responder_protocol" in info:
        version = info["responder_protocol"]
        if info["mode"] != "driven" or type(version) is not int or version != 3:
            raise ValueError("invalid responder_protocol")
        if started:
            count, bound = label.get(count_key), label.get(bound_key)
            if type(count) is not int or not 0 < count <= frames:
                raise ValueError(f"missing or invalid {count_key}")
            if type(bound) is not int or not count <= bound <= frames:
                raise ValueError(f"missing or invalid {bound_key}")
            return bound
    if count_key in label or bound_key in label:
        raise ValueError("reference evidence requires a started version 3 response")
    return None


def measure(directory):
    if directory is None:
        raise ValueError("no authorized synchronized physical session supplied")
    with regular(directory / "session.json").open("rb") as f:
        raw = f.read(1_048_577)
    if len(raw) > 1_048_576:
        raise ValueError("oversized session manifest")
    try:
        info = json.loads(raw, object_pairs_hook=distinct_keys)
    except RecursionError as error:
        raise ValueError("session manifest nested too deeply") from error
    if not isinstance(info, dict) or info.get("origin") != "authorized-single-clock-physical":
        raise ValueError("physical provenance declaration required")
    if not all(isinstance(info.get(key), str) and info[key].strip() for key in ("route", "gains")):
        raise ValueError("describe the single capture route and fixed gains")
    mode = info.get("mode")
    if mode not in ("passive", "driven"):
        raise ValueError("explicit passive or driven recording mode required")
    threshold = info.get("threshold_pcm")
    if type(threshold) is not int:
        raise ValueError("integer calibrated threshold required")
    rate = info.get("sample_rate_hz")
    if type(rate) is not int or not 8000 <= rate <= 192000:
        raise ValueError("acquisition sample_rate_hz in the range 8000..192000 required")
    control = checked_capture(directory / "muted-control.wav", threshold, info.get("control_pcm_sha256"))
    if control.rate != rate:
        raise ValueError("muted control rate differs from acquisition sample_rate_hz")
    check_control(control, info)

    trials = directory / "trials"
    if trials.is_symlink() or not trials.is_dir():
        raise ValueError("expected physical trials directory")
    paths = sorted(trials.iterdir())
    inventory = info.get("trials")
    if not isinstance(inventory, dict) or inventory.keys() != {p.name for p in paths}:
        raise ValueError("trial inventory must cover exactly the entire trials directory")
    seen = {control.fingerprint}
    latencies = []
    misses = {}
    by_scene = {scene: [] for scene in SCENES}
    for path in paths:
        label = inventory[path.name]
        if (path.suffix.lower() != ".wav" or not isinstance(label, dict)
                or label.get("scene") not in (*SCENES, "control")):
            raise ValueError(f"{path.name}: expected a labeled trial WAV")
        try:
            sample = checked_capture(path, threshold, label.get("pcm_sha256"))
            if sample.rate != control.rate:
                raise ValueError("capture rate differs from muted control")
            if sample.fingerprint in seen:
                raise ValueError("duplicate PCM recording")
            seen.add(sample.fingerprint)
            started = bridge_started(label, "bridge_started", mode)
            before = reference_bound(info, label, "", started, sample.frames)
            if before is not None and sample.first_output_end is not None and sample.first_output_end <= before:
                # The entire first active speaker bin was captured before the
                # responder could have queued audio. Do not hunt for a later onset.
                raise ValueError("apparent speaker onset precedes bridge enqueue")
            latency = None
            if started is False or sample.first_output_end is None:
                # The last input window must leave time for even a near-deadline onset
                # to be confirmed by sustained output. A truncated observation is not a miss.
                required = acoustic.P99_LIMIT_MS + acoustic.SUSTAINED_OUTPUT_MS
                if (sample.frames - sample.last_input_end) * 1000 < required * sample.rate:
                    raise ValueError("insufficient post-stimulus recording to establish a miss")
            if started is False:
                misses[path.name] = "no bridge started"
            elif sample.first_output_end is None:
                misses[path.name] = "no sustained speaker response"
            else:
                latency = sample.latency_ms()
        except INPUT_ERRORS as error:
            raise ValueError(f"{path.name}: {error}") from error
        latencies.append(latency)
        if label["scene"] in by_scene:
            by_scene[label["scene"]].append(latency)
    if len(paths) < acoustic.MIN_TRIALS:
        raise ValueError(f"need at least {acoustic.MIN_TRIALS} trials")
    device = ({"passed": False, "trials": len(paths), "p50_ms": None, "p99_ms": None, "misses": misses}
              if misses else acoustic.summarize(latencies))
    noisy = {scene: {"trials": len(values), "misses": values.count(None),
                     "max_ttfa_ms": None if None in values else max(values, default=None)}
             for scene, values in by_scene.items()}
    return {
        "manifest_sha256": hashlib.sha256(raw).hexdigest(),
        "muted_control_checked": True, "task_10_2": device, "noisy_scenes": noisy,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description="Verify an original physical session against the Task 10.2 TTFA gate.")
    parser.add_argument("session", type=Path, help="directory with session.json, muted-control.wav and trials/")
    try:
        evidence = measure(parser.parse_args(argv).session)
    except INPUT_ERRORS as error:
        print(f"Invalid physical session: {error}", file=sys.stderr)
        return 2
    print(json.dumps(evidence, indent=2))
    return 0 if evidence["task_10_2"]["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
