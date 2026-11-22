import contextlib
import hashlib
import io
import json
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import software_turns as turns


def archive(clips):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w:gz") as tar:
        for name, frames in clips:
            info = (16_000 << 44) | (15 << 36) | frames
            payload = b"fLaC\x80\x00\x00\x22" + bytes(10) + info.to_bytes(8, "big") + bytes(16) + b"x"
            entry = tarfile.TarInfo("LibriSpeech/dev-clean/1/2/" + name + ".flac")
            entry.size = len(payload)
            tar.addfile(entry, io.BytesIO(payload))
    stream.seek(0)
    return stream


class SoftwareCorpusTests(unittest.TestCase):
    def test_recorded_pair_pins_full_sources_and_crosses_scenes_without_replacing_the_corpus(self):
        tracks = {"ko": (0.25, -0.75), "ja": (0.5, 1, -0.5), "zh": (-1, 0.125)}
        first, second = turns.duplex.RECORDINGS
        with (tempfile.NamedTemporaryFile(dir=turns.duplex.ROOT) as a,
              tempfile.NamedTemporaryFile(dir=turns.duplex.ROOT) as b,
              patch.object(turns.duplex, "speech_tracks", return_value=tracks)):
            models, sources = {}, {}
            for key, path, length in ((first, Path(a.name), 1040), (second, Path(b.name), 2080)):
                pcm = [900, -1500] * (length // 2)
                turns.duplex.write_pcm(path, pcm)
                models[key] = {"dest": path, "archive": {}, "size_bytes": path.stat().st_size,
                               "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                sources[key] = pcm
            batch = io.BytesIO()
            inventory = turns.write_batch(batch, models, recorded_pair=True)
            expected = [("silent-control", "silence"), *((key, scene) for key in (first, second) for scene in turns.SCENES)]
            self.assertEqual([(t["id"], t["scene"]) for t in inventory], expected)
            for t in inventory:
                n, = struct.unpack("<I", batch.read(4))
                self.assertEqual(t["frames"], n)
                stereo = batch.read(4 * n)
                if t["scene"] == "silence":
                    self.assertEqual(stereo, bytes(4 * turns.TAIL))
                    continue
                dry, mix = zip(*struct.iter_unpack("<hh", stereo))
                pcm = sources[t["id"]]
                self.assertEqual(list(dry), [0] * turns.LEAD + pcm + [0] * turns.TAIL)
                self.assertEqual(t["dry_end"], turns.LEAD + len(pcm))
                if t["scene"] == "clean":
                    self.assertEqual(mix, dry)
                else:
                    self.assertLessEqual(abs(t["snr_db"]), 0.01)
                    self.assertTrue(any(mix[:turns.LEAD]) and any(mix[-turns.TAIL:]))
            self.assertEqual(batch.read(), bytes(4))  # zero frame terminator, no more takes
            for fault in ({"size_bytes": 0}, {"sha256": "0" * 64}):
                with self.assertRaisesRegex(ValueError, "differs from model lock"):
                    list(turns.recorded_sources({**models, second: {**models[second], **fault}}))
            with self.assertRaisesRegex(ValueError, "duplicate source PCM"):
                turns.write_batch(io.BytesIO(), {**models, second: models[first]}, recorded_pair=True)
            with self.assertRaisesRegex(KeyError, turns.CORPUS):
                turns.write_batch(io.BytesIO(), models)

    def test_rank_whole_eligible_utterances_not_first_archive_members(self):
        items = [(f"1-2-{i:04d}", 16_000 + i) for i in range(8)]
        expected = sorted(items, key=lambda pair: hashlib.sha256(pair[0].encode()).digest())[:3]
        items.append(("1-2-9999", 16_000 * 13))
        for order in (items, list(reversed(items))):
            self.assertEqual([(key, frames) for key, frames, _ in turns.select_clips(archive(order), 3)], expected)
        with self.assertRaisesRegex(ValueError, "duplicate"):
            turns.select_clips(archive(items + items[:1]), 3)
        with self.assertRaisesRegex(ValueError, "distinct"):
            turns.select_clips(archive(items), 9)

    def test_dry_reference_is_never_the_noisy_inference_channel(self):
        raw = struct.pack("<100h", *[900, -900] * 50)
        clean, snr = turns.channels(raw, "clean", {})
        self.assertIsNone(snr)
        noisy, snr = turns.channels(raw, "traffic-0db", {})
        self.assertLessEqual(abs(snr), 0.01)
        plain = list(struct.iter_unpack("<hh", clean))
        mixed = list(struct.iter_unpack("<hh", noisy))
        self.assertTrue(all(a == b for a, b in plain))
        self.assertEqual([a for a, _ in plain], [a for a, _ in mixed])
        self.assertTrue(all(a == 0 for a, _ in mixed[-turns.TAIL:]))
        self.assertTrue(any(b != 0 for _, b in mixed[-turns.RATE:]))

    def test_lengths_count_stereo_frames_not_bytes_or_samples(self):
        stream = io.BytesIO()
        payload = struct.pack("<hhhh", 1, -2, 3, -4)
        self.assertEqual(turns.write_take(stream, payload), 2)
        self.assertEqual(stream.getvalue(), struct.pack("<I", 2) + payload)

    def test_dry_activity_uses_aligned_1ms_bins_including_trailing_padding(self):
        self.assertEqual(turns.last_active(struct.pack("<16h", *[100] * 16)), turns.LEAD + 16)
        self.assertEqual(turns.last_active(struct.pack("<17h", *([100] * 16 + [1]))), turns.LEAD + 16)
        self.assertEqual(turns.last_active(struct.pack("<17h", *([0] * 16 + [400]))), turns.LEAD + 32)
        with self.assertRaisesRegex(ValueError, "no foreground"):
            turns.last_active(bytes(800))


class SoftwareScoreTests(unittest.TestCase):
    def setUp(self):
        self.inventory = [{"id": "silent-control", "scene": "silence", "frames": 16000, "dry_end": None}]
        self.rows = [dict(take=1, status="no_reply", endpoint_sample=None, last_active_end_sample=None, latency_us=None)]
        for i in range(turns.COUNT):
            self.inventory.append({"id": str(i), "scene": turns.SCENES[i % 4], "frames": 16000, "dry_end": 1040})
            self.rows.append(dict(take=i + 2, status="ok", endpoint_sample=1536, last_active_end_sample=1040, latency_us=80_000))

    def score(self, reason=None):
        return turns.evaluate(self.inventory, b"".join(json.dumps(r).encode() + b"\n" for r in self.rows), reason)

    def score_recorded_pair(self, rows, tail=b"", reason=None):
        inventory = [{**take, "id": key} for take, (key, _) in zip(self.inventory, turns.PAIR_TAKES)]
        data = b"".join(json.dumps(r).encode() + b"\n" for r in rows) + tail
        return turns.evaluate(inventory, data, reason, recorded_pair=True)

    def test_pair_reports_native_bin_bound_and_remaining_wall_time_not_percentiles(self):
        rows = self.rows[:9]
        rows[1]["latency_us"] = 310_000  # no clean per-take ceiling in the full gate
        rows[2]["latency_us"] = 199_999
        report = self.score_recorded_pair(rows)
        self.assertEqual((report["scope"], report["acceptance"], report["status"]),
                         ("recorded-pair-diagnostic", False, "passed"))
        self.assertNotIn("p50_us", report)
        self.assertNotIn("p99_us", report)
        self.assertNotIn("render_remainder_us", report["takes"][0])
        noisy = report["takes"][2]
        self.assertEqual((noisy["endpoint_sample_clock_us"], noisy["render_remainder_us"]), (32_000, 167_999))
        rows[2]["latency_us"] = 200_000
        report = self.score_recorded_pair(rows)
        self.assertEqual((report["status"], report["takes"][2]["status"], report["takes"][2]["check"]),
                         ("failed", "ok", "failed"))

    def test_pair_only_attributes_complete_unique_dry_checked_rows(self):
        rows = self.rows[:8]  # final noisy take was never flushed
        rows[2]["last_active_end_sample"] -= 16
        report = self.score_recorded_pair(rows + [rows[3]], b'{"take":9,"status":', "responder exited 1")
        self.assertEqual(report["status"], "unavailable")
        for i in (2, 3, 8):
            self.assertEqual(report["takes"][i]["status"], "unavailable")
            self.assertIsNone(report["takes"][i]["latency_us"])
            self.assertNotIn("endpoint_sample_clock_us", report["takes"][i])
        self.assertEqual(report["takes"][1]["latency_us"], 80_000)
        report = self.score_recorded_pair(self.rows[:8], json.dumps(self.rows[8]).encode())  # missing native flush
        self.assertEqual(report["takes"][8]["status"], "unavailable")
        rows[1].update(status="early_endpoint", endpoint_sample=512, latency_us=None)
        report = self.score_recorded_pair(rows, reason="responder deadline")
        self.assertEqual((report["status"], report["takes"][1]["check"]), ("failed", "failed"))
        self.assertNotIn("render_remainder_us", report["takes"][1])

    def test_pair_missing_prerequisites_have_no_timing_evidence(self):
        with patch.object(turns.duplex, "locked_models", side_effect=FileNotFoundError("missing lock")):
            report = turns.measure(recorded_pair=True)
        self.assertEqual((report["status"], report["acceptance"], len(report["takes"])), ("unavailable", False, 9))
        self.assertTrue(all(r["status"] == "unavailable" and r["latency_us"] is None for r in report["takes"]))
        self.assertIn("missing lock", report["reason"])
        self.assertNotIn("p99_us", report)

    def test_pair_cli_is_opt_in(self):
        with (patch.object(turns, "measure", return_value={"status": "failed"}) as measure,
              contextlib.redirect_stdout(io.StringIO())):
            for args in ([], ["--recorded-pair"]):
                self.assertEqual(turns.main(args), 1)
                measure.assert_called_with(recorded_pair=bool(args))

    def test_silence_is_not_timed_and_ceilings_are_strict(self):
        self.assertEqual(self.score()["p99_us"], 80_000)
        self.assertEqual(self.score()["status"], "passed")
        self.rows[1]["latency_us"] = 280_000  # clean, 100th ordered observation
        self.assertEqual(self.score()["status"], "passed")
        self.rows[5]["latency_us"] = 280_000
        self.assertEqual(self.score()["status"], "failed")
        self.assertEqual(self.score()["p99_us"], 280_000)
        self.rows[5]["latency_us"] = 80_000
        self.rows[2]["latency_us"] = 200_000  # even one noisy turn at the limit fails
        self.assertEqual(self.score()["status"], "failed")
        for row in self.rows[1:]:
            row["latency_us"] = 170_000
        self.assertEqual(self.score()["status"], "failed")

    def test_minimum_includes_first_observable_sink_bin(self):
        self.inventory[2]["dry_end"] = 928
        self.rows[2].update(last_active_end_sample=928, endpoint_sample=4096, latency_us=199_999)
        self.assertEqual(self.score()["status"], "unavailable")
        self.rows[2]["latency_us"] = 200_000
        self.assertEqual(self.score()["status"], "failed")

    def test_malformed_row_cannot_hide_an_independently_attributed_miss(self):
        self.rows[1] = {"take": 2, "bogus": True}
        self.rows[2].update(status="early_endpoint", endpoint_sample=512, latency_us=None)
        report = self.score()
        self.assertEqual(report["status"], "failed")
        self.assertEqual(report["takes"][1]["status"], "unavailable")
        self.assertEqual(report["takes"][2]["status"], "early_endpoint")
        self.rows.append(self.rows[2])
        self.assertEqual(self.score()["status"], "unavailable")  # duplicate ordinal cannot be credited

    def test_only_confirmed_silent_warmup_has_a_measured_startup_failure(self):
        self.rows = [{"startup_failure": "silent_bridge"}]
        self.assertEqual(self.score("responder exited 1")["status"], "failed")
        self.assertEqual(self.score()["status"], "unavailable")
        self.rows = []
        self.assertEqual(self.score("responder exited 1")["status"], "unavailable")

    def test_early_endpoint_or_missing_sustained_render_is_not_a_fast_reply(self):
        for status, end in (("early_endpoint", 512), ("no_reply", None), ("no_render_onset", 1536)):
            self.rows[1].update(status=status, endpoint_sample=end, latency_us=None)
            result = self.score()
            self.assertEqual(result["status"], "failed")
            self.assertIsNone(result["p99_us"])
            self.assertEqual(result["scenes"]["clean"]["misses"], 1)
        self.rows.pop()
        self.assertEqual(self.score("responder exited 1")["status"], "failed")

    def test_truncated_crashed_or_inconsistent_batches_cannot_pass(self):
        self.assertEqual(self.score("responder exited 1")["status"], "unavailable")
        self.rows.pop()
        self.assertEqual(self.score()["status"], "unavailable")
        for field, value in (("last_active_end_sample", 900), ("endpoint_sample", False),
                             ("endpoint_sample", 1408), ("latency_us", 1)):
            original = self.rows[1][field]
            self.rows[1][field] = value
            self.assertIn("take 2:", self.score()["reason"])
            self.rows[1][field] = original
        self.rows[0].update(status="no_input_activity", endpoint_sample=512)
        self.assertEqual(self.score()["status"], "failed")

    def test_missing_control_or_incomplete_scene_inventory_cannot_pass(self):
        self.inventory[-1]["scene"] = "clean"
        with self.assertRaisesRegex(ValueError, "scene inventory"):
            self.score()
        self.inventory.pop(0)
        with self.assertRaisesRegex(ValueError, "silent control"):
            self.score()

    def test_help_and_unknown_arguments_never_start_the_runner(self):
        with (patch.object(turns, "measure", side_effect=AssertionError),
              contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO())):
            for arg, code in (("--help", 0), ("--invalid-option", 2)):
                with self.assertRaises(SystemExit) as raised:
                    turns.main([arg])
                self.assertEqual(raised.exception.code, code)

    def test_native_executable_is_the_unique_release_artifact_reported_by_cargo(self):
        artifact = {"reason": "compiler-artifact", "target": {"name": "eon-rig-turn"},
                    "executable": "/custom/target/release/eon-rig-turn"}
        with patch.object(turns.subprocess, "run") as build:
            for count in (0, 1, 2):
                build.return_value.stdout = ((json.dumps(artifact) + "\n") * count).encode()
                if count == 1:
                    self.assertEqual(str(turns.build_responder()), artifact["executable"])
                else:
                    with self.assertRaisesRegex(ValueError, "release responder"):
                        turns.build_responder()
            command = build.call_args.args[0]
            self.assertIn("--release", command)
            self.assertIn("--offline", command)
            self.assertIn("--locked", command)
