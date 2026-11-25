//! End-to-end generation test: the pinned Qwen3.5-0.8B Q4_K_M model from `scripts/models.lock.json`, run through
//! llama.cpp with greedy decoding.

use std::path::PathBuf;
use std::time::Instant;

use eon_cognition::backend::LocalModel;
use eon_cognition::persona::SpeechAct;
use eon_cognition::router::{CognitiveRouter, Route, SpeechRequest, Workload};

fn model_path() -> PathBuf {
    if let Ok(path) = std::env::var("EON_QWEN_MODEL") {
        return PathBuf::from(path);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../perception/models/slm/Qwen3.5-0.8B-Q4_K_M.gguf")
}

fn load() -> LocalModel {
    let path = model_path();
    assert!(
        path.exists(),
        "missing {}; run: python3 scripts/download_models.py --ids qwen3.5-0.8b-q4_k_m",
        path.display()
    );
    LocalModel::load(path).expect("pinned model must load")
}

#[test]
fn generates_text_for_a_factual_prompt() {
    let model = load();
    let text = model.generate_greedy("The capital of France is", 16).unwrap();
    assert!(!text.trim().is_empty(), "model produced no text");
}

#[test]
fn greedy_generation_is_deterministic() {
    let model = load();
    let first = model.generate_greedy("Name a primary color:", 12).unwrap();
    let second = model.generate_greedy("Name a primary color:", 12).unwrap();
    assert_eq!(first, second);
}

#[test]
fn prompt_larger_than_the_context_window_is_refused() {
    let model = load();
    let huge = "word ".repeat(10_000);
    assert!(model.generate_greedy(&huge, 8).is_err());
}

#[test]
fn qwen_public_speech_uses_the_ciel_contract() {
    let model = load();
    // One output token cannot generate this report: the legacy caller takes the real reflex path.
    assert_eq!(
        model.generate_speech(SpeechAct::Report, "router status", 1).unwrap(),
        "Report: Local tactical backend configured."
    );
    let request = "The battery sensor reads 84 percent. Report this reading.";
    let router = CognitiveRouter {
        tactical_local: Some(&model),
        ..Default::default()
    };
    let start = Instant::now();
    let reply = router
        .plan(
            SpeechRequest {
                act: SpeechAct::Report,
                text: request,
                max_tokens: 96,
            },
            Workload::Interactive,
        )
        .unwrap()
        .execute()
        .unwrap();
    let validated = start.elapsed();
    assert_eq!(reply.route, Route::TacticalLocal);
    eprintln!(
        "preloaded local model, one text turn: selection={:?}, backend through EOG={:?}, \
         request to validated speech={validated:?} (not TTFT or audio latency)",
        reply.route_elapsed,
        reply.backend_elapsed.unwrap(),
    );
    let speech = reply.speech;
    assert!(speech.starts_with("Report: "));
    assert_eq!(speech.matches("Report:").count(), 1);
    assert!(!speech.contains(['<', '>']));
    assert!(model.generate_speech(SpeechAct::Report, request, 1).is_err());
}

#[test]
fn qwen_notice_generates_its_own_public_body() {
    let speech = load()
        .generate_speech(
            SpeechAct::Notice,
            "The library is closed. Notify the listener briefly.",
            48,
        )
        .unwrap();
    let body = speech.strip_prefix("Notice: ").expect("wrong speech act");
    assert!(body.to_lowercase().contains("closed"));
    assert!(!body.contains(['<', '>']));
}
