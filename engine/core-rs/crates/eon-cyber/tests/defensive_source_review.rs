use std::ops::Range;

use eon_cyber::cpg::Lang;
use eon_cyber::defense::{
    Assessment, Gap, MAX_SOURCE_BYTES,
    auto_patch::propose_checked_byte_lookup as propose,
    taint_auditor::{Rule, SourceReport, review_source},
};

const LOOKUP: &str = "fn read_sample(sample: &[u8], position: usize) -> Option<u8> { Some(unsafe { *sample.get_unchecked(position) }) }";

fn evidence(report: &SourceReport, rule: Rule) -> &Assessment<Range<usize>> {
    let check = report.checks.iter().find(|c| c.rule == rule).unwrap();
    &check.evidence
}

#[track_caller]
fn assert_gap(report: &SourceReport, rule: Rule, gap: Gap) {
    assert_eq!(evidence(report, rule), &Assessment::Unverified(gap));
}

fn first_span(source: &str, expression: &str) -> Assessment<Range<usize>> {
    let start = source.find(expression).unwrap();
    Assessment::Observed(start..start + expression.len())
}

#[test]
fn rust_observations_are_first_matching_byte_ranges_not_source() {
    let source = r#"
        // λ private_fixture
        fn inspect() {
            private_slice.get_unchecked_mut(/* position */ idx);
            another.get_unchecked(next);
            counter.store(1 + counter.load(order), /* ordering */ order);
        }
    "#;
    let report = review_source(Lang::Rust, source);
    assert_eq!(report.language, Lang::Rust);
    assert_eq!(
        evidence(&report, Rule::UncheckedIndex),
        &first_span(
            source,
            "private_slice.get_unchecked_mut(/* position */ idx)"
        )
    );
    assert_eq!(
        evidence(&report, Rule::SplitAtomicUpdate),
        &first_span(
            source,
            "counter.store(1 + counter.load(order), /* ordering */ order)"
        )
    );
    for rule in [Rule::InterpolatedQuery, Rule::EarlyTokenMismatch] {
        assert_gap(&report, rule, Gap::Unsupported);
    }
    let diagnostic = format!("{report:?}");
    for marker in [
        "private_fixture",
        "private_slice",
        "counter.load",
        "get_unchecked",
    ] {
        assert!(!diagnostic.contains(marker));
    }
}

#[test]
fn rust_does_not_join_unrelated_calls_or_read_decoys_as_calls() {
    let source = r#"
        fn inspect() {
            // bytes.get_unchecked_mut(idx)
            let _ = "bytes.get_unchecked(idx)";
            bytes.get(idx);
            bytes.get_unchecked();
            get_unchecked(idx);
            a.store(b.load(order) + 1, order);
            a.store(a.load() + 1, order);
            a.store(combine(a.load(order)), order);
            let prior = a.load(order);
            a.store(prior + 1, order);
        }
    "#;
    let report = review_source(Lang::Rust, source);
    for rule in [Rule::UncheckedIndex, Rule::SplitAtomicUpdate] {
        assert_gap(&report, rule, Gap::OutsideRule);
    }
}

#[test]
fn python_observes_only_a_direct_interpolated_first_argument() {
    let decoys = r#"
# cursor.execute(f"{private_label}")
example = 'cursor.execute(f"{private_label}")'
cursor.execute(f"{{literal}}")
cursor.execute("fixed", (f"{private_label}",))
cursor.execute(f"{label}" for label in labels)
cursor.execute(query=f"{private_label}")
recorder.note(f"{private_label}")
execute(f"{private_label}")
"#;
    let report = review_source(Lang::Python, decoys);
    assert_gap(&report, Rule::InterpolatedQuery, Gap::OutsideRule);
    for method in ["execute", "executemany"] {
        let call = format!("cursor.{method}(\n    # π\n    f\"{{private_label}}\")");
        let source = format!("{decoys}\n{call}\n{call}");
        let report = review_source(Lang::Python, &source);
        assert_eq!(
            evidence(&report, Rule::InterpolatedQuery),
            &first_span(&source, &call)
        );
        assert_gap(&report, Rule::UncheckedIndex, Gap::Unsupported);
        assert!(!format!("{report:?}").contains("private_label"));
    }
}

#[test]
fn python_templates_do_not_hide_the_first_formatted_query() {
    let templates =
        "db.execute(t'{first}')\ndb.execute(RT\"{second}\")\ndb.execute(Tr'''{third}''')\n";
    let report = review_source(Lang::Python, templates);
    assert_gap(&report, Rule::InterpolatedQuery, Gap::OutsideRule);
    for prefix in ["f", "F", "rf", "Rf", "fr", "FR"] {
        let call = format!("db.execute({prefix}'''{{label}}''')");
        let source = format!("{templates}{call}");
        let report = review_source(Lang::Python, &source);
        assert_eq!(
            evidence(&report, Rule::InterpolatedQuery),
            &first_span(&source, &call)
        );
    }
}

#[test]
fn malformed_python_layout_suppresses_evidence_for_the_whole_unit() {
    let call = "db.execute(f'{label}')";
    for (label, source) in [
        ("initial indent", format!("  {call}")),
        ("unexpected indent", format!("{call}\n  value = 0")),
        ("empty at EOF", format!("{call}\ndef helper():\n")),
        (
            "comment-only suite",
            format!("if ready:\n    # note\n{call}"),
        ),
        (
            "missing nested suite",
            format!("if ready:\n    if busy:\n    {call}"),
        ),
        (
            "inconsistent dedent",
            format!("if ready:\n    {call}\n  other()"),
        ),
        (
            "misaligned clause",
            format!("if ready:\n    {call}\n  else:\n    other()"),
        ),
    ] {
        let report = review_source(Lang::Python, &source);
        for check in report.checks {
            assert_eq!(
                check.evidence,
                Assessment::Unverified(Gap::InvalidInput),
                "{label}"
            );
        }
    }
}

#[test]
fn python_layouts_outside_the_space_indented_profile_are_unverified() {
    for source in [
        "if ready: db.execute(f'{label}')",
        "if ready:\n\tdb.execute(f'{label}')",
        "pass; db.execute(f'{label}')",
        "db.execute(\\\n f'{label}')",
    ] {
        let report = review_source(Lang::Python, source);
        assert_gap(&report, Rule::InterpolatedQuery, Gap::OutsideRule);
    }
}

#[test]
fn python_space_indented_suites_allow_comments_and_continuation_lines() {
    let call = "db.execute(\n        f'{label}'\n    )";
    let source = format!("def run():\n  if ready:\n    pass\n  else:\n # note\n    {call}\n");
    let report = review_source(Lang::Python, &source);
    assert_eq!(
        evidence(&report, Rule::InterpolatedQuery),
        &first_span(&source, call)
    );
}

#[test]
fn token_review_reports_the_guarded_branch_location_only() {
    let manual = include_str!("fixtures/token_check/manual.py")
        .replace("presented", "private_fixture_token");
    let report = review_source(Lang::Python, &manual);
    assert_eq!(
        evidence(&report, Rule::EarlyTokenMismatch),
        &first_span(&manual, "private_fixture_token[i] != expected[i]")
    );
    assert!(!format!("{report:?}").contains("private_fixture_token"));
    let hardened = include_str!("fixtures/token_check/hardened.py");
    let report = review_source(Lang::Python, hardened);
    assert_gap(&report, Rule::EarlyTokenMismatch, Gap::OutsideRule);
}

#[test]
fn incomplete_reviews_do_not_publish_partial_evidence() {
    let at_limit = format!("{LOOKUP}{}", " ".repeat(MAX_SOURCE_BYTES - LOOKUP.len()));
    assert!(matches!(propose(&at_limit), Assessment::Observed(_)));
    let too_large = at_limit + " ";
    let too_many_nodes = format!("c.execute(f'{{label}}')\n{}", "x=0\n".repeat(2000));
    assert!(too_many_nodes.len() < MAX_SOURCE_BYTES);
    for (lang, source, gap) in [
        (Lang::Rust, "fn incomplete(", Gap::InvalidInput),
        (Lang::Python, "def incomplete(", Gap::InvalidInput),
        (Lang::Rust, &too_large, Gap::LimitExceeded),
        (Lang::Python, &too_many_nodes, Gap::LimitExceeded),
    ] {
        let report = review_source(lang, source);
        for check in report.checks {
            assert_eq!(check.evidence, Assessment::Unverified(gap));
        }
    }
    assert!(matches!(
        propose(&too_large),
        Assessment::Unverified(Gap::LimitExceeded)
    ));
    assert!(matches!(
        propose("fn incomplete("),
        Assessment::Unverified(Gap::InvalidInput)
    ));
}

#[test]
fn checked_lookup_replaces_only_the_tail_and_is_not_reapplied() {
    let signature =
        "\r\nfn sample_read ( private_slice : &[u8],\tidx_2 :usize)->Option< u8 >{\r\n\t";
    let tail = "Some (unsafe{*private_slice . get_unchecked(\nidx_2 )})";
    let suffix = "\r\n}\t\r\n";
    let source = format!("{signature}{tail}{suffix}");
    let Assessment::Observed(patch) = propose(&source) else {
        panic!("expected isolated byte lookup proposal");
    };
    assert_eq!(
        patch.replaced,
        signature.len()..signature.len() + tail.len()
    );
    assert_eq!(
        patch.patched_source(),
        format!("{signature}private_slice.get(idx_2).copied(){suffix}")
    );
    assert!(!format!("{patch:?}").contains("private_slice"));
    let report = review_source(Lang::Rust, patch.patched_source());
    assert_gap(&report, Rule::UncheckedIndex, Gap::OutsideRule);
    assert!(matches!(
        propose(patch.patched_source()),
        Assessment::Unverified(Gap::OutsideRule)
    ));
}

#[test]
fn proposal_declines_unproven_contracts_even_when_review_observes_a_call() {
    assert!(matches!(propose(LOOKUP), Assessment::Observed(_)));
    for (label, old, new) in [
        ("public API", "fn read_sample", "pub fn read_sample"),
        ("attribute", "fn read_sample", "#[inline] fn read_sample"),
        (
            "other item",
            "fn read_sample",
            "type Option<T> = T;\nfn read_sample",
        ),
        ("other parameter type", "[u8]", "[u16]"),
        ("mutable slice", "&[u8]", "&mut [u8]"),
        ("other index type", ": usize", ": u32"),
        ("duplicate binding", "position", "sample"),
        ("other return contract", "Option<u8>", "Result<u8, ()>"),
        ("other receiver", "*sample.", "*other."),
        ("expression as index", "(position)", "(select(position))"),
        ("other binding", "(position)", "(sample)"),
        ("other method", "get_unchecked", "get_unchecked_mut"),
        ("extra statement", "{ Some", "{ record(); Some"),
        ("comment would be lost", "unsafe {", "unsafe { /* note */"),
    ] {
        let source = LOOKUP.replace(old, new);
        let report = review_source(Lang::Rust, &source);
        let unchecked = evidence(&report, Rule::UncheckedIndex);
        assert!(matches!(unchecked, Assessment::Observed(_)), "{label}");
        assert!(
            matches!(propose(&source), Assessment::Unverified(Gap::OutsideRule)),
            "{label}"
        );
    }
    let reserved = LOOKUP.replace("position", "gen");
    assert!(matches!(propose(&reserved), Assessment::Unverified(_)));
}
