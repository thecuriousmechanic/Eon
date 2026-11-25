//! One labeled module and its reviewed counterpart. Neither is executed.
use eon_cyber::{
    cpg::Lang,
    defense::{
        Assessment, Gap,
        taint_auditor::{Rule, review_source},
    },
    remediation::patch_isolated_python_token_check as repair,
};

const BEFORE: &str = include_str!("../../../eon-cyber/tests/fixtures/token_check/manual.py");
const AFTER: &str = include_str!("../../../eon-cyber/tests/fixtures/token_check/hardened.py");

#[test]
fn token_check_detection_repair_and_negative_controls_are_source_only() {
    let public = BEFORE.replace("verify_auth_token", "same_public_id");
    let fix = repair(BEFORE).unwrap().expect("expected isolated repair");
    assert!(fix.patched_source() == AFTER, "not the reviewed repair");
    for (source, positive) in [
        (BEFORE, true),
        (fix.patched_source(), false),
        (&public, false),
    ] {
        let report = review_source(Lang::Python, source);
        let check = report
            .checks
            .iter()
            .find(|c| c.rule == Rule::EarlyTokenMismatch)
            .unwrap();
        if positive {
            let span = &fix.flow.branch_condition;
            assert!(&source[span.clone()] == "presented[i] != expected[i]");
            assert_eq!(check.evidence, Assessment::Observed(span.clone()));
        } else {
            assert_eq!(check.evidence, Assessment::Unverified(Gap::OutsideRule));
            assert!(repair(source).unwrap().is_none());
        }
        let diagnostics = format!("{report:?} {fix:?}");
        let markers = [
            "presented",
            "verify_auth_token",
            "same_public_id",
            "return False",
        ];
        for marker in markers {
            assert!(!diagnostics.contains(marker), "source in diagnostic");
        }
    }
}
