import contextlib
import io
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import speaker_handoffs as handoffs


class SpeakerHandoffTests(unittest.TestCase):
    def test_cli_keeps_measured_failure_even_if_corpus_is_missing(self):
        def partial(status):
            yield {"pair": "locked", "status": status}
            raise FileNotFoundError("no corpus")

        for status, code in [("failed", 1), ("passed", 2)]:
            with (patch.object(handoffs, "run_pairs", return_value=partial(status)),
                  contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO())):
                self.assertEqual(handoffs.main([]), code)

    def test_agreement_handoff_is_not_evidence_for_the_shipping_policy(self):
        policy = handoffs.duplex.AGREEMENT
        result = subprocess.CompletedProcess([], 0, f"test {handoffs.TEST} ... ok\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored;", f"handoff speaker model: {policy}\n")
        self.assertEqual(handoffs.observation(result, policy)[0], "passed")
        self.assertEqual(handoffs.observation(result)[0], "unavailable")

    def test_requires_named_test_and_confirmed_shipping_encoder(self):
        marker = "handoff speaker model: wespeaker-resnet34\n"
        test = f"test {handoffs.TEST} ... "
        ok = "test result: ok. 1 passed; 0 failed; 0 ignored; 6 filtered out"
        wrong = "handoff failed: playback interrupted 280 ms into the stranger after 270 ms of owner"
        for code, out, err, expected in (
                (0, test + "ok\n" + ok, marker, "passed"),
                (0, test + "ok\n" + ok, marker.replace("34", "221"), "unavailable"),
                (0, ok, marker, "unavailable"), (0, test + "ignored", marker, "unavailable"),
                (101, test + "FAILED", marker + wrong, "failed"),
                (101, test + "FAILED", marker + "handoff failed: returning clean owner never interrupted", "failed"),
                (101, test + "FAILED", marker + "handoff failed: audible output after owner mute", "failed"),
                (101, test + "FAILED", marker + "handoff failed: missing or incorrect owner audit", "failed"),
                (101, test + "FAILED", marker + "handoff unexercised: no switch exercised a rejected owner probe",
                 "unavailable"), (101, test + "FAILED", marker + "input missing", "unavailable")):
            with self.subTest(code=code, out=out, err=err):
                self.assertEqual(handoffs.observation(subprocess.CompletedProcess([], code, out, err))[0], expected)

    def test_missing_stock_recording_does_not_hide_an_independent_failure(self):
        clips = {n: [(f"{n}-1", b"\x01\x00"), (f"{n}-2", b"\x02\x00")] for n in (84, 1462)}
        with (patch.object(handoffs.duplex, "verify_native_models"),
              patch.object(handoffs.duplex, "verified_paths", side_effect=FileNotFoundError("stock wav")),
              patch.object(handoffs, "PAIRS", ((84, 1462),)), patch.object(handoffs, "reader_clips", return_value=clips),
              patch.object(handoffs, "evaluate", return_value={"pair": "84:1462", "status": "failed"})):
            locked, reader = handoffs.run_pairs()
        self.assertEqual((locked["status"], reader["status"]), ("unavailable", "failed"))

    def test_corpus_must_match_the_lock_and_uses_first_two_long_enough_ids(self):
        import hashlib
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "test.tar.gz"
            with tarfile.open(path, "w:gz") as tar:
                for index, data in ((5, b"E"), (3, b"C"), (1, b"short"), (2, b"B"), (4, b"D")):
                    entry = tarfile.TarInfo(f"LibriSpeech/dev-clean/84/12/84-12-000{index}.flac")
                    entry.size = len(data)
                    tar.addfile(entry, io.BytesIO(data))
            lock = {"voice-test-librispeech-dev-clean": {
                "dest": path.name, "size_bytes": path.stat().st_size, "sha256": "0" * 64}}
            with (patch.object(handoffs.duplex, "ROOT", path.parent),
                  patch.object(handoffs.duplex, "locked_models", return_value=lock),
                  patch.object(handoffs, "GROUPS", ((84,),)),
                  patch.object(handoffs, "crop", side_effect=lambda b: None if b == b"short" else b) as decode):
                with self.assertRaisesRegex(ValueError, "digest"):
                    handoffs.reader_clips()
                decode.assert_not_called()
                lock["voice-test-librispeech-dev-clean"]["sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
                self.assertEqual(handoffs.reader_clips()[84], [("84-12-0002", b"B"), ("84-12-0003", b"C")])
