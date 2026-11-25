use std::process::Command;

use eon_cyber::remediation::patch_isolated_python_token_check as patch;

const MANUAL: &str = include_str!("fixtures/token_check/manual.py");
const HARDENED: &str = include_str!("fixtures/token_check/hardened.py");

#[test]
fn direct_parameter_flow_is_remediated_once() {
    let fix = patch(MANUAL).unwrap().expect("labeled manual comparison");
    assert_eq!(&MANUAL[fix.flow.parameter.clone()], "presented");
    assert_eq!(&MANUAL[fix.flow.indexed_read.clone()], "presented[i]");
    assert_eq!(
        &MANUAL[fix.flow.branch_condition.clone()],
        "presented[i] != expected[i]"
    );
    assert_eq!(fix.patched_source(), HARDENED);
    assert!(patch(HARDENED).unwrap().is_none());
}

#[test]
fn offsets_preserve_unedited_bytes_and_diagnostics_do_not_echo_source() {
    let variant = |s: &str| {
        s.replace("presented", "private_fixture_token")
            .replace("expected", "stored_token")
            .replace("for i ", "for idx ")
            .replace("[i]", "[idx]")
            .replace('\n', "  \r\n")
    };
    let source = variant(MANUAL);
    let fix = patch(&source).unwrap().unwrap();
    assert_eq!(fix.patched_source(), variant(HARDENED));
    assert!(fix.patched_source().starts_with(&source[..fix.replaced.start]));
    assert!(fix.patched_source().ends_with(&source[fix.replaced.end..]));
    assert!(!format!("{fix:?}").contains("private_fixture_token"));
}

#[test]
fn false_positives_and_unproven_preconditions_are_left_alone() {
    for (label, old, new) in [
        ("public comparator", "verify_auth_token", "same_public_id"),
        ("no stdlib binding", "import hmac", ""),
        ("different import", "import hmac", "import other as hmac"),
        ("rebound module", "import hmac", "import hmac\nhmac = other"),
        ("shadowed builtin", "import hmac", "import hmac\nlen = custom_length"),
        ("shadowed primitive", "presented", "hmac"),
        ("shadowed builtin parameter", "expected", "range"),
        ("aliased parameters", "expected", "presented"),
        ("decorator", "def verify_auth_token", "@wrapper\ndef verify_auth_token"),
        ("body extension", "    for i", "    consume(expected)\n    for i"),
        (
            "subclasses possible",
            "type(presented) is not bytes",
            "not isinstance(presented, bytes)",
        ),
        ("wrong type", "type(expected) is not bytes", "type(expected) is not str"),
        ("missing length guard", "len(expected) != 32", "False"),
        (
            "only annotations",
            "type(presented) is not bytes or type(expected) is not bytes",
            "False",
        ),
        (
            "guards do not dominate",
            "        return False\n    for",
            "        pass\n    for",
        ),
        ("prefix comparison", "range(32)", "range(31)"),
        ("other data", "expected[i]", "presented[i]"),
        ("different branch", "!= expected[i]", "== expected[i]"),
        ("fallthrough differs", "return True", "return False"),
        ("extra scope", "    return True", "    return True\n    hmac = other"),
        ("malformed", "in range(32):", "in range(32"),
        ("unexpected indentation", "import hmac", "    import hmac"),
        ("coding declaration", "import hmac", "# coding: latin1\nimport hmac"),
    ] {
        let candidate = MANUAL.replace(old, new);
        assert_ne!(candidate, MANUAL, "{label}");
        assert!(patch(&candidate).unwrap().is_none(), "{label}");
    }
    for decoy in [
        format!("'''{MANUAL}'''"),
        MANUAL.lines().map(|l| format!("# {l}\n")).collect(),
    ] {
        assert!(patch(&decoy).unwrap().is_none());
    }
}

#[test]
fn generated_source_matches_the_semantically_tested_counterpart() {
    assert_eq!(patch(MANUAL).unwrap().unwrap().patched_source(), HARDENED);
    // Execute only the checked-in hardened fixture + independent value oracle, never
    // the analyzed text. Isolated mode excludes the project and user site from imports.
    let status = Command::new("python3")
        .args([
            "-I",
            "-S",
            "-B",
            "-c",
            concat!(
                include_str!("fixtures/token_check/hardened.py"),
                "\n",
                include_str!("fixtures/token_check/semantics.py")
            ),
        ])
        .status()
        .expect("this regression requires a local CPython 3 interpreter");
    assert!(status.success(), "isolated semantics fixture failed: {status}");
}
