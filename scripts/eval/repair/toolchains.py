"""Diagnostics see only the subject. Behavior checks enter only the verifier."""

import json
from pathlib import Path
import re

from . import python_rules
from .processes import BenchError


SOURCE = {"rust": "subject.rs", "typescript": "subject.ts", "kotlin": "Subject.kt", "python": "subject.py"}
TS_FLAGS = ["--strict", "--noImplicitOverride", "--target", "ES2022", "--module", "commonjs",
            "--pretty", "false", "--noEmitOnError"]


def byte_position(src, row, column, lang):
    """Compiler one-based UTF-16 line/column to a tree-sitter UTF-8 byte offset."""
    # Kotlin doesn't treat e.g. U+2028 inside a string as a new source line.
    separators = r"\r\n|[\r\n]" if lang == "kotlin" else r"\r\n|[\r\n\u2028\u2029]"
    starts = [0, *(m.end() for m in re.finditer(separators, src))]
    if not (1 <= row <= len(starts) and 1 <= column):
        raise BenchError("invalid diagnostic location")
    start = starts[row - 1]
    line = src[start:starts[row] if row < len(starts) else len(src)]
    units = re.sub(f"(?:{separators})$", "", line).encode("utf-16-le")
    n = 2 * (column - 1)
    if n >= len(units):
        raise BenchError("diagnostic outside source")
    try:
        prefix = units[:n].decode("utf-16-le")
    except UnicodeDecodeError as exc:
        raise BenchError("diagnostic splits a surrogate pair") from exc
    return len(src[:start].encode()) + len(prefix.encode())


def diagnostics(tools, cwd, lang, src):
    if lang == "python":
        return python_rules.diagnose(src)
    name = SOURCE[lang]
    (cwd / name).write_text(src)
    if lang == "rust":
        cmd = [tools.bins["rustc"], "--edition=2021", "--crate-type=lib", "--emit=metadata",
               "--error-format=json", name]
    elif lang == "kotlin":
        cmd = [tools.bins["kotlinc"], "-Xrender-internal-diagnostic-names", name, "-d", "classes"]
    else:
        cmd = [tools.bins["tsc"], *TS_FLAGS, "--noEmit", name]
    status, log = tools.run(cwd, cmd)
    if status == 0:
        return []
    if lang == "rust":
        errors = [d for d in map(json.loads, log.splitlines())
                  if d.get("level") == "error" and any(s["file_name"] == name and s["is_primary"] for s in d["spans"])]
    else:
        pattern = (r"^Subject\.kt:(\d+):(\d+): error: \[(\w+)\] (.*)$" if lang == "kotlin"
                   else r"^subject\.ts\((\d+),(\d+)\): error TS(\d+): (.*)$")
        errors = []
        for row, col, code, message in re.findall(pattern, log, re.M):
            offset = byte_position(src, int(row), int(col), lang)
            if lang == "typescript":
                code = int(code)
            errors.append(dict(code=code, offset=offset, message=message))
    if not errors:
        raise BenchError(f"{lang} failed without a recognized subject diagnostic")
    return errors


def candidate(tools, cwd, lang, src, errors):
    err = errors[0]
    if lang == "python":
        # Multiple competing lint diagnoses are not ranked by behavior tests.
        return (err.code, err.apply(src)) if len(errors) == 1 else None
    request = json.dumps(dict(language=lang, source=src, diagnostic=err)).encode()
    status, log = tools.run(cwd, [tools.bins["edit"]], request)
    if status != 0:
        raise BenchError("source-only edit adapter failed")
    change = json.loads(log)
    return (change["rule"], change["source"]) if change else None


def behavior(tools, cwd, lang, src, checks):
    """Return pass/fail for the fixed, separately authored behavior suite."""
    ok = "EON_CHECKS_PASSED"
    if lang == "rust":
        (cwd / "subject.rs").write_text(src)
        (cwd / "behavior.rs").write_text(f'include!("subject.rs");\nfn main() {{\n{checks}\nprintln!("{ok}");\n}}\n')
        compile_cmd = [tools.bins["rustc"], "--edition=2021", "behavior.rs", "-o", "behavior"]
        run = [str(cwd / "behavior")]
    elif lang == "typescript":
        program = (src + '\nfunction check(test: boolean) { if (!test) throw Error("behavior mismatch"); }\n'
                   + "async function verify() {\n" + checks + f'\nconsole.log("{ok}");\n}}\nvoid verify();\n')
        (cwd / "behavior.ts").write_text(program)
        compile_cmd = [tools.bins["tsc"], *TS_FLAGS, "behavior.ts"]
        run = [tools.bins["node"], "--unhandled-rejections=strict", "--max-old-space-size=256", "behavior.js"]
    elif lang == "kotlin":
        # The clean standalone compilation produced classes/. Compile checks
        # separately so they cannot affect the subject's diagnostic or typing.
        (cwd / "Behavior.kt").write_text(f'fun main() {{\n{checks}\nprintln("{ok}")\n}}\n')
        compile_cmd = [tools.bins["kotlinc"], "-classpath", "classes", "Behavior.kt", "-d", "checks"]
        stdlib = Path(tools.bins["kotlinc"]).resolve().parents[1] / "lib/kotlin-stdlib.jar"
        run = [tools.bins["java"], "-Xmx192m", "-XX:-UsePerfData", f"-Djava.io.tmpdir={cwd}", "-cp",
               f"checks:classes:{stdlib}", "BehaviorKt"]
    else:
        (cwd / "behavior.py").write_text(src + '\nprint("EON_CHECKS_STARTED", flush=True)\n' + checks + f'\nprint("{ok}")\n')
        compile_cmd = None
        run = [tools.bins["python3"], "-I", "-B", "-S", "behavior.py"]
    if compile_cmd and tools.run(cwd, compile_cmd)[0] != 0:
        return False
    status, output = tools.run(cwd, run)
    if lang == "python" and "EON_CHECKS_STARTED" not in output.splitlines():
        raise BenchError("Python checks did not start")
    return status == 0 and output.splitlines()[-1:] == [ok]
