import argparse
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import MagicMock, patch
import wave

import record_physical_session as rig
import stereo_pcm


class RigTests(unittest.TestCase):
    """Recorder protocol only. No device or physical timing result is exercised here."""

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.dest = self.root / "takes"
        self.args = argparse.Namespace(directory=self.dest, route="unit test", gains="fixed",
                                       threshold=500, device="hw:2,0", rate=16000, seconds=2, noisy=True,
                                       responder=None, speaker=None, models=None, route_calibration=None)
        self.console = self.enterContext(contextlib.redirect_stdout(io.StringIO()))
        self.device = self.enterContext(patch.object(rig, "endpoint"))

    def responder(self, body):
        self.args.speaker = "alsa:hw:CARD=Test,DEV=1"
        self.args.models = self.root
        self.args.route_calibration = self.root / "calibration.json"
        self.args.route_calibration.write_text(json.dumps({
            "reference": "unit-test fixture only", "capture_device": self.args.device, "speaker": self.args.speaker,
            "capture_controller": "/dev/snd/by-id/usb-Rig_SN01-00",
            "capture_period_frames": 80, "capture_buffer_frames": 320,
            "route": self.args.route, "gains": self.args.gains, "sample_rate_hz": self.args.rate, "upper_bound_frames": 160,
        }))
        self.args.responder = self.root / "test-worker"
        self.args.responder.write_text("#!/usr/bin/env python3\nimport os, sys, struct\n"
                                       "reader = sys.stdin.buffer\n"
                                       "def header(): return struct.unpack('<I', reader.read(4))[0]\n" + body)
        self.args.responder.chmod(0o700)

    def collect(self, directory, turns, take):
        info = rig.configuration(self.args)
        info["verified_capture_controller"] = info["route_calibration"]["capture_controller"]  # test endpoint
        info["capture_geometry_verified"] = True
        rig.collect(self.args, directory, turns, take, info)

    def test_driven_acquire_passes_calibrated_controller_to_endpoint(self):
        self.responder("")
        with (patch.object(rig.sys.stdin, "isatty", return_value=True), patch("builtins.input", return_value="RECORD"),
              patch.object(rig, "collect") as collect):
            rig.acquire(self.args)
        identity = "/dev/snd/by-id/usb-Rig_SN01-00"
        self.device.assert_called_once_with("hw:2,0", 16000, json.loads(self.args.route_calibration.read_text()))
        self.assertEqual(collect.call_args.args[-1]["verified_capture_controller"], identity)
        self.assertIs(collect.call_args.args[-1]["capture_geometry_verified"], True)

    def test_missing_or_different_route_calibration_stops_before_capture(self):
        self.responder("")
        path = self.args.route_calibration
        with patch.object(rig.sys.stdin, "isatty", return_value=True), patch("builtins.input") as prompt:
            for value, reason in ((None, "--route-calibration"), (self.root / "missing", "regular file"),
                                  (path, "acquisition speaker")):
                self.args.route_calibration = value
                self.args.speaker = "alsa:hw:CARD=Other,DEV=0"
                with self.subTest(value=value), self.assertRaisesRegex(ValueError, reason):
                    rig.acquire(self.args)
            prompt.assert_not_called()
            self.device.assert_not_called()

    def test_new_driven_acquisition_requires_both_calibrated_buffer_sizes(self):
        self.responder("")
        path = self.args.route_calibration
        calibration = json.loads(path.read_text())
        with patch.object(rig.sys.stdin, "isatty", return_value=True), patch("builtins.input") as prompt:
            for period, buffer in [(None, None), (True, 320), (80, None), (0, 320), (320, 80), (80, 480001)]:
                path.write_text(json.dumps({k: v for k, v in dict(
                    calibration, capture_period_frames=period, capture_buffer_frames=buffer).items() if v is not None}))
                with self.subTest(period=period, buffer=buffer), self.assertRaisesRegex(ValueError, "capture_period"):
                    rig.acquire(self.args)
            prompt.assert_not_called()
            self.device.assert_not_called()

    def test_device_selection_and_consent_before_capture(self):
        with (patch.object(rig.sys.stdin, "isatty", return_value=False) as tty,
              patch("builtins.input", return_value="no") as operator, patch.object(rig, "record") as recorder):
            with self.assertRaisesRegex(ValueError, "terminal"):
                rig.acquire(self.args)
            tty.return_value = True
            for device in ("default", "pulse", "hw:2,0 extra", "null"):
                self.args.device = device
                with self.assertRaisesRegex(ValueError, "hardware capture"):
                    rig.acquire(self.args)
            operator.assert_not_called()
            self.args.device = "hw:2,0"
            with self.assertRaisesRegex(ValueError, "not authorized"):
                rig.acquire(self.args)
            self.assertFalse(self.dest.exists())
            recorder.assert_not_called()
            self.device.assert_not_called()

    def test_responder_startup_failure_never_starts_capture(self):
        take = MagicMock()
        for ready in (b"!", b"R", b"2"):  # older workers lack the pre-enqueue count
            self.responder(f"header()\nos.write(1, {ready!r})\n")
            with (self.subTest(ready=ready), self.assertRaisesRegex(ValueError, "responder status"),
                  rig.responding_take(self.args, take) as capture):
                capture(1, lambda _: self.fail("unexpected PCM"))
        take.assert_not_called()

    def test_preserve_every_take_and_never_overwrite_a_session(self):
        def fake(path, *args):
            path.touch(exist_ok=False)
            return hashlib.sha256(path.name.encode()).hexdigest(), None

        with (patch.object(rig.sys.stdin, "isatty", return_value=True),
              patch("builtins.input", side_effect=["RECORD", "MUTED", *(f"TAKE {i}" for i in range(1, 101)), "RECORD"]),
              patch.object(rig, "record", side_effect=fake) as recorder,
              patch.object(rig.session, "checked_capture"), patch.object(rig.session, "check_control"),
              patch.object(rig.session, "measure", side_effect=AssertionError("do not select by result"))):
            rig.acquire(self.args)
            manifest = json.loads((self.dest / "session.json").read_bytes())
            self.assertEqual(manifest["mode"], "passive")
            self.assertEqual(manifest["sample_rate_hz"], 16000)
            self.assertNotIn("control_bridge_started", manifest)
            self.assertNotIn("control_playback_frame", manifest)
            self.assertNotIn("route_calibration", manifest)
            self.assertNotIn("capture_geometry_verified", manifest)
            expected = ["control", *rig.session.SCENES]
            self.assertEqual(manifest["trials"], {
                name: {"pcm_sha256": hashlib.sha256(name.encode()).hexdigest(), "scene": expected[i // 25]}
                for i, name in enumerate(f"turn-{n:03d}.wav" for n in range(1, 101))
            })
            self.assertEqual(recorder.call_count, 101)
            self.device.assert_called_once_with("hw:2,0", 16000, None)
            self.assertEqual({call.args[1] for call in recorder.call_args_list},
                             {self.device.return_value.__enter__.return_value})
            self.assertEqual(self.dest.stat().st_mode & 0o777, 0o700)
            with self.assertRaises(FileExistsError):
                rig.acquire(self.args)
            self.assertEqual(recorder.call_count, 101)

    def test_bad_control_and_cancel_do_not_publish_or_retry(self):
        with (patch.object(rig.sys.stdin, "isatty", return_value=True),
              patch("builtins.input", side_effect=["RECORD", "MUTED"]),
              patch.object(rig, "record", return_value=("control", None)) as recorder,
              patch.object(rig.session, "checked_capture", side_effect=ValueError("invalid control"))):
            with self.assertRaisesRegex(ValueError, "invalid control"):
                rig.acquire(self.args)
            recorder.assert_called_once()
            self.assertFalse((self.dest / "session.json").exists())
        self.args.directory = self.root / "second"
        with (patch.object(rig.sys.stdin, "isatty", return_value=True),
              patch("builtins.input", side_effect=["RECORD", "MUTED", "TAKE 1", KeyboardInterrupt()]),
              patch.object(rig, "record", side_effect=[("muted", None), ("first", None)]) as recorder,
              patch.object(rig.session, "checked_capture"),
              patch.object(rig.session, "check_control")):
            with self.assertRaises(KeyboardInterrupt):
                rig.acquire(self.args)
            self.assertEqual(recorder.call_count, 2)  # control and the first turn
            self.assertFalse((self.args.directory / "session.json").exists())

    def test_capture_stimulus_duplicate_and_turn_ack_are_required(self):
        for label, inputs, hashes, analyzer, reason, count in (
            ("queued-return", [""], ["first"], [None], "turn cancelled", 1),
            ("wrong-turn", ["TAKE 2"], ["first"], [None], "turn cancelled", 1),
            ("duplicate", ["TAKE 1"], ["first"] * 2, [None, None], "duplicate PCM", 2),
            ("no-input", ["TAKE 1"], ["first", "second"], [None, ValueError("no input")], "no input", 2),
        ):
            self.args.directory = self.root / label
            with (self.subTest(label=label), patch.object(rig.sys.stdin, "isatty", return_value=True),
                  patch("builtins.input", side_effect=["RECORD", "MUTED", *inputs]),
                  patch.object(rig, "record", side_effect=[(h, None) for h in hashes]) as recorder,
                  patch.object(rig.session, "checked_capture", side_effect=analyzer),
                  patch.object(rig.session, "check_control"), self.assertRaisesRegex(ValueError, reason)):
                rig.acquire(self.args)
            self.assertEqual(recorder.call_count, count)
            self.assertFalse((self.args.directory / "session.json").exists())

    def test_native_recorder_frame_count_and_digest(self):
        rate = 16000
        pcm = b"\x01\x00\x02\x00" * rate

        path = self.root / "capture.wav"
        write = lambda seconds, sink: sink(pcm)
        self.assertEqual(rig.record(path, write, rate, 1, 500), (hashlib.sha256(pcm).hexdigest(), None))
        self.assertEqual(os.stat(path).st_mode & 0o777, 0o600)
        with self.assertRaisesRegex(ValueError, "clock and frame count"):
            rig.record(self.root / "mismatch.wav", write, rate, 2, 500)

    def test_each_cue_requires_50ms_of_original_quiet_on_both_channels(self):
        rate = 44100
        for ch in (None, 0, 1):
            with self.subTest(channel=ch):
                self.console.truncate(0)
                self.console.seek(0)
                pcm = bytearray(4 * rate)
                if ch is not None:
                    for n in range(44):
                        struct.pack_into("<h", pcm, 4 * n + 2 * ch, 501)

                def take(seconds, save):
                    save(pcm[:4 * (rate // 20 - 1)])
                    self.assertNotIn("Apply stimulus", self.console.getvalue())
                    save(pcm[4 * (rate // 20 - 1):4 * (rate // 20)])
                    self.assertEqual("Apply stimulus" in self.console.getvalue(), ch is None)
                    save(pcm[4 * (rate // 20):])

                path = self.root / f"{ch}.wav"
                if ch is None:
                    rig.record(path, take, rate, 1, 500)
                    self.assertEqual(self.console.getvalue().count("Apply stimulus"), 1)
                else:
                    with self.assertRaisesRegex(ValueError, "quiet lead-in"):
                        rig.record(path, take, rate, 1, 500)
                with wave.open(str(path)) as wav:
                    self.assertEqual(wav.readframes(rate), pcm)

    def test_one_worker_exact_reference_after_short_writes_and_eagain_on_successive_takes(self):
        self.args.rate = 16000
        self.responder("for left, result in [(1,b'PB'), (2,b'N')]:\n"
                       "    n = header()\n    os.write(1,b'3')\n"
                       "    assert reader.read(n*2) == bytes([left,0])*n\n    os.write(1,result)\n"
                       "    if result == b'PB':\n"
                       "        for byte in struct.pack('<I', n): os.write(1,bytes([byte]))\n"
                       "assert header() == 0\nos.write(1,b'D')\n")
        real_write = os.write
        attempts = 0

        def fragment(fd, data):
            nonlocal attempts
            attempts += 1
            if attempts % 3 == 2:
                raise BlockingIOError()
            return real_write(fd, data[:2 if len(data) == 4 else 511])

        left = 0
        def take(seconds, save):
            nonlocal left
            left += 1
            for _ in range(16):
                save(bytes([left, 0, 0xab, 0xcd]) * 1000, lambda: 0)

        with patch.object(rig.os, "write", side_effect=fragment), rig.responding_take(self.args, take) as capture:
            for i, expected in ((1, True), (2, False)):
                saved = bytearray()
                end = 16000 if expected else None
                self.assertEqual(capture(1, saved.extend), rig.BridgeOutcome(expected, None, end, end))
                self.assertEqual(saved, bytes([i, 0, 0xab, 0xcd]) * 16000)
        self.assertGreater(attempts, 100)
        self.args.speaker = "default"
        with patch.object(rig.sys.stdin, "isatty", return_value=True), self.assertRaisesRegex(ValueError, "speaker"):
            rig.acquire(self.args)
        self.device.assert_not_called()

    def test_overflow_or_dead_worker_retains_complete_stereo_stops_session(self):
        pcm = b"\x01\x00\x02\x00" * 16000
        def take(seconds, save):
            for i in range(0, len(pcm), 4096):
                save(pcm[i:i + 4096], lambda: 0)

        for failure in ("overflow", "death"):
            self.responder("header()\nos.write(1, b'3')\n" +
                           ("reader.read()\n" if failure == "overflow" else "os._exit(1)\n"))
            path = self.root / f"{failure}.wav"
            real_write = os.write
            def blocked(fd, data):
                if len(data) > 4 and failure == "overflow":
                    raise BlockingIOError()
                return real_write(fd, data)
            with (self.subTest(failure=failure), patch.object(rig.os, "write", side_effect=blocked),
                  self.assertRaises((BrokenPipeError, OSError, ValueError)) as error,
                  rig.responding_take(self.args, take) as capture):
                rig.record(path, capture, 16000, 1, 500)
            if failure == "overflow":
                self.assertIn("overflow", str(error.exception))
            with wave.open(str(path)) as wav:
                self.assertEqual(wav.readframes(16000), pcm)

    def test_capture_cutoff_follows_write_completing_decisive_frame(self):
        # Deliver only the first byte of frame 12,001 in its original
        # callback. The decisive byte reaches the worker later, even though
        # that worker reports the same reference length in both cases.
        self.responder("n = header()\nos.write(1,b'3')\nassert len(reader.read(2*n)) == 2*n\n"
                       "os.write(1,b'PB'+struct.pack('<I',12001))\nassert header()==0\nos.write(1,b'D')\n")
        real_write = os.write
        for resume, cutoff in ((15, 15023), (17, 16000)):
            block = 0
            half = False
            def fragment(fd, data):
                nonlocal half
                if 13 <= block < resume:
                    if not half:
                        half = True
                        return real_write(fd, data[:1])
                    raise BlockingIOError()
                return real_write(fd, data)

            def take(_, save):
                nonlocal block
                for block in range(1, 17):
                    save(bytes(4 * 1000), lambda: 23)
                    rig.time.sleep(0.001)
                block = 17  # drain after capture stops

            with (self.subTest(resume=resume), patch.object(rig.os, "write", side_effect=fragment),
                  rig.responding_take(self.args, take) as capture):
                self.assertEqual(capture(1, lambda _: None), rig.BridgeOutcome(True, None, 12001, cutoff))

    def test_live_playback_cannot_attest_to_an_unsent_frame(self):
        flag = self.root / "marked"
        self.responder("n = header()\nos.write(1,b'3')\nassert len(reader.read(2000)) == 2000\n"
                       "os.write(1,b'P')\nopen(sys.argv[-1]+'/marked','x').close()\n"
                       "assert len(reader.read(2*(n-1000))) == 2*(n-1000)\n"
                       "os.write(1,b'B'+struct.pack('<I',1001))\nreader.read()\n")
        def take(_, save):
            save(bytes(4 * 1000), lambda: 50)
            deadline = rig.time.monotonic() + 2
            while not flag.exists():
                self.assertLess(rig.time.monotonic(), deadline)
                rig.time.sleep(0.001)
            for _ in range(15):
                save(bytes(4 * 1000), lambda: 50)
                rig.time.sleep(0.001)

        with (self.assertRaisesRegex(ValueError, "invalid pre-enqueue"),
              rig.responding_take(self.args, take) as capture):
            capture(1, lambda _: None)

    def test_driven_inventory_uses_control_and_each_exact_take_even_for_misses(self):
        self.responder("for result in (b'B', b'PB', b'N'):\n"
                       "    n = header()\n    os.write(1, b'3')\n"
                       "    if result == b'B':\n"
                       "        assert len(reader.read(2400)) == 2400\n        n -= 1200\n"
                       "        os.write(1, b'P')\n"
                       "    assert len(reader.read(2*n)) == 2*n\n"
                       "    os.write(1, result + (struct.pack('<I',1200) if result.endswith(b'B') else b''))\n"
                       "assert header() == 0\nos.write(1,b'D')\n")
        # Slow the fixture producer only outside capture callbacks, like the hardware sample clock.
        index = 0
        def take(seconds, write):
            nonlocal index
            def save(block):
                write(block, lambda: 0)
            index += 1
            prior = self.console.getvalue().count("Apply stimulus")
            self.assertEqual(prior, index - 1)
            save(bytes(4 * 799))
            self.assertEqual(self.console.getvalue().count("Apply stimulus"), prior)
            save(bytes(4))
            self.assertEqual(self.console.getvalue().count("Apply stimulus"), index)
            save(struct.pack("<hh", 800 + index, 0) * 400)
            remaining = 16000 * seconds - 1200
            while remaining:
                n = min(1024, remaining)
                save(bytes(4 * n))
                remaining -= n
                rig.time.sleep(0.001)
        with patch("builtins.input", side_effect=["MUTED", "TAKE 1", "TAKE 2"]):
            self.collect(self.root, rig.session.SCENES[:2], take)
        info = json.loads((self.root / "session.json").read_text())
        self.assertEqual(info["mode"], "driven")
        self.assertIs(info["control_bridge_started"], True)
        self.assertEqual((info["responder_protocol"], info["control_reference_frames"]), (3, 1200))
        self.assertGreaterEqual(info["control_captured_before_reference"], 1200)
        frame = info["control_playback_frame"]
        self.assertGreater(frame, 1200)  # includes the entire callback that observed playback
        self.assertGreaterEqual(30 * self.args.rate - frame - info["route_calibration"]["upper_bound_frames"], 4480)
        self.assertEqual([x["bridge_started"] for x in info["trials"].values()], [True, False])
        self.assertEqual(info["trials"]["turn-001.wav"]["reference_frames"], 1200)
        self.assertGreaterEqual(info["trials"]["turn-001.wav"]["captured_before_reference"], 1200)
        self.assertNotIn("reference_frames", info["trials"]["turn-002.wav"])
        self.assertNotIn("captured_before_reference", info["trials"]["turn-002.wav"])
        self.assertEqual(info["trials"].keys(), {p.name for p in (self.root / "trials").iterdir()})

    def test_control_tail_starts_beyond_the_adc_backlog_at_live_marker(self):
        frames = self.args.rate * 30
        marker_after = (frames // 1024 - 7) * 1024
        pcm = bytes(4 * 800) + struct.pack("<hh", 1000, 0) * 400 + bytes(4 * (frames - 1200))
        flag = self.root / "marked"
        self.responder(f"n = header()\nos.write(1,b'3')\nk = {marker_after}\n"
                       "assert len(reader.read(2*k)) == 2*k\nos.write(1,b'P')\n"
                       "open(sys.argv[-1] + '/marked', 'x').close()\n"
                       "assert len(reader.read(2*(n-k))) == 2*(n-k)\nos.write(1,b'B'+struct.pack('<I',k))\n"
                       "assert header() == 0\nos.write(1,b'D')\n")
        for future_frames in (0, 4639, 4640):  # bound of 160 plus exactly 4480 frames still needed
            directory = self.root / str(future_frames)
            directory.mkdir()
            alsa = self.mock_alsa()
            pos = 0

            def read(_, buf, requested):
                nonlocal pos
                if pos == marker_after:
                    # The test device delivers its next read only once P is ready.
                    deadline = rig.time.monotonic() + 2
                    while not flag.exists():
                        self.assertLess(rig.time.monotonic(), deadline)
                        rig.time.sleep(0.001)
                end = pos + requested
                stereo_pcm.c.memmove(buf, pcm[4 * pos:4 * end], 4 * requested)
                pos = end
                rig.time.sleep(0.001)
                return requested

            def available(_):
                return max(0, frames - pos - future_frames) if pos >= marker_after + 1024 else 0

            alsa.snd_pcm_readi.side_effect = read
            alsa.snd_pcm_avail.side_effect = available
            with (self.subTest(future_frames=future_frames), patch.object(stereo_pcm, "library", return_value=alsa),
                  stereo_pcm.endpoint("hw:2,0", self.args.rate) as take, patch("builtins.input", return_value="MUTED")):
                if future_frames == 4640:
                    self.collect(directory, [], take)
                    info = json.loads((directory / "session.json").read_text())
                    self.assertEqual(info["control_playback_frame"], frames - future_frames)
                    self.assertEqual(info["route_calibration"], json.loads(self.args.route_calibration.read_text()))
                else:
                    with self.assertRaisesRegex(ValueError, "captured tail after playback"):
                        self.collect(directory, [], take)
                    self.assertNotIn("Unmute", self.console.getvalue())
                    self.assertFalse((directory / "session.json").exists())
            with wave.open(str(directory / "muted-control.wav")) as wav:
                self.assertEqual(wav.readframes(frames), pcm)
            self.assertEqual(alsa.snd_pcm_avail.call_count, alsa.snd_pcm_readi.call_count)
            flag.unlink()

    def test_absent_or_post_capture_playback_stops_before_unmute(self):
        stalled = False
        real_write = os.write

        def fragment(fd, data):
            if stalled:
                raise BlockingIOError()
            return real_write(fd, data)

        def take(seconds, save):
            nonlocal stalled
            self.assertEqual(seconds, 30)
            save(bytes(4 * 800), lambda: 0)
            save(struct.pack("<hh", 1000, 0) * 400, lambda: 0)
            remaining = 16000 * seconds - 1200
            while remaining:
                n = min(1024, remaining)
                stalled = n == remaining  # final reference block cannot reach the worker until drain
                save(bytes(4 * n), lambda: 0)
                remaining -= n
                rig.time.sleep(0.001)
            stalled = False

        for outcome, reason in ((b"N", "control did not start a bridge"), (b"PB", "live control_playback_frame")):
            directory = self.root / outcome.decode()
            directory.mkdir()
            self.responder("n = header()\nos.write(1,b'3')\nassert len(reader.read(2*n)) == 2*n\n"
                           f"os.write(1,{outcome!r} + (struct.pack('<I',n) if {outcome!r} == b'PB' else b''))\nreader.read()\n")
            with (self.subTest(outcome=outcome), patch("builtins.input", side_effect=["MUTED"]) as prompt,
                  patch.object(rig.os, "write", side_effect=fragment), self.assertRaisesRegex(ValueError, reason)):
                self.collect(directory, rig.session.SCENES, take)
            prompt.assert_called_once()
            self.assertNotIn("Unmute", self.console.getvalue())
            with wave.open(str(directory / "muted-control.wav")) as wav:
                self.assertEqual(wav.getnframes(), 30 * self.args.rate)
            self.assertEqual(list((directory / "trials").iterdir()), [])
            self.assertFalse((directory / "session.json").exists())

    def test_malformed_markers_or_terminal_outcomes_fail_closed(self):
        pcm = bytes(4 * self.args.rate)
        for outcome in (b"B", b"PPB", b"PN", b"P", b"D", b"NB", b"PBP"):
            self.responder("n = header()\nos.write(1,b'3')\nassert len(reader.read(2*n)) == 2*n\n"
                           f"os.write(1,{outcome!r})\n")
            def take(seconds, save):
                for i in range(0, len(pcm), 4096):
                    save(pcm[i:i + 4096], lambda: 0)
                    rig.time.sleep(0.001)
            path = self.root / f"{outcome.decode()}.wav"
            with (self.subTest(outcome=outcome), self.assertRaisesRegex(ValueError, "responder status"),
                  rig.responding_take(self.args, take) as capture):
                rig.record(path, capture, self.args.rate, 1, 500)
            with wave.open(str(path)) as wav:
                self.assertEqual(wav.readframes(self.args.rate), pcm)

    def test_loss_before_final_stream_check_cannot_publish(self):
        self.responder("assert header() == 0\n")  # exits zero without checking the held route
        with (patch("builtins.input", return_value="MUTED"),
              patch.object(rig, "record", return_value=("control", rig.BridgeOutcome(True, 1200, 1024, 1024))),
              patch.object(rig.session, "checked_capture"), patch.object(rig.session, "check_control"),
              self.assertRaisesRegex(ValueError, "missing responder status")):
            self.collect(self.root, [], MagicMock())
        self.assertFalse((self.root / "session.json").exists())

    def test_bad_pre_enqueue_count_leaves_original_wav(self):
        for count in (0, self.args.rate + 1):
            self.responder("n = header()\nos.write(1,b'3')\nreader.read(n*2)\n"
                           f"os.write(1,b'PB'+struct.pack('<I',{count}))\nreader.read()\n")
            path = self.root / f"bad-{count}.wav"
            pcm = bytes(4 * self.args.rate)
            def take(_, save):
                for i in range(0, len(pcm), 4096):
                    save(pcm[i:i + 4096], lambda: 0)
                    rig.time.sleep(0.001)
            with (self.subTest(count=count), self.assertRaisesRegex(ValueError, "invalid pre-enqueue"),
                  rig.responding_take(self.args, take) as capture):
                rig.record(path, capture, self.args.rate, 1, 500)
            with wave.open(str(path)) as wav:
                self.assertEqual(wav.readframes(self.args.rate), pcm)

    def mock_alsa(self):
        alsa = MagicMock()
        for name in ("open", "close", "hw_params_malloc", "hw_params_any", "hw_params_set_access",
                     "hw_params_set_format", "hw_params_set_channels", "hw_params_set_rate",
                     "hw_params_set_period_size", "hw_params_set_buffer_size", "hw_params", "prepare", "drop"):
            getattr(alsa, "snd_pcm_" + name).return_value = 0
        def installed(_, buffer, period):
            buffer._obj.value, period._obj.value = 320, 80
            return 0
        alsa.snd_pcm_get_params.side_effect = installed
        return alsa

    def test_controller_lookup_checks_open_pcm_direct_link_device_number_and_serial(self):
        identity = "/dev/snd/by-id/usb-Rig_SN01-00"
        data = b"E:ID_BUS=usb\nE:ID_SERIAL=Rig_SN01\nE:ID_SERIAL_SHORT=SN01\nE:ID_USB_INTERFACE_NUM=00\n"
        char = stereo_pcm.stat.S_IFCHR
        dev = os.makedev(116, 32)
        for edits, reason in (
            ({}, None),
            ({"identity": "/dev/snd/by-path/usb-SN01"}, "capture_controller"),
            ({"identity": identity + "/../../controlC2"}, "capture_controller"),
            ({"parent": "/dev/snd/by-path"}, "not a direct link"),
            ({"link": "../by-path/usb-SN01"}, "not a direct link"),
            ({"link": "../controlC3"}, "not a direct link"),
            ({"card": 3}, "opened PCM"),
            ({"number": 1}, "opened PCM"),
            ({"mode": stereo_pcm.stat.S_IFREG}, "character device"),
            ({"dev": dev + 1}, "character device"),
            ({"data": b""}, "USB serial"),
            ({"data": data.replace(b"E:ID_SERIAL_SHORT=SN01\n", b"")}, "USB serial"),
            ({"data": data.replace(b"SN01", b"SN02")}, "USB serial"),
        ):
            alsa = self.mock_alsa()
            alsa.snd_pcm_info_malloc.return_value = alsa.snd_pcm_info.return_value = 0
            alsa.snd_pcm_info_get_card.return_value = edits.get("card", 2)
            alsa.snd_pcm_info_get_device.return_value = edits.get("number", 0)
            node = argparse.Namespace(st_mode=char, st_rdev=dev)
            target = argparse.Namespace(st_mode=edits.get("mode", char), st_rdev=edits.get("dev", dev))

            def udev(path, mode):
                self.assertEqual(path, Path("/run/udev/data/c116:32"))
                alsa.snd_pcm_open.assert_called_once()
                alsa.snd_pcm_hw_params_malloc.assert_not_called()
                return io.BytesIO(edits.get("data", data))

            with (self.subTest(edits=edits), patch.object(stereo_pcm, "library", return_value=alsa),
                  patch.object(os.path, "realpath", return_value=edits.get("parent", "/dev/snd/by-id")),
                  patch.object(os, "readlink", return_value=edits.get("link", "../controlC2")),
                  patch.object(Path, "lstat", return_value=node), patch.object(Path, "stat", return_value=target),
                  patch.object(Path, "open", autospec=True, side_effect=udev)):
                expected = self.assertRaisesRegex(ValueError, reason) if reason else contextlib.nullcontext()
                calibration = dict(capture_controller=edits.get("identity", identity),
                                   capture_period_frames=80, capture_buffer_frames=320)
                with expected, stereo_pcm.endpoint("hw:2,0", 16000, calibration):
                    self.assertIsNone(reason)
                    self.assertEqual(alsa.snd_pcm_hw_params_set_period_size.call_args.args[2:], (80, 0))
                    self.assertEqual(alsa.snd_pcm_hw_params_set_buffer_size.call_args.args[2:], (320,))
                alsa.snd_pcm_close.assert_called_once()
                alsa.snd_pcm_prepare.assert_not_called()
                alsa.snd_pcm_readi.assert_not_called()
                if reason:
                    alsa.snd_pcm_hw_params_malloc.assert_not_called()

    def test_unsupported_or_changed_capture_geometry_never_starts_a_take(self):
        for period, buffer, fail in [(81, 320, None), (80, 319, None), (80, 320, "hw_params_set_period_size"),
                                     (80, 320, "hw_params_set_buffer_size")]:
            alsa = self.mock_alsa()
            if fail:
                getattr(alsa, "snd_pcm_" + fail).return_value = -22
            calibration = dict(capture_controller="/dev/snd/by-id/usb-Rig_SN01-00",
                               capture_period_frames=period, capture_buffer_frames=buffer)
            with (self.subTest(period=period, buffer=buffer, fail=fail),
                  patch.object(stereo_pcm, "library", return_value=alsa), patch.object(stereo_pcm, "verify_controller"),
                  self.assertRaises(OSError if fail else ValueError),
                  stereo_pcm.endpoint("hw:2,0", 16000, calibration)):
                self.fail("configuration mismatch admitted")
            alsa.snd_pcm_prepare.assert_not_called()
            alsa.snd_pcm_readi.assert_not_called()
            alsa.snd_pcm_close.assert_called_once()

    def test_incompatible_rate_rejected_before_capture_and_loss_never_reopens(self):
        alsa = self.mock_alsa()
        with patch.object(stereo_pcm, "library", return_value=alsa):
            alsa.snd_pcm_hw_params_set_rate.return_value = -22
            with self.assertRaises(OSError), stereo_pcm.endpoint("hw:2,0", 96000):
                self.fail("mismatched rate admitted")
            alsa.snd_pcm_prepare.assert_not_called()
            alsa.snd_pcm_readi.assert_not_called()
            alsa.snd_pcm_close.assert_called_once()
            alsa.reset_mock()
            alsa.snd_pcm_hw_params_set_rate.return_value = 0
            with self.assertRaises(OSError), stereo_pcm.endpoint("hw:2,0", 16000) as take:
                alsa.snd_pcm_readi.return_value = -19  # device disconnected
                take(2, lambda _: self.fail("unexpected audio"))
            alsa.snd_pcm_drop.assert_called_once()
            alsa.snd_pcm_close.assert_called_once()
            alsa.snd_pcm_open.assert_called_once()


if __name__ == "__main__":
    unittest.main()
