"""Host regressions for the export check that runs before publishing the signed APK."""

from pathlib import Path
import re
import unittest

from verify_apk import check_exports


class BridgeExports(unittest.TestCase):
    def test_requires_every_udl_function_and_its_checksum(self):
        # Use the bridge contract, not the verifier's own allowlist, as the oracle.
        udl = Path(__file__).resolve().parents[2] / "core-rs/crates/eon-uniffi/src/eon_core.udl"
        source = re.sub(r"//[^\n]*", "", udl.read_text())
        namespace = re.search(r"\bnamespace\s+eon_uniffi\s*\{([^}]*)\}", source)
        functions = re.findall(r"\b(\w+)\s*\([^()]*\)\s*;", namespace[1])
        self.assertIn("validate_task_dag", functions)
        names = [f"uniffi_eon_uniffi_{kind}_func_{name}" for name in functions for kind in ("fn", "checksum")]
        lib = "libeon_uniffi.so"

        def symbols_without(missing=None):
            return "\n".join(f"{i}: 00001000 4 FUNC GLOBAL DEFAULT 12 {name}"
                             for i, name in enumerate(names, 1) if name != missing)

        check_exports(lib, lib, symbols_without())
        for missing in names:
            with self.subTest(missing=missing), self.assertRaisesRegex(ValueError, missing):
                check_exports(lib, lib, symbols_without(missing))

    def test_only_externally_visible_symbols_count(self):
        lib = "libjnidispatch.so"
        row = "1: 00001000 4 FUNC GLOBAL {} 12 JNI_OnLoad"
        for visibility in ("DEFAULT", "PROTECTED"):
            with self.subTest(visibility=visibility):
                check_exports(lib, lib, row.format(visibility))
        for visibility in ("HIDDEN", "INTERNAL"):
            with self.subTest(visibility=visibility), self.assertRaisesRegex(ValueError, "JNI_OnLoad"):
                check_exports(lib, lib, row.format(visibility))


if __name__ == "__main__":
    unittest.main()
