"""Small AST lint/fix pass for simple single-function CPython source units."""

import ast
from dataclasses import dataclass
import re
import warnings


@dataclass(frozen=True)
class Finding:
    code: str
    edits: tuple  # byte-start, byte-end, replacement

    def apply(self, src):
        data = src.encode()
        for start, end, value in sorted(self.edits, reverse=True):
            data = data[:start] + value.encode() + data[end:]
        return data.decode()


def diagnose(src):
    if len(src.encode()) > 8192:
        raise ValueError("source budget exceeded")
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", SyntaxWarning)
        module = ast.parse(src)
    if len(module.body) != 1 or not isinstance(module.body[0], ast.FunctionDef):
        return []
    fn = module.body[0]
    if any(isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)) and n is not fn
           for n in ast.walk(fn)):
        return []  # parameter types do not carry into nested declarations
    names = {p.arg: p.annotation for p in fn.args.args}
    lines = src.encode().splitlines(keepends=True)
    starts = [0]
    for line in lines:
        starts.append(starts[-1] + len(line))

    def edit(n, value):
        return starts[n.lineno - 1] + n.col_offset, starts[n.end_lineno - 1] + n.end_col_offset, value

    def typed(n, kind):
        if not isinstance(n, ast.Name):
            return False
        ann = names.get(n.id)
        if isinstance(ann, ast.Subscript):
            ann = ann.value
        return isinstance(ann, ast.Name) and ann.id == kind

    findings = []

    def emit(code, n, value):
        findings.append(Finding(code, (edit(n, value),)))

    def expression(n):
        return ast.get_source_segment(src, n)

    for arg, default in zip(reversed(fn.args.args), reversed(fn.args.defaults)):
        if isinstance(default, ast.List) and not default.elts:
            first = fn.body[0]
            i = starts[first.lineno - 1]
            indent = lines[first.lineno - 1][:first.col_offset].decode()
            if indent.isspace() and not ast.get_docstring(fn):
                guard = f"{indent}if {arg.arg} is None:\n{indent}    {arg.arg} = []\n"
                findings.append(Finding("mutable-default", (edit(default, "None"), (i, i, guard))))

    for n in ast.walk(fn):
        match n:
            case ast.Assign(targets=[ast.Name() as name], value=ast.Call(func=ast.Attribute() as attr) as call):
                if (attr.attr in {"append", "extend", "reverse", "sort"} and typed(attr.value, "list")
                        and attr.value.id == name.id):
                    emit("assigned-in-place-result", n, expression(call))
            case ast.BinOp(left=ast.List(elts=[ast.BinOp(left=ast.List(), op=ast.Mult()) as row]),
                           op=ast.Mult(), right=ast.Name() as count):
                # Only duplicate a pure row expression; never repeat a call.
                if (all(isinstance(x, (ast.List, ast.BinOp, ast.Mult, ast.Load, ast.Constant, ast.Name))
                        for x in ast.walk(row)) and not any(isinstance(x, ast.Name) and x.id == "_" for x in ast.walk(row))):
                    emit("repeated-mutable-row", n, f"[{expression(row)} for _ in range({count.id})]")
            case ast.comprehension(target=ast.Tuple(elts=[_, _]), iter=ast.Name() as d) if typed(d, "dict"):
                emit("dict-unpack", d, d.id + ".items()")
            case ast.Compare(left=lhs, ops=[ast.Is() | ast.IsNot() as op], comparators=[ast.Constant(value=str()) as rhs]):
                emit("string-identity", n, f"{expression(lhs)} {'!=' if isinstance(op, ast.IsNot) else '=='} {expression(rhs)}")
            case ast.BoolOp(op=ast.Or(), values=[
                    ast.Compare(left=ast.Name() as name, ops=[ast.Eq()], comparators=[ast.Constant(value=str())]),
                    ast.Constant(value=str() as s) as rhs]) if s:
                emit("truthy-or-operand", rhs, f"{name.id} == {expression(rhs)}")
            case ast.If(test=ast.Attribute(attr=attr) as pred) if typed(pred.value, "str") and attr in {
                    "isalpha", "isdecimal", "isdigit", "islower", "isspace", "isupper"}:
                emit("method-as-condition", pred, expression(pred) + "()")
            case ast.ListComp(elt=ast.Lambda() as lam, generators=[ast.comprehension(target=ast.Name() as var)]):
                if (not ast.unparse(lam.args) and any(isinstance(x, ast.Name) and x.id == var.id for x in ast.walk(lam.body))
                        and not any(isinstance(x, (ast.Lambda, ast.comprehension)) for x in ast.walk(lam.body))):
                    emit("closure-loop-capture", lam, f"lambda {var.id}={var.id}: {expression(lam.body)}")
            case ast.ExceptHandler(type=ast.BoolOp(op=ast.Or(), values=exceptions) as ex):
                if all(isinstance(e, ast.Name) and e.id.endswith("Error") for e in exceptions):
                    emit("except-or", ex, "(" + ", ".join(e.id for e in exceptions) + ")")
            case ast.Return(value=ast.GeneratorExp() as gen) if isinstance(fn.returns, ast.Name) and fn.returns.id == "list":
                emit("declared-list-generator", gen, ast.unparse(ast.ListComp(elt=gen.elt, generators=gen.generators)))
            case ast.Assert(test=ast.Tuple(elts=[test, ast.Constant(value=str()) as message]), msg=None):
                emit("assert-truthy-tuple", n, f"assert {expression(test)}, {expression(message)}")
            case ast.Return(value=ast.Constant(value=str() as s) as literal):
                fields = re.findall(r"\{([a-zA-Z_]\w*)\}", s)
                raw = expression(literal)
                if (fields and set(fields) <= names.keys() and len(fields) == s.count("{") == s.count("}")
                        and raw.startswith(("'", '"'))):
                    emit("unformatted-parameters", literal, "f" + raw)
    return findings
