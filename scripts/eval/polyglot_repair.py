"""Run the closed benign diagnostic-repair corpus; accepts no source, path or command."""

import argparse
from collections import Counter
import sys

from repair.corpus import FILES, inventory
from repair.processes import BenchError, Toolchain, private_workspace
from repair.toolchains import behavior, candidate, diagnostics


MAX_EDITS = 3


def trial(tools, cwd, case):
    lang, src = case.language, case.source
    seen, steps = {src}, []
    while True:
        errors = diagnostics(tools, cwd, lang, src)
        if not errors:
            if not steps:
                return "baseline-clean", steps
            return ("pass" if behavior(tools, cwd, lang, src, case.checks) else "behavior-fail"), steps
        # Unlike a compiler error, a lint diagnostic alone doesn't establish a
        # behavioral defect. Prove the original fails the same independent test.
        if not steps and lang == "python" and behavior(tools, cwd, lang, src, case.checks):
            return "baseline-passed", steps
        if len(steps) == MAX_EDITS:
            return "edit-budget", steps
        change = candidate(tools, cwd, lang, src, errors)
        if change is None:
            return "abstain", steps
        rule, src = change
        if src in seen or len(src.encode()) > 8192:
            return "cycle-or-size", steps
        seen.add(src)
        steps.append(rule)


def run_corpus(langs, log):
    cases = inventory(langs)
    tools = Toolchain(langs)
    outcomes = Counter()
    first = 0
    with private_workspace() as work:
        for n, case in enumerate(cases):
            cwd = work / str(n)
            cwd.mkdir(mode=0o700)
            status, rules = trial(tools, cwd, case)
            print(f"{case.language}/{case.label}: {status} ({', '.join(rules) or 'no edit'})", file=log, flush=True)
            outcomes[status] += 1
            first += status == "pass" and len(rules) == 1
    # An abstention stays in the denominator. The 10.6 acceptance bar is
    # strictly >80%; use the more conservative single-edit score as the gate.
    # Never accept a mislabeled baseline or candidate that failed behavior.
    return {"cases": len(cases), "one_edit_pass": first, "bounded_pass": outcomes["pass"],
            "outcomes": dict(sorted(outcomes.items())),
            "passed": first * 5 > len(cases) * 4 and
            not any(outcomes[k] for k in ("baseline-clean", "baseline-passed", "behavior-fail"))}


def main(argv=None):
    cli = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    cli.add_argument("--language", choices=FILES, help="run just this fixed partition")
    args = cli.parse_args(argv)
    result = run_corpus([args.language] if args.language else list(FILES), sys.stdout)
    print("one-edit pass: {one_edit_pass}/{cases}; bounded-loop pass: {bounded_pass}/{cases}; "
          "outcomes: {outcomes}".format(**result))
    return int(not result["passed"])


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (BenchError, OSError, ValueError) as exc:
        print(f"benchmark error: {exc}", file=sys.stderr)
        sys.exit(2)
