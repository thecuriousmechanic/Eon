import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from run_lean_backtest import run, verify_summary


class BacktestResultTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.path = Path(directory.name) / "summary.json"

    def result(self, status, orders, equity):
        self.path.write_text(json.dumps({
            "state": {"Status": status},
            "statistics": {"Total Orders": orders, "End Equity": equity},
        }), encoding="utf-8")

    def request_log(self, skip=()):
        paths = (
            f"/equity/usa/minute/spy/{date}_{kind}.zip"
            for date in ("20131007", "20131008", "20131009", "20131010", "20131011")
            for kind in ("trade", "quote")
            if (date, kind) not in skip
        )
        log = self.path.parent / "succeeded-data-requests-001.txt"
        log.write_text("\n".join(paths), encoding="utf-8")

    def test_pinned_sample_oracle(self):
        self.result("Completed", "1", "101691.92")
        self.request_log()
        self.assertEqual(verify_summary(self.path, True)["Total Orders"], "1")
        for orders, equity in [("0", "100000"), ("1", "100000")]:
            with self.subTest(orders=orders, equity=equity):
                self.result("Completed", orders, equity)
                with self.assertRaises(ValueError):
                    verify_summary(self.path, True)

    def test_correct_equity_is_not_enough_when_the_middle_session_lacks_trade_data(self):
        self.result("Completed", "1", "101691.92")
        with self.assertRaisesRegex(ValueError, "request log"):
            verify_summary(self.path, True)
        self.request_log(skip={("20131009", "trade")})
        with self.assertRaisesRegex(ValueError, "20131009_trade.zip"):
            verify_summary(self.path, True)

    def test_other_strategies_may_finish_without_orders_but_not_with_errors(self):
        self.result("Completed", "0", "100000")
        self.assertEqual(verify_summary(self.path, False)["Total Orders"], "0")
        self.result("RuntimeError", "1", "101691.92")
        with self.assertRaises(ValueError):
            verify_summary(self.path, False)

    def test_missing_or_invalid_output_is_not_success(self):
        with self.assertRaises(FileNotFoundError):
            verify_summary(self.path, True)
        self.path.write_text("{", encoding="utf-8")
        with self.assertRaises(ValueError):
            verify_summary(self.path, True)

    def test_missing_config_never_falls_back_to_the_launcher_default(self):
        lean = self.path.parent
        launcher = lean / "Launcher"
        launcher.mkdir()
        (launcher / "QuantConnect.Lean.Launcher.csproj").touch()
        with patch("run_lean_backtest.LEAN", lean), patch("run_lean_backtest.CONFIG", self.path):
            with self.assertRaisesRegex(ValueError, "missing backtest configuration"):
                run("BasicTemplateAlgorithm", lean, lean)


if __name__ == "__main__":
    unittest.main()
