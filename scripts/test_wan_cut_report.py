import contextlib
import io
import unittest
from pathlib import Path
from unittest.mock import patch

from wan_cut_report import report


class CutReportTests(unittest.TestCase):
    def score(self, *, standby=(), delays=None, missing=(), media=False, skipped=False):
        # The first and last sends have deliberately stale marker labels.
        ms = 1_000_000
        files = {k: str(t * ms) for k, t in [
            ("cut-start", 20), ("cut-installed", 31), ("fallback-a", 83),
            ("fallback-b", 89), ("counter-ready", 90)
        ]}
        a, b = [], []
        for seq, phase in enumerate(["direct", "direct", "cut", "cut", "cut", "cut"]):
            t = seq * 20 * ms
            direct = seq < 5
            path = "Some(Ip(peer))" if direct else "Some(Relay(url))"
            slot = seq + int(skipped and seq >= 3)
            a.append(f"send seq={seq} t_us=0 phase={phase} mono_ns={t} slot={slot} path={path}")
            if seq not in missing:
                via = "StandbyCopy" if seq in standby else "Primary"
                ns = t + (delays or {}).get(seq, 1) * ms
                b.append(f"received seq={seq} mono_ns={ns} via={via} direct={str(direct).lower()}")
        files.update({"a.log": "\n".join(a), "b.log": "\n".join(b)})
        with patch.object(Path, "read_text", autospec=True, side_effect=lambda p: files[p.name]):
            with contextlib.redirect_stdout(io.StringIO()):
                return report(Path("."), True, "short", "interface-loss", media, "qad")

    def test_bounds_are_by_send_not_label_or_decode(self):
        self.assertTrue(self.score(missing=[5]))  # after counter-ready, despite label
        self.assertFalse(self.score(missing=[1]))  # exact cut-start, despite label
        self.assertFalse(self.score(delays={4: 11}))  # decoded *after* counter-ready
        self.assertFalse(self.score(delays={2: 5}))  # strict inequality
        self.assertFalse(self.score(skipped=True))

    def test_only_installed_pre_recovery_evidence_counts(self):
        self.assertTrue(self.score(standby=[2], media=True))
        for seqs, delays in [([1], {}), ([4], {4: 4}), ([5], {})]:
            with self.assertRaisesRegex(AssertionError, "no standby delivery"):
                self.score(standby=seqs, delays=delays, media=True)


if __name__ == "__main__":
    unittest.main()
