//! Real C ABI regression. Requires the verified local SDK, model and 0.wav;
//! missing assets are errors, never skips. No microphone or network needed.

use std::path::PathBuf;

use eon_voice::asr::{MAX_UTTERANCE_SAMPLES, PartialTranscript, ZipformerAsr};
use eon_voice::pre_cognition::IntentPreview;
use sha2::{Digest, Sha256};

mod common;

const REFERENCE: &str =
    "AFTER EARLY NIGHTFALL THE YELLOW LAMPS WOULD LIGHT UP HERE AND THERE THE SQUALID QUARTER OF THE BROTHELS";

fn asset(env: &str, default: &str) -> PathBuf {
    std::env::var_os(env)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(default))
}

fn model_dir() -> PathBuf {
    asset("EON_ZIPFORMER_DIR", "../../../../perception/models/asr")
}

fn recording() -> Vec<f32> {
    let path = asset("EON_ZIPFORMER_TEST_WAV_0", "tests/fixtures/0.wav");
    let digest = Sha256::digest(std::fs::read(&path).expect("install voice-test-zipformer-0"));
    assert_eq!(
        format!("{digest:x}"),
        "6bc58a4efdf20daac252b6b1502632601a71efe0308f6757dc1eda34891a7e4f"
    );
    common::read_pcm16_mono_16k(&path)
}

#[derive(Debug, PartialEq, Eq)]
struct Run {
    updates: Vec<(usize, PartialTranscript)>,
    transcript: String,
}

fn run(model: &mut ZipformerAsr, samples: &[f32], chunk_size: usize) -> Run {
    let mut stream = model.start_utterance().unwrap();
    let mut updates = Vec::new();
    let mut consumed = 0;
    for chunk in samples.chunks(chunk_size) {
        consumed += chunk.len();
        if let Some(partial) = stream.push(chunk).unwrap() {
            updates.push((consumed, partial));
        }
    }
    Run {
        updates,
        transcript: stream.finish().unwrap(),
    }
}

#[test]
fn asr_tests_sherpa_online_partials_intent_and_stream_isolation() {
    let mut model = ZipformerAsr::load(model_dir()).unwrap();
    let samples = recording();
    let first = run(&mut model, &samples, 320);
    let first_intent = first.updates.iter().find(|(_, p)| p.intent.is_some()).unwrap();
    eprintln!(
        "{} samples; {} updates; first {:?}; first intent {:?}",
        samples.len(),
        first.updates.len(),
        first.updates.first(),
        first_intent
    );
    assert_eq!(first.transcript, REFERENCE);
    assert!(first.updates.len() > 2);
    assert!(first.updates[0].0 < samples.len() / 2);
    assert_ne!(first.updates[0].1.text, first.transcript);
    assert!(first_intent.0 < samples.len() / 2);
    assert_eq!(first_intent.1.intent, Some(IntentPreview::OpenSpeech));
    assert!(first.updates.iter().all(|(_, p)| p.hint.is_none()));
    assert!(first.updates.windows(2).all(|p| p[0].1.text != p[1].1.text));

    // Drop a partially decoded utterance without finishing, then process silence.
    let mut cancelled = model.start_utterance().unwrap();
    assert!(cancelled.push(&samples[..samples.len() / 2]).unwrap().is_some());
    drop(cancelled);
    let silence = run(&mut model, &[0.0; 16_000], 317);
    assert!(silence.updates.is_empty());
    assert!(silence.transcript.is_empty());
    assert_eq!(run(&mut model, &samples, 320), first);
    assert_eq!(run(&mut model, &samples, 317).transcript, first.transcript);
}

#[test]
fn asr_tests_invalid_input_does_not_change_the_sherpa_stream() {
    let mut model = ZipformerAsr::load(model_dir()).unwrap();
    let samples = recording();
    let mut stream = model.start_utterance().unwrap();
    stream.push(&samples[..320]).unwrap();
    for invalid in [f32::NAN, f32::INFINITY, -1.01, 1.01] {
        assert!(stream.push(&[0.0, invalid]).is_err());
    }
    assert!(stream.push(&vec![0.0; MAX_UTTERANCE_SAMPLES - 319]).is_err());
    assert!(stream.push(&[]).unwrap().is_none());
    stream.push(&samples[320..]).unwrap();
    assert_eq!(stream.finish().unwrap(), REFERENCE);
}

#[cfg(feature = "onnx-speech")]
#[test]
fn asr_tests_both_online_backends_on_the_same_incoming_pcm() {
    let samples = recording();
    let direct = run(&mut ZipformerAsr::load_ort(model_dir()).unwrap(), &samples, 320);
    let sherpa = run(&mut ZipformerAsr::load(model_dir()).unwrap(), &samples, 320);
    assert_eq!(direct.transcript, REFERENCE);
    assert_eq!(sherpa.transcript, direct.transcript);
    // Compare every pre-EOF update, not just finish(). The trailing BPE word
    // can differ ("QUAR" vs "QUARTER"); compare completed words and intent.
    let before_eof = |r: &Run| {
        r.updates
            .iter()
            .filter(|(n, _)| *n < samples.len())
            .map(|(_, p)| (p.text.rsplit_once(' ').map(|(words, _)| words.to_owned()), p.intent))
            .collect::<Vec<_>>()
    };
    assert_eq!(before_eof(&sherpa), before_eof(&direct));
}
