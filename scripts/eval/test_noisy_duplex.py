import contextlib
import hashlib
import io
import json
import math
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import noisy_duplex as bench


class FixtureTests(unittest.TestCase):
    def test_digest_checked_archive_reads_only_named_background_members(self):
        with tempfile.TemporaryDirectory(dir=bench.ROOT) as work, patch.object(bench, "MODELS", Path(work)):
            path = Path(work) / "downloads" / "release.tar.bz2"
            path.parent.mkdir()
            wav = Path(work) / "track.wav"
            with tarfile.open(path, "w:bz2") as tar:
                for name, level in (("elsewhere/ko", -1), ("test_wavs/en", -1),
                                    ("test_wavs/ko", 1), ("test_wavs/ja", 1), ("test_wavs/zh", 1)):
                    bench.write_pcm(wav, (level,) * 100)
                    tar.add(wav, arcname=f"release/{name}.wav")
            entry = {"url": path.name, "size_bytes": path.stat().st_size,
                     "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
            self.assertEqual(bench.speech_tracks(entry), dict.fromkeys(("ko", "ja", "zh"), (1.0,) * 100))
            with self.assertRaisesRegex(ValueError, "digest differs"):
                bench.speech_tracks(dict(entry, sha256="0" * 64))


class MixTests(unittest.TestCase):
    tracks = {"ko": (0.25, -0.75), "ja": (0.5, 1, -0.5), "zh": (-1, 0.125)}

    def test_reported_snr_uses_quantized_pcm_and_scaled_clean(self):
        clean, bed = (32_000, -24_000, 0, 0), (1, 0, -1, 1)
        out, measured = bench.mix_at_zero_db(clean, bed)
        gain = 32_767 / (32_000 + 40_000 / math.sqrt(3))
        self.assertEqual(out[0], 32_767)
        reference = [gain * x for x in clean]
        self.assertAlmostEqual(measured, 10 * math.log10(
            sum(x * x for x in reference) / sum((y - x) ** 2 for x, y in zip(reference, out))))
        self.assertLess(abs(measured), 0.01)
        for invalid in (((0,), (1,)), ((1,), (0,)), ((1, 0), (1,))):
            with self.assertRaises(ValueError):
                bench.mix_at_zero_db(*invalid)

    def test_babble_overlaps_clips_but_dialogue_takes_turns(self):
        muted = {**self.tracks, "zh": (0,)}
        self.assertNotEqual(bench.babble(100, self.tracks), bench.babble(100, muted))
        turn = bench.RATE * 3 // 2
        dialog = bench.tv_dialogue(turn + 100, self.tracks)
        without_zh = bench.tv_dialogue(turn + 100, muted)
        self.assertEqual(dialog[:turn], without_zh[:turn])
        self.assertNotEqual(dialog[turn:], without_zh[turn:])

    def test_every_mixture_is_repeatable_and_keeps_enrollment_separate(self):
        enrollment = (11,) * 8000
        query = (-2500, 3000) * 4000 + (1000,)
        other = (2200, 0, -1800) * 3000
        results = set()
        for scene in bench.CONDITIONS[1:]:
            with self.subTest(scene=scene):
                def mix(prefix, held_out):
                    return bench.scene_inputs(scene, prefix + held_out, other, self.tracks)
                combined, negative, owner_snr, other_snr = mix(enrollment, query)
                self.assertEqual(len(enrollment), len(combined) // 2)  # odd fixture length
                self.assertEqual(combined[:len(enrollment)], enrollment)
                self.assertEqual(len(negative), len(other))
                changed_prefix = mix((8000,) * len(enrollment), query)
                self.assertEqual(changed_prefix[0][len(enrollment):], combined[len(enrollment):])
                self.assertEqual(changed_prefix[1:], (negative, owner_snr, other_snr))
                self.assertEqual(mix(enrollment, query[::-1])[1], negative)
                self.assertLess(max(abs(owner_snr), abs(other_snr)), 0.01)
                results.add(negative)
        self.assertEqual(len(results), 4)


class ReportingTests(unittest.TestCase):
    rejected = "recorded non-owner: 3 candidate embeddings rejected\n"
    verified = "recorded owner: verified after 500 ms of fixture audio\n"
    ok = "test result: ok. 1 passed; 0 failed; 0 ignored;"

    def test_failed_owner_postcheck_requires_the_exact_native_failure(self):
        marker = "recorded owner: checking decision, mute and audit\n"
        failed = f"test {bench.TEST} ... FAILED\n"
        for output, status in [(marker, "unavailable"), (failed, "unavailable"), (failed + marker, "failed")]:
            row = bench.observations(self.rejected + output, 101)
            self.assertEqual(bench.condition_status(row), status)

    def test_agreement_requires_both_pins_and_cannot_inherit_a_single_model_pass(self):
        with patch.object(bench, "verified_paths", return_value={
                "silero-vad-v5": bench.MODELS / "vad/silero_vad.onnx",
                **{k: bench.MODELS / v for k, v in bench.SPEAKER_MODELS.items()}}) as pins:
            bench.verify_native_models({}, bench.AGREEMENT)
        self.assertEqual(pins.call_args.args[1], ("silero-vad-v5", bench.BASELINE_MODEL, "camplusplus-zh-en-advanced"))
        run = subprocess.CompletedProcess([], 0, self.ok, f"recorded speaker model: {bench.BASELINE_MODEL}\n"
                                          + self.rejected + self.verified)
        with patch.object(bench.subprocess, "run", return_value=run) as invoke, contextlib.redirect_stderr(io.StringIO()):
            self.assertIn("unavailable", bench.evaluate(bench.CLEAN, "a", "b", bench.AGREEMENT))
        env = invoke.call_args.kwargs["env"]
        self.assertEqual((env["EON_SPEAKER_MODEL"], env["EON_SPEAKER_CONFIRMATION"]),
                         (bench.BASELINE_MODEL, "camplusplus-zh-en-advanced"))
        self.assertEqual(bench.policy_env(bench.BASELINE_MODEL)["EON_SPEAKER_CONFIRMATION"], "none")

    def test_rejection_survives_owner_failure_but_missing_observations_stay_null(self):
        result = bench.observations(self.rejected, 101)
        self.assertEqual(result["non_owner_embeddings_rejected"], 3)
        self.assertEqual(result["non_owner_interruptions"], 0)
        self.assertIsNone(result["owner_decision_audio_ms"])
        self.assertIsNone(result["ttfa_ms"])
        self.assertFalse(result["duplex_check_passed"])
        self.assertIsNone(bench.observations("", 101)["non_owner_interruptions"])
        for incomplete in ("test result: ok. 0 passed;", self.ok, self.verified + self.ok,
                           self.rejected + self.ok, self.rejected + self.verified):
            self.assertFalse(bench.observations(incomplete, 0)["duplex_check_passed"])
        self.assertTrue(bench.observations(self.rejected + self.verified + self.ok, 0)["duplex_check_passed"])

    def test_miss_false_interrupt_and_no_native_inference_are_different(self):
        missed = bench.observations(self.rejected + "recorded owner: missed; 0 candidate embeddings rejected", 101)
        self.assertIs(missed["owner_missed"], True)
        self.assertEqual(missed["missed_owner_embeddings_rejected"], 0)
        self.assertEqual(missed["non_owner_interruptions"], 0)
        false = bench.observations("recorded non-owner: interrupted playback", 101)
        self.assertEqual(false["non_owner_interruptions"], 1)
        self.assertIsNone(false["owner_missed"])
        self.assertIsNone(bench.observations("build failed", 101)["owner_missed"])
        empty_negative = self.rejected.replace("3 candidate", "0 candidate") + self.verified + self.ok
        self.assertFalse(bench.observations(empty_negative, 0)["duplex_check_passed"])
        contradictory = self.rejected + self.verified + "recorded owner: missed; 1 candidate embeddings rejected\n"
        result = bench.observations(contradictory + self.ok, 0)
        self.assertFalse(result["duplex_check_passed"])
        self.assertIsNone(result["owner_decision_audio_ms"])

    def test_require_confirmation_from_selected_encoder_before_reporting_decisions(self):
        model = "camplusplus-zh-en-advanced"
        for markers, expected in (([], False), ([bench.BASELINE_MODEL], False),
                                  ([model, model], False), ([model], True)):
            stderr = "".join(f"recorded speaker model: {m}\n" for m in markers) + self.rejected + self.verified
            completed = subprocess.CompletedProcess([], 0, "test recorded ... ok\n" + self.ok, stderr)
            with (patch.object(bench.subprocess, "run", return_value=completed) as run,
                  contextlib.redirect_stderr(io.StringIO())):
                row = bench.evaluate(bench.CLEAN, "owner.wav", "other.wav", model)
            self.assertEqual(run.call_args.kwargs["env"]["EON_SPEAKER_MODEL"], model)
            self.assertEqual(row["duplex_check_passed"], expected)
            if not expected:
                self.assertIn("unavailable", row)
                self.assertIsNone(row["non_owner_interruptions"])

    def test_missing_speech_and_rumble_failure_do_not_skip_traffic(self):
        with tempfile.TemporaryDirectory(dir=bench.ROOT) as work:
            wav = Path(work) / "input.wav"
            bench.write_pcm(wav, (2500, -3000) * 5000)
            paths = dict.fromkeys(("voice-test-sensevoice-en", "voice-test-zipformer-0"), wav)
            def simulated(label, owner, other, model):
                return bench.observations(self.rejected, 101) if label == bench.RUMBLE else (
                    bench.observations(self.rejected + self.verified + self.ok, 0))
            with (patch.object(bench, "ROOT", Path(work)), patch.object(bench, "pinned_inputs", return_value=(paths, {})),
                  patch.object(bench, "speech_tracks", side_effect=FileNotFoundError("missing speech archive")),
                  patch.object(bench, "evaluate", side_effect=simulated) as evaluate,
                  contextlib.redirect_stdout(io.StringIO()) as output):
                self.assertEqual(bench.main([]), 2)
            rows = [json.loads(line) for line in output.getvalue().splitlines()]
            self.assertEqual([row["condition"] for row in rows], list(bench.CONDITIONS))
            self.assertTrue(all(row["speaker_model"] == bench.BASELINE_MODEL for row in rows))
            self.assertEqual([call.args[0] for call in evaluate.call_args_list], [bench.CLEAN, bench.RUMBLE, bench.STREET])
            self.assertEqual(rows[1]["non_owner_interruptions"], 0)
            self.assertTrue(rows[3]["duplex_check_passed"])
            for row in (rows[2], rows[4]):
                self.assertEqual(row["unavailable"], "missing speech archive")
                self.assertIsNone(row["non_owner_interruptions"])
                self.assertIsNone(row["native_exit_code"])


if __name__ == "__main__":
    unittest.main()
