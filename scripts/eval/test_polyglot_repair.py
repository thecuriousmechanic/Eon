import contextlib
import io
import json
import unittest
from unittest.mock import Mock, mock_open, patch

import polyglot_repair as bench
from repair import python_rules
from repair.corpus import Case, FILES, inventory
from repair.processes import BenchError, private_workspace
from repair.toolchains import byte_position


class InventoryTests(unittest.TestCase):
    def test_closed_inventory_has_distinct_subjects_and_checks(self):
        cases = inventory(FILES)
        self.assertEqual(len(cases), 50)
        for field in ("label", "source", "checks"):
            self.assertEqual(len({getattr(c, field) for c in cases}), len(cases))
        with patch("pathlib.Path.open", mock_open(read_data=b"# @case changed\npass\n")):
            with self.assertRaisesRegex(BenchError, "unreviewed corpus"):
                inventory(["python"])

    def test_no_source_or_command_cli(self):
        for args in (["--source", "anything"], ["--language", "tsx"], ["cat"]):
            with self.subTest(args=args), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as failure:
                    bench.main(args)
                self.assertEqual(failure.exception.code, 2)

    def test_private_workspace_cleanup_on_exception(self):
        with self.assertRaises(ValueError):
            with private_workspace() as path:
                self.assertEqual(path.stat().st_mode & 0o777, 0o700)
                raise ValueError()
        self.assertFalse(path.exists())


class SourceOnlyTests(unittest.TestCase):
    def test_python_uses_ast_and_types_not_identifier_or_comment_text(self):
        src = ('def keep(group: list, value):\n'
               '    # group = group.sort()\n'
               '    group = group.extend(value)\n'
               '    return group\n')
        (issue,) = python_rules.diagnose(src)
        self.assertEqual(issue.code, "assigned-in-place-result")
        self.assertEqual(issue.apply(src), src.replace("group = group.extend", "group.extend"))
        self.assertFalse(python_rules.diagnose(issue.apply(src)))
        self.assertFalse(python_rules.diagnose(src.replace("list", "CustomGroup")))

    def test_conservative_lint_cases(self):
        for src in (
                "def f(thing):\n    return thing is None\n",
                "def g(x):\n    return [lambda x=x: x for x in x]\n",
                "def label(x):\n    return '{unknown}'\n"):
            with self.subTest(src=src):
                self.assertFalse(python_rules.diagnose(src))

    def test_byte_vs_character_offsets(self):
        for lang in ("kotlin", "typescript"):
            self.assertEqual(byte_position("é🌱x\n", 1, 4, lang), 6)
            self.assertEqual(byte_position("é🌱x\r\nmany\n", 2, 2, lang), 10)
            for col in (0, 3, 5, 6):
                with self.subTest(lang=lang, col=col), self.assertRaises(BenchError):
                    byte_position("é🌱x\r\n", 1, col, lang)
        src = 'def f(tag: str):\n    return "🌱" + str(tag is "oak")\n'
        (issue,) = python_rules.diagnose(src)
        self.assertEqual(issue.apply(src), src.replace('tag is ', 'tag == '))

    def test_k2_diagnostic_to_native_request_is_source_only(self):
        prefix = "fun f(x: String?): Int? = /* é🌱\u2028 */ x"
        src = prefix + ".length\n"
        col = len(prefix.encode("utf-16-le")) // 2 + 1
        log = f"Subject.kt:1:{col}: error: [UNSAFE_CALL] compiler message\n"
        tools = Mock(bins={"kotlinc": "compiler", "edit": "adapter"})
        tools.run.side_effect = [(1, log), (0, "null")]
        with private_workspace() as cwd:
            errors = bench.diagnostics(tools, cwd, "kotlin", src)
            self.assertIsNone(bench.candidate(tools, cwd, "kotlin", src, errors))
            directory, cmd, payload = tools.run.call_args.args
            self.assertEqual((directory, cmd), (cwd, ["adapter"]))
            self.assertEqual(json.loads(payload), {
                "language": "kotlin", "source": src, "diagnostic": {
                    "code": "UNSAFE_CALL", "offset": len(prefix.encode()), "message": "compiler message",
                },
            })


class ScoringTests(unittest.TestCase):
    # These are state-machine tests, not execution of extra source cases.
    case = Case("rust", "opaque", "initial", "private-checks")

    def test_behavior_failure_cannot_trigger_another_repair(self):
        with (patch.object(bench, "diagnostics", side_effect=[["D"], []]),
              patch.object(bench, "candidate", return_value=("R", "revision")) as repair,
              patch.object(bench, "behavior", return_value=False) as verify):
            self.assertEqual(bench.trial(None, None, self.case), ("behavior-fail", ["R"]))
            repair.assert_called_once_with(None, None, "rust", "initial", ["D"])
            verify.assert_called_once_with(None, None, "rust", "revision", "private-checks")

    def test_clean_baseline_is_not_a_score(self):
        with patch.object(bench, "diagnostics", return_value=[]), patch.object(bench, "behavior") as verify:
            self.assertEqual(bench.trial(None, None, self.case), ("baseline-clean", []))
            verify.assert_not_called()

    def test_cycles_and_edit_budget_do_not_run_behavior(self):
        with (patch.object(bench, "diagnostics", return_value=["D"]),
              patch.object(bench, "candidate", return_value=("R", "initial")) as repair,
              patch.object(bench, "behavior") as verify):
            self.assertEqual(bench.trial(None, None, self.case), ("cycle-or-size", []))
            repair.side_effect = [("R", str(i)) for i in range(bench.MAX_EDITS)]
            self.assertEqual(bench.trial(None, None, self.case), ("edit-budget", ["R"] * bench.MAX_EDITS))
            verify.assert_not_called()


if __name__ == "__main__":
    unittest.main()
