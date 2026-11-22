//! Explicit full acceptance target. The normal kernel suite runs `eval_components` instead.
use std::{env, path::Path, process::Command};

#[test]
fn complete_roadmap_evaluation() {
    let runner =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../scripts/eval/acceptance.py");
    let mut command = Command::new("python3");
    command.arg("-B").arg(runner);
    if let Some(directory) = env::var_os("EON_PHYSICAL_SESSION") {
        assert!(
            Path::new(&directory).is_absolute(),
            "EON_PHYSICAL_SESSION must be absolute"
        );
        command.arg("--physical-session").arg(directory);
    }
    let status = command
        .status()
        .expect("could not start the acceptance runner");
    assert!(
        status.success(),
        "full acceptance did not pass ({status}); see per-gate results above"
    );
}
