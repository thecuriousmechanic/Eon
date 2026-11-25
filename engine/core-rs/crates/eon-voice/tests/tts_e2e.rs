//! End-to-end TTS: the pinned Kokoro-82M int8 model and the af_heart voice, with espeak-ng for phonemes.
//!
//! The check is a round trip. The synthesized speech is resampled to 16 kHz and transcribed by the pinned Zipformer
//! ASR. Enough content words must come back, so the test fails on silence or on speech that is not intelligible.

use std::path::PathBuf;
use std::time::Instant;

#[path = "common/kokoro_release.rs"]
mod kokoro_release;
#[cfg(target_os = "linux")]
#[path = "common/private_monitor.rs"]
mod private_monitor;

use eon_voice::asr::ZipformerAsr;
use eon_voice::resample::resample_mono;
use eon_voice::tts::{KokoroTts, SAMPLE_RATE_HZ};
use eon_voice::{
    LeaseId,
    barge_in::BargeIn,
    commitment::{Bridge, BridgeCache, CommitmentGate},
};

fn repo_models() -> PathBuf {
    if let Some(path) = std::env::var_os("EON_VOICE_MODELS") {
        return path.into();
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../perception/models")
}

#[test]
fn synthesized_speech_is_intelligible_to_the_pinned_asr() {
    let models = repo_models();
    let tts_model = models.join("tts/kokoro-v1.0-int8.onnx");
    let voice = models.join("tts/voices/af_heart.bin");
    assert!(
        tts_model.exists() && voice.exists(),
        "missing Kokoro assets; run: python3 scripts/download_models.py --ids kokoro-v1.0-int8 kokoro-v1.0-voice-af_heart"
    );
    let tts = KokoroTts::load(&tts_model, &voice).expect("pinned Kokoro model must load");
    check_intelligibility(tts);
}

fn check_intelligibility(mut tts: KokoroTts) {
    let start = Instant::now();
    let speech = tts
        .synthesize("Hello world, this is a test of the voice.", 1.0)
        .expect("synthesis must succeed");

    let seconds = speech.len() as f32 / SAMPLE_RATE_HZ as f32;
    let synthesis = start.elapsed();
    eprintln!(
        "Kokoro waveform={seconds:.3}s synthesis={synthesis:?} RTF={:.3}",
        synthesis.as_secs_f32() / seconds
    );
    assert!(seconds > 1.5, "speech is only {seconds:.2} s");
    let peak = speech.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.05, "speech is silent (peak {peak})");

    let at_16k = resample_mono(&speech, SAMPLE_RATE_HZ, 16_000).expect("resampling must succeed");
    let asr_model = repo_models().join("asr");
    let mut asr = ZipformerAsr::load(&asr_model).expect("pinned Zipformer models must load");
    let transcript = asr
        .transcribe(&at_16k)
        .expect("transcription must succeed")
        .to_uppercase();

    let content_words = ["HELLO", "WORLD", "THIS", "TEST", "VOICE"];
    let found = content_words
        .iter()
        .filter(|w| transcript.split_whitespace().any(|token| token == **w))
        .count();
    eprintln!("ROUND_TRIP transcript={transcript:?} found={found}");
    assert!(found >= 3, "round trip recovered {found} of 5 words: {transcript:?}");
}

#[test]
#[ignore = "requires explicitly selected and hash-checked upstream release graph; not the pinned graph test"]
fn alternate_release_is_intelligible_to_the_pinned_asr() {
    check_intelligibility(kokoro_release::load(&repo_models()));
}

#[test]
fn streaming_delivers_the_first_clause_before_the_reply_is_finished() {
    let models = repo_models();
    let mut tts = KokoroTts::load(
        models.join("tts/kokoro-v1.0-int8.onnx"),
        models.join("tts/voices/af_heart.bin"),
    )
    .expect("pinned Kokoro model must load");
    let mut callbacks = 0usize;
    let mut total_samples = 0usize;
    let timing = tts
        .synthesize_streaming(
            "Affirmative. The report is ready, and the tests all passed. Shall I summarize it now?",
            1.0,
            |samples| {
                callbacks += 1;
                total_samples += samples.len();
            },
        )
        .expect("streaming synthesis must succeed");

    assert_eq!(timing.clauses, 4, "four clauses: commas and full stops both split");
    assert_eq!(callbacks, 4, "one callback per clause");
    let seconds = total_samples as f32 / SAMPLE_RATE_HZ as f32;
    assert!(seconds > 2.0, "streamed audio is only {seconds:.2} s");
    assert!(
        timing.first_audio < timing.total.mul_f32(0.7),
        "first audio at {:?} is not clearly before the full reply at {:?}",
        timing.first_audio,
        timing.total
    );
}

#[test]
fn cached_bridge_and_committed_clause_meet_synthesis_budgets() {
    let models = repo_models();
    let tts = KokoroTts::load(
        models.join("tts/kokoro-v1.0-int8.onnx"),
        models.join("tts/voices/af_heart.bin"),
    )
    .unwrap();
    check_synthesis_budgets(tts);
}

#[test]
#[ignore = "requires explicitly selected and hash-checked upstream release graph; uses the same fixed budgets"]
fn alternate_release_cached_bridge_and_committed_clause_meet_synthesis_budgets() {
    check_synthesis_budgets(kokoro_release::load(&repo_models()));
}

fn check_synthesis_budgets(mut tts: KokoroTts) {
    let cache = BridgeCache::warm(&mut tts, 1.0).unwrap();
    let start = std::time::Instant::now();
    let mut gate = CommitmentGate::new(1, BargeIn::new());
    gate.queue("The test report is ready.").unwrap();
    let bridge = gate.bridge(&cache, Bridge::Report).unwrap();
    let first = start.elapsed();
    assert!(bridge.iter().any(|s| s.abs() > 0.05));
    assert!(gate.synthesize_next(&mut tts, 1.0).unwrap().is_none());
    gate.lease_acquired(1, LeaseId::new());
    let synthesis = std::time::Instant::now();
    let clause = gate.synthesize_next(&mut tts, 1.0).unwrap().unwrap();
    let rtf = synthesis.elapsed().as_secs_f32() / (clause.len() as f32 / SAMPLE_RATE_HZ as f32);
    eprintln!("Kokoro cached-prefix available={first:?}, uncached clause RTF={rtf:.3}");
    // Software availability only, not endpoint-to-speaker TTFA.
    assert!(first < std::time::Duration::from_millis(170));
    assert!(rtf < 0.15);
}

/// Exercise the ordinary text (not PreparedClause) renderer with a real
/// Kokoro instance. Only the child test process sees the fake espeak on PATH.
#[cfg(target_os = "linux")]
#[test]
fn revoking_plain_text_interrupts_stalled_g2p_without_publishing_pcm() {
    use std::{fs, os::unix::fs::DirBuilderExt, os::unix::fs::PermissionsExt, process::Command, time::Duration};

    use eon_voice::{commitment::ReplyRenderer, ring::pcm_ring};

    const FIXTURE: &str = "EON_TEST_STALLED_G2P";
    let Some(marker) = std::env::var_os(FIXTURE) else {
        let exe = std::env::current_exe().unwrap();
        let dir = exe
            .parent()
            .unwrap()
            .join(format!("stalled-g2p-{}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let fixture = dir.join("espeak-ng");
        let marker = dir.join("started");
        fs::write(
            &fixture,
            "#!/bin/sh\n: > \"$EON_TEST_STALLED_G2P\"\nexec /bin/sleep 10\n",
        )
        .unwrap();
        fs::set_permissions(fixture, fs::Permissions::from_mode(0o700)).unwrap();
        let child = Command::new(exe)
            .args([
                "--exact",
                "revoking_plain_text_interrupts_stalled_g2p_without_publishing_pcm",
                "--nocapture",
            ])
            .env("PATH", &dir)
            .env(FIXTURE, &marker)
            .output();
        let started = marker.is_file(); // --exact with zero matching tests also exits successfully.
        fs::remove_dir_all(dir).unwrap();
        let child = child.unwrap();
        assert!(
            started && child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        return;
    };

    let models = repo_models();
    let mut tts = KokoroTts::load(
        models.join("tts/kokoro-v1.0-int8.onnx"),
        models.join("tts/voices/af_heart.bin"),
    )
    .unwrap();
    let (tx, rx) = pcm_ring(64);
    let mut renderer = ReplyRenderer::new(1, BargeIn::new(), tx, SAMPLE_RATE_HZ).unwrap();
    renderer.queue("The report is ready.").unwrap();
    renderer.lease_acquired(1, LeaseId::new());
    let revoke = renderer.revocation();
    let observer = std::thread::spawn(move || {
        let start = Instant::now();
        while !std::path::Path::new(&marker).is_file() {
            assert!(start.elapsed() < Duration::from_secs(3), "espeak fixture did not start");
            std::thread::sleep(Duration::from_millis(2));
        }
        let interrupted = Instant::now();
        revoke.invalidate(1);
        interrupted
    });
    assert_eq!(renderer.pump(&mut tts, 1.0).unwrap(), 0);
    assert!(
        observer.join().unwrap().elapsed() < Duration::from_secs(1),
        "fell back to deadline"
    );
    assert_eq!(rx.available(), 0);
}

/// Output only, never opens a microphone. Start a private virtual sink explicitly.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires private virtual output, espeak-ng and the verified upstream release graph"]
fn alternate_release_prefix_and_claim_reach_the_cpal_render_reference() {
    use eon_voice::commitment::ReplyRenderer;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    assert_eq!(std::env::var("EON_VOICE_TEST_PRIVATE_OUTPUT").as_deref(), Ok("1"));
    private_monitor::require_null_sink();
    let mut tts = kokoro_release::load(&repo_models());
    let (mut worker, device, mut reference) = ReplyRenderer::open_default(1).unwrap();
    let rate = device.sample_rate;
    let cache = BridgeCache::warm_for_output(&mut tts, 1.0, rate).unwrap();
    reference.flush(); // Exclude startup/warm-up silence and its reference accounting.
    let baseline_drops = reference.dropped();
    let start = Instant::now();
    worker.queue("The test report is ready.").unwrap();
    let prefix = worker.start_bridge(&cache, Bridge::Report).unwrap();
    assert!(prefix > 0);
    let mut buf = [0.; 1024];
    let mut played = Vec::new();
    while start.elapsed() < Duration::from_secs(2) {
        let n = reference.pop_slice(&mut buf);
        played.extend_from_slice(&buf[..n]);
        if buf[..n].iter().any(|s| s.abs() > 0.01) {
            break;
        }

        std::thread::sleep(Duration::from_micros(500));
    }
    let first = start.elapsed();
    assert!(played.iter().any(|s| s.abs() > 0.01), "no rendered prefix");
    worker.lease_acquired(1, LeaseId::new());
    let mut written = worker.pump(&mut tts, 1.0).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let n = reference.pop_slice(&mut buf);
        played.extend_from_slice(&buf[..n]);
        written += worker.pump(&mut tts, 1.0).unwrap();
        if device.counters.frames.load(Ordering::Acquire) as usize == prefix + written {
            // All offered audio has been consumed; poll again in case a partial clause is still staged.
            let tail = worker.pump(&mut tts, 1.0).unwrap();
            written += tail;
            if tail == 0 {
                break;
            }
        }
        assert!(Instant::now() < deadline, "render stalled");
        std::thread::sleep(Duration::from_millis(1));
    }
    while reference.available() != 0 {
        let n = reference.pop_slice(&mut buf);
        played.extend_from_slice(&buf[..n]);
    }
    assert_eq!(device.counters.driver_errors.load(Ordering::Acquire), 0);
    assert_eq!(reference.dropped(), baseline_drops, "post-render reference overran");
    let sample_16k = resample_mono(&played, rate, 16_000).unwrap();
    let transcript = ZipformerAsr::load(repo_models().join("asr"))
        .unwrap()
        .transcribe(&sample_16k)
        .unwrap();
    eprintln!(
        "virtual CPAL {rate}Hz: first observed non-silent reference={first:?}, \
         first write={prefix}, later submitted={written}, rendered transcript={transcript:?}"
    );
    let transcript = transcript.to_uppercase();
    let mut words = transcript.split_whitespace();
    assert_eq!(words.next(), Some("REPORT"), "the rendered prefix must be intelligible");
    assert!(words.any(|word| word == "TEST") && words.any(|word| word == "READY"));
    assert!(
        first < Duration::from_millis(170),
        "cached prefix to virtual callback, not acoustic TTFA"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn private_output_flags_alone_do_not_enable_playback() {
    // Exercise the actual entry point in a child, before model loading or CPAL.
    // Empty ALSA config, nonexistent model and socket: no host fallback in this check.
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "alternate_release_prefix_and_claim_reach_the_cpal_render_reference",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("EON_VOICE_TEST_PRIVATE_OUTPUT", "1")
        .env("EON_VOICE_TEST_VIRTUAL_MONITOR", "1")
        .env("ALSA_CONFIG_PATH", "/dev/null")
        .env("PULSE_SERVER", "unix:/dev/null/no-server")
        .env("EON_KOKORO_RELEASE_INT8", "/dev/null/no-model")
        .output()
        .unwrap();
    assert!(!child.status.success());
    assert!(String::from_utf8_lossy(&child.stderr).contains("ALSA default must route exclusively"));
}
