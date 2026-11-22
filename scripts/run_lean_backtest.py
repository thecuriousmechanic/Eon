#!/usr/bin/env python3
"""Build the pinned LEAN launcher and run a local C# simulation (never live trading)."""

import argparse
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LEAN = ROOT / "integrations" / "lean"
CONFIG = ROOT / "integrations" / "backtest.json"
SAMPLE = "BasicTemplateAlgorithm"


def verify_sample_sessions(run_dir: Path) -> None:
    """Missing middle sessions may not change a buy-and-hold strategy's ending equity."""
    logs = list(run_dir.glob("succeeded-data-requests-*.txt"))
    if len(logs) != 1:
        raise ValueError("expected one LEAN successful data request log for the sample")
    expected = {
        f"equity/usa/minute/spy/201310{day:02d}_{kind}.zip"
        for day in range(7, 12) for kind in ("trade", "quote")
    }
    with logs[0].open(encoding="utf-8-sig") as requests:
        for line in requests:
            expected.discard(line.rstrip("\r\n").replace("\\", "/").lstrip("/"))
    if expected:
        raise ValueError(f"sample did not read: {', '.join(sorted(expected))}")


def verify_summary(path: Path, check_sample: bool) -> dict:
    """Check LEAN's persisted result, not just its process exit code."""
    result = json.loads(path.read_text(encoding="utf-8-sig"))
    if result["state"]["Status"] != "Completed":
        raise ValueError(f"backtest did not complete: {result['state']['Status']}")
    stats = result["statistics"]
    if check_sample:
        expected = {"Total Orders": "1", "End Equity": "101691.92"}
        for key, value in expected.items():
            if stats[key] != value:
                raise ValueError(f"sample {key}: expected {value}, got {stats[key]}")
        verify_sample_sessions(path.parent)
    return stats


def run(algorithm: str, data: Path, output_root: Path) -> Path:
    if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", algorithm):
        raise ValueError("algorithm must be a C# class name")
    project = LEAN / "Launcher" / "QuantConnect.Lean.Launcher.csproj"
    if not project.is_file():
        raise ValueError("initialize LEAN first: git submodule update --init -- integrations/lean")
    # LEAN otherwise falls back to the launcher's config, which can select a live brokerage.
    if not CONFIG.is_file():
        raise ValueError(f"missing backtest configuration: {CONFIG}")
    if not data.is_dir():
        raise ValueError(f"no local data directory at {data}")
    subprocess.run(["dotnet", "build", str(project), "-c", "Release"], cwd=LEAN, check=True)
    binary = project.parent / "bin" / "Release" / "QuantConnect.Lean.Launcher.dll"
    output_root.mkdir(parents=True, exist_ok=True)
    run_dir = Path(tempfile.mkdtemp(prefix=f"{algorithm}-", dir=output_root.resolve()))
    print(f"Backtest output: {run_dir}", flush=True)
    subprocess.run(
        [
            "dotnet", str(binary),
            "--config", str(CONFIG),
            "--environment", "backtesting",
            "--algorithm-type-name", algorithm,
            "--algorithm-id", algorithm,
            "--data-folder", str(data.resolve()),
            "--results-destination-folder", str(run_dir),
        ],
        cwd=binary.parent, check=True, stdin=subprocess.DEVNULL,
    )
    summary = run_dir / f"{algorithm}-summary.json"
    stats = verify_summary(summary, algorithm == SAMPLE and data.resolve() == LEAN / "Data")
    print(f"Simulated orders: {stats['Total Orders']}; ending equity: {stats['End Equity']}")
    return summary


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--algorithm", default=SAMPLE, help="trusted class in the pinned C# assembly")
    parser.add_argument("--data", type=Path, default=LEAN / "Data", help="local LEAN-format data")
    parser.add_argument("--output", type=Path, default=ROOT / "backtests", help="parent for fresh result directories")
    args = parser.parse_args()
    try:
        print(run(args.algorithm, args.data, args.output))
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print(f"Backtest failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
