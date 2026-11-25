//! Opt-in Tier 1 gate. From the repository root, install **only** the locked P0
//! model, then run offline against the installed file (no inference service):
//!
//! ```sh
//! python3 scripts/download_models.py --ids qwen3.5-0.8b-q4_k_m
//! cargo test --locked --release --manifest-path engine/core-rs/Cargo.toml -p eon-cognition \
//!   --no-default-features --features router-native-gate --test router_native_gate -- --ignored --nocapture
//! ```
//!
//! The separate `completed_reflex_p95_under_20ms` test needs no model. Both
//! gates time `plan` through validated text, not first token or audible speech.
//! Tier 1 uses the production greedy Qwen speaker and bounded speech filter,
//! not the separate GBNF tool-call generator.
//! For a release deployed on the build host, `RUSTFLAGS="-C target-cpu=native"`
//! enables llama.cpp's host ISA kernels. Such binaries are not portable.

use std::fs::File;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use eon_cognition::backend::LocalModel;
use eon_cognition::persona::SpeechAct;
use eon_cognition::profile::ModelProfile;
use eon_cognition::router::{CognitiveRouter, Route, SpeechRequest, Workload};
use serde_json::Value;
use sha2::{Digest, Sha256};

const MODEL: &str = "qwen3.5-0.8b-q4_k_m";
const LOCK: &str = include_str!("../../../../../scripts/models.lock.json");

fn load_pinned_p0() -> LocalModel {
    let lock: Value = serde_json::from_str(LOCK).unwrap();
    let entry = lock["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == MODEL)
        .expect("P0 test model missing from lock");
    assert_eq!(entry["license"], "Apache-2.0", "review changed model terms");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .unwrap();
    let path = root.join(entry["dest"].as_str().unwrap());
    let size = entry["size_bytes"].as_u64().unwrap();
    let mut file = File::open(&path).unwrap_or_else(|e| {
        panic!(
            "cannot open locked model at {} ({e}); run scripts/download_models.py --ids {MODEL}",
            path.display()
        )
    });
    assert_eq!(file.metadata().unwrap().len(), size, "wrong GGUF size");
    let mut hash = Sha256::new();
    assert_eq!(io::copy(&mut file, &mut hash).unwrap(), size);
    let digest = format!("{:x}", hash.finalize());
    assert_eq!(
        digest,
        entry["sha256"].as_str().unwrap(),
        "wrong GGUF digest"
    );
    eprintln!("Verified {MODEL}: {size} bytes, SHA-256 {digest}");
    LocalModel::load(path).expect("verified P0 GGUF must load")
}

fn completed_turn(router: &CognitiveRouter<'_>, text: &str, reading: &str) -> Duration {
    let request = SpeechRequest {
        act: SpeechAct::Report,
        text,
        max_tokens: 32,
    };
    let start = Instant::now();
    let reply = router
        .plan(request, Workload::Interactive)
        .and_then(|plan| plan.execute())
        .expect("local model must reach EOG and pass speech validation");
    let elapsed = start.elapsed();
    assert_eq!(reply.route, Route::TacticalLocal);
    assert!(reply.backend_elapsed.is_some());
    let body = reply
        .speech
        .strip_prefix("Report: ")
        .expect("wrong speech act");
    assert!(!body.contains(['<', '>']));
    assert!(
        body.split(|c: char| !c.is_ascii_digit())
            .any(|n| n == reading),
        "validated report did not convey the requested reading"
    );
    elapsed
}

#[test]
#[ignore = "requires the digest-pinned P0 GGUF and a native release build"]
fn warm_tactical_p95_under_120ms() {
    let model = load_pinned_p0();
    let router = CognitiveRouter::for_profile(ModelProfile::P0, [Some(&model), None, None, None]);
    let cases = [
        (
            "Battery sensor: 84 percent. Report the reading using digits.",
            "84",
        ),
        (
            "Battery sensor: 37 percent. Report the reading using digits.",
            "37",
        ),
    ];
    for (text, reading) in cases {
        completed_turn(&router, text, reading);
    }

    let mut samples = Vec::with_capacity(40);
    for (text, reading) in cases.into_iter().cycle().take(40) {
        samples.push(completed_turn(&router, text, reading));
    }
    eprintln!("TacticalLocal complete turns (in order): {samples:?}");
    samples.sort_unstable();
    let quantile = |p: usize| samples[(p * samples.len()).div_ceil(100) - 1];
    let p95 = quantile(95);
    eprintln!(
        "TacticalLocal request to validated text: n={}, p50={:?}, p95={p95:?}, max={:?}",
        samples.len(),
        quantile(50),
        samples.last().unwrap()
    );
    assert!(p95 < Duration::from_millis(120), "warm Tier 1 p95: {p95:?}");
}
