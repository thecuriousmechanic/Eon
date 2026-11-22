"""Run the fixed Task 10.6 checks and independently measure supplied physical turns."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

import noisy_duplex
import physical_session
import polyglot_repair
import software_turns
import speaker_handoffs
from repair.processes import BenchError, private_workspace


REPLAY = ("identical_submissions_and_verified_results_replay_to_the_same_dag_and_outcomes",
          "replay_engine::frozen_submission_rejects_a_different_dag_even_with_identical_topological_order",
          "replay_engine::historical_lost_ack_rejoins_the_uninterrupted_dag_and_file_outcomes",
          "replay_engine::torn_ack_never_reissues_non_idempotent_work_or_unblocks_its_join")
DEFENSE = ("defensive_regressions::token_check_detection_repair_and_negative_controls_are_source_only",)


def verdict(statuses):
    statuses = tuple(statuses)
    if "failed" in statuses:
        return "failed"
    return "passed" if statuses and all(s == "passed" for s in statuses) else "unavailable"


def measured(passed):
    return "passed" if passed else "failed"


def kernel_checks():
    command = ["cargo", "test", "--offline", "--locked", "-j2", "-p", "eon-kernel",
               "--test", "eval_components", "--", "--format", "pretty"]
    with private_workspace() as scratch:
        run = subprocess.run(command, cwd=noisy_duplex.ROOT / "engine/core-rs", text=True,
                             stdout=subprocess.PIPE,
                             env={**os.environ, "CARGO_TERM_COLOR": "never", "TMPDIR": str(scratch)})
    print(run.stdout, file=sys.stderr, end="")
    results = dict(re.findall(r"^test (\S+) \.\.\. (ok|FAILED|ignored)$", run.stdout, re.M))
    complete = (run.returncode == 0 and
                re.search(r"^test result: ok\. \d+ passed; 0 failed; 0 ignored;", run.stdout, re.M))

    def group(names):
        marked = {name: results.get(name) for name in names}
        if "FAILED" in marked.values():
            status = "failed"
        elif all(v == "ok" for v in marked.values()) and (
                complete or any(results.get(name) == "FAILED" for name in (*REPLAY, *DEFENSE))):
            status = "passed"
        else:
            status = "unavailable"
        return {"status": status, "tests": marked}
    return group(REPLAY), group(DEFENSE)


def speaker_checks(policy=noisy_duplex.BASELINE_MODEL):
    rows = []
    reason = None
    try:
        rows.extend(noisy_duplex.run_conditions(policy))
    except noisy_duplex.INPUT_ERRORS as error:
        reason = str(error)
    states = [noisy_duplex.condition_status(row) for row in rows]
    if reason is not None or [r["condition"] for r in rows] != list(noisy_duplex.CONDITIONS):
        states.append("unavailable")
    handoffs = []
    try:
        handoffs.extend(speaker_handoffs.run_pairs(policy))
    except speaker_handoffs.INPUT_ERRORS as error:
        handoff_error = str(error)
    else:
        handoff_error = None
    states.extend(pair["status"] for pair in handoffs)
    if handoff_error is not None or [p["pair"] for p in handoffs] != speaker_handoffs.EXPECTED:
        states.append("unavailable")
    report = {"status": verdict(states), "speaker_policy": policy, "conditions": rows, "active_handoffs": handoffs}
    if handoff_error is not None:
        report["handoff_input_error"] = handoff_error
    if reason is not None:
        report["reason"] = reason
    return report


def common_gates(policy=noisy_duplex.BASELINE_MODEL):
    gates = {}
    try:
        gates["replay"], gates["defensive"] = kernel_checks()
    except OSError as error:
        gates["replay"] = gates["defensive"] = {"status": "unavailable", "reason": str(error)}
    try:
        repair = polyglot_repair.run_corpus(list(polyglot_repair.FILES), sys.stderr)
        gates["repair"] = {**repair, "status": measured(repair["cases"] == 50 and repair["passed"])}
    except (BenchError, OSError, ValueError, subprocess.SubprocessError) as error:
        gates["repair"] = {"status": "unavailable", "reason": str(error)}
    gates["speaker_decisions"] = speaker_checks(policy)
    return gates


def run(directory):
    gates = common_gates()
    try:
        evidence = physical_session.measure(directory)
        device = evidence["task_10_2"]
        device = {**device, "status": measured(device["passed"])}
        scenes = evidence["noisy_scenes"]
        timing = verdict("failed" if s["misses"] else
                         "unavailable" if s["max_ttfa_ms"] is None else measured(s["max_ttfa_ms"] < 200)
                         for s in scenes.values())
        gates["noisy_physical_ttfa"] = {"status": timing, "scenes": scenes}
        session = evidence["manifest_sha256"]
    except physical_session.INPUT_ERRORS as error:
        device = {"status": "unavailable", "p50_ms": None, "p99_ms": None, "reason": str(error)}
        gates["noisy_physical_ttfa"] = {"status": "unavailable", "scenes": None, "reason": str(error)}
        session = None
    suite = verdict(g["status"] for g in gates.values())
    return {"physical_manifest_sha256": session, "task_10_2": device, "task_10_6": {"status": suite, "gates": gates}}


def software_run(policy):
    gates = common_gates(policy)
    gates["software_render"] = software_turns.measure()
    return {"scope": "software-only", "status": verdict(g["status"] for g in gates.values()), "gates": gates}


def main(argv=None):
    cli = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    scope = cli.add_mutually_exclusive_group()
    scope.add_argument("--physical-session", type=Path, help="directory of authorized original captures; no devices")
    scope.add_argument("--software-only", action="store_true", help="measure recorded input to software render, not acoustics")
    cli.add_argument("--speaker-policy", choices=noisy_duplex.POLICIES, help="explicit software-only speaker comparison")
    args = cli.parse_args(argv)
    if args.speaker_policy and not args.software_only:
        cli.error("--speaker-policy requires --software-only")
    if args.software_only:
        result = software_run(args.speaker_policy or noisy_duplex.BASELINE_MODEL)
        status = result["status"]
    else:
        result = run(args.physical_session)
        status = verdict(result[t]["status"] for t in ("task_10_2", "task_10_6"))
    print(json.dumps(result, indent=2))
    return {"passed": 0, "failed": 1, "unavailable": 2}[status]


if __name__ == "__main__":
    raise SystemExit(main())
