"""Closed inventory. Digests pin inputs and tests, never a repaired answer."""

from dataclasses import dataclass
import hashlib
from pathlib import Path
import re

from .processes import BenchError


FILES = {
    # language: (scenario count, cases sha256, checks sha256)
    "rust": (17, "fc25f1c9231b7394c5a12bef891b0366e0fabd9c486b1f4169c6565e21c5b29d",
             "78b8bfdab5f6b985ca7ded3d02493986ffd82dfc1d3717d93dec04692b91dd1f"),
    "kotlin": (10, "887b02a01401670ce0d39742a11dad2e0dcfc4f2f8660002753549c725aa1723",
               "2423ed89883d7fc69e2dcdccc272b83a5ffc6be012e07d073622b816293820fc"),
    "python": (12, "e9779dfc3e4785566720991b959b1fbd3e649dcb50b3bca19771adc40ddae862",
               "0d5ac27ff7cc73f9faf889e992ab2eb2a3caf23a8f115e1caecddcc3f73b6364"),
    "typescript": (11, "435dac391f46f3417eb43a04cd53539be57cfcbd26244e716ec1e15ea7f96d50",
                   "2d81389a3b2cb5cd53b43b5d6e831cd48f69c41354cf3e8d24274727272a7a56"),
}


@dataclass(frozen=True)
class Case:
    language: str
    label: str
    source: str
    checks: str


def _read(language, suffix, digest):
    path = Path(__file__).with_name("corpus") / f"{language}.{suffix}"
    with path.open("rb") as f:
        raw = f.read(65537)
    if len(raw) > 65536 or hashlib.sha256(raw).hexdigest() != digest:
        raise BenchError(f"unreviewed corpus: {path.name}")
    parts = re.split(r"^(?://|#) @case ([a-z0-9-]+)\n", raw.decode(), flags=re.M)
    data = dict(zip(parts[1::2], parts[2::2], strict=True))
    if parts[0] or len(data) != len(parts[1::2]) or any(not s.strip() or len(s) > 4096 for s in data.values()):
        raise BenchError(f"invalid sections: {path.name}")
    return data


def inventory(languages):
    cases = []
    for language in languages:
        count, a, b = FILES[language]
        sources = _read(language, "cases", a)
        checks = _read(language, "checks", b)
        if len(sources) != count or sources.keys() != checks.keys() or len(set(sources.values())) != count:
            raise BenchError(f"invalid inventory: {language}")
        cases.extend(Case(language, k, v, checks[k]) for k, v in sources.items())
    return cases
