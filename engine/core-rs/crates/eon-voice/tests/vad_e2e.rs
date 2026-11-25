//! End-to-end VAD test: pinned Silero v5 ONNX model over a real 16 kHz speech recording.
//!
//! The model comes from `scripts/models.lock.json` (`silero-vad-v5`). Put it at the repo path below, or set
//! `EON_SILERO_MODEL`. The WAV fixture is the upstream `test_wavs/en.wav` (sha256 eb1eb008...).

use std::path::PathBuf;

mod common;

use common::read_pcm16_mono_16k;
use eon_voice::endpoint::{Completeness, Endpointer};
use eon_voice::geometry::VAD_FRAME_SAMPLES;
use eon_voice::vad::SileroVad;

fn model_path() -> PathBuf {
    if let Ok(path) = std::env::var("EON_SILERO_MODEL") {
        return PathBuf::from(path);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../perception/models/vad/silero_vad.onnx")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn load_model() -> SileroVad {
    let path = model_path();
    assert!(
        path.exists(),
        "missing {}; run: python3 scripts/download_models.py --ids silero-vad-v5",
        path.display()
    );
    SileroVad::load(path).expect("pinned Silero model must load")
}

#[test]
fn callback_sized_fragments_of_recorded_speech_reach_the_endpoint_at_device_rate() {
    use eon_voice::resample::{CaptureFrames, resample_mono};
    use eon_voice::ring::pcm_ring;
    use eon_voice::vad::{VadEndpoint, VadStep};

    let speech = read_pcm16_mono_16k(&fixture("en.wav"));
    let mut device = resample_mono(&speech, 16_000, 44_100).unwrap();
    device.resize(device.len() + 2 * 44_100, 0.0);
    let (mut tx, rx) = pcm_ring(4096);
    let mut input = CaptureFrames::new(rx, 44_100).unwrap();
    let mut endpoint = VadEndpoint::new(load_model());
    let mut endpoints = Vec::new();
    for callback in device.chunks(193) {
        assert_eq!(tx.push_slice(callback), callback.len());
        while let VadStep::Processed(event) = endpoint.poll_capture(&mut input, "").unwrap() {
            endpoints.extend(event);
        }
    }
    assert!(!endpoints.is_empty());
    assert!(endpoints.iter().all(|e| e.silence_ms == 800));
}

#[test]
fn vad_tests_30ms_device_windows_are_never_padded_or_scored_twice() {
    use eon_voice::resample::{CaptureFrames, resample_mono};
    use eon_voice::ring::pcm_ring;
    use eon_voice::vad::{VadEndpoint, VadStep};

    let speech = read_pcm16_mono_16k(&fixture("en.wav"));
    // Include both supported bounds, common USB/Bluetooth rates and a
    // non-integer ratio. Only the continuous resampler sees the device rate.
    let oversized_partial = format!("{}stop", " ".repeat(8192));
    for (hz, partial, expected_ms) in [
        (8_000, "", 800),
        (16_000, "stop", 320),
        (44_100, "run tests and", 1152),
        (48_000, "", 800),
        (96_000, "stop", 320),
        (192_000, oversized_partial.as_str(), 1152),
    ] {
        let mut source = resample_mono(&speech[19_200..35_200], 16_000, hz).unwrap();
        source.resize(5 * hz as usize / 2, 0.0);
        let (mut tx, rx) = pcm_ring(16_384);
        let mut input = CaptureFrames::new(rx, hz).unwrap();
        let mut vad = VadEndpoint::new(load_model());
        let (mut frames, mut ends) = (0, 0);
        for window in source.chunks(hz as usize * 30 / 1000) {
            assert_eq!(tx.push_slice(window), window.len());
            if let VadStep::Processed(event) = vad.poll_capture(&mut input, partial).unwrap() {
                frames += 1;
                if let Some(end) = event {
                    assert_eq!(end.silence_ms, expected_ms);
                    ends += 1;
                }
            }
            assert_eq!(vad.poll_capture(&mut input, partial).unwrap(), VadStep::Waiting);
        }
        assert!(
            (77..=78).contains(&frames),
            "{hz} Hz returned {frames} frames for 2.5 seconds"
        );
        assert_eq!(ends, 1, "{hz} Hz failed to close the recorded turn");
    }
}

#[test]
fn vad_tests_invalid_stream_geometry_and_gaps_are_not_silence() {
    use eon_voice::resample::CaptureFrames;
    use eon_voice::ring::pcm_ring;
    use eon_voice::vad::{VadEndpoint, VadStep};
    for rate in [0, 7_999, 192_001] {
        assert!(CaptureFrames::<VAD_FRAME_SAMPLES>::new(pcm_ring(8192).1, rate).is_err());
    }
    assert!(CaptureFrames::<VAD_FRAME_SAMPLES>::new(pcm_ring(479).1, 16_000).is_err());
    let mut vad = VadEndpoint::new(load_model());
    for corrupt in [false, true] {
        let (mut tx, rx) = pcm_ring(VAD_FRAME_SAMPLES);
        let mut input = CaptureFrames::new(rx, 16_000).unwrap();
        assert_eq!(tx.push_slice(&[0.0; 480]), 480);
        assert_eq!(vad.poll_capture(&mut input, "").unwrap(), VadStep::Waiting);
        if corrupt {
            assert_eq!(tx.push_slice(&[f32::NAN; 32]), 32);
        } else {
            assert_eq!(tx.push_slice(&[0.0; 33]), 32, "must have lost one sample");
        }
        assert!(vad.poll_capture(&mut input, "").is_err());
        assert!(vad.poll_capture(&mut input, "").is_err());
    }
}

#[cfg(all(feature = "native", target_os = "linux"))]
#[path = "common/private_monitor.rs"]
mod private_monitor;

#[cfg(all(feature = "native", target_os = "linux"))]
#[test]
#[ignore = "use tests/vad_private_monitor.py --private-monitor; never opens an ambient microphone"]
fn vad_tests_private_monitor() {
    use eon_voice::barge_in::BargeIn;
    use eon_voice::device::{capture_default_frames, play_default_output};
    use eon_voice::resample::resample_mono;
    use eon_voice::ring::pcm_ring;
    use eon_voice::vad::CaptureVad;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    private_monitor::require_null_sink();

    // Repeat the same second of the digest-pinned recording with 1.5 s of
    // explicit digital silence after each copy. No microphone is a fixture.
    let speech = read_pcm16_mono_16k(&fixture("en.wav"));
    let vad = load_model();
    let (mut tx, rx) = pcm_ring(1 << 21);
    let output = play_default_output(rx, BargeIn::new()).unwrap();
    let mut turn = resample_mono(&speech[19_200..35_200], 16_000, output.sample_rate).unwrap();
    turn.resize(turn.len() + output.sample_rate as usize * 3 / 2, 0.0);
    let source = turn.repeat(2);
    let input = capture_default_frames().unwrap();
    let input_rate = input.1.sample_rate;
    let counters = input.1.counters.clone();
    let mut stream = CaptureVad::new(vad, input);
    let start = Instant::now();
    let mut inference = Vec::new();
    let mut last_speech = 0;
    let mut last_signal = None;
    let mut observed = Vec::new();
    let mut strong = 0;
    let mut pauses = Vec::new();
    let mut ends = Vec::new();
    loop {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "timed out after {} scored frames; pauses={pauses:?}, ends={ends:?}",
            inference.len()
        );
        let Some(score) = stream.poll("").unwrap() else {
            std::thread::sleep(Duration::from_micros(250));
            continue;
        };
        inference.push(score.inference_time);
        let i = inference.len();
        let now = Instant::now();
        if score.rms > 0.0032 {
            last_signal = Some((i, now));
        }
        if i <= 10 {
            assert!(last_signal.is_none() && score.probability < 0.35);
            assert!(score.acoustic_pause.is_none() && score.turn_end.is_none());
            if i == 10 {
                assert_eq!(tx.push_slice(&source), source.len());
            }
        }
        if score.probability >= 0.35 {
            last_speech = i;
        }
        strong += usize::from(score.probability >= 0.60);
        if let Some(pause) = score.acoustic_pause {
            assert_eq!(pause.silence_ms, 256);
            assert_eq!(i - last_speech, 8);
            assert!(strong >= 8, "no sustained speech before pause {i}");
            let (block, time) = last_signal.expect("no captured signal");
            let since_signal_ms = (i - block) * 32;
            assert!(
                (160..=512).contains(&since_signal_ms),
                "pause relative to replay signal: {since_signal_ms} ms"
            );
            observed.push((since_signal_ms, now.duration_since(time)));
            strong = 0;
            pauses.push(i);
        }
        if let Some(end) = score.turn_end {
            assert_eq!(end.silence_ms, 800);
            assert_eq!(i - last_speech, 25);
            ends.push(i);
        }
        if ends.len() == 2 && i - ends[1] >= 16 && output.counters.frames.load(Ordering::Acquire) == source.len() as u64
        {
            break;
        }
    }
    assert_eq!(pauses.len(), 2);
    // The clips were played exactly 2.5 seconds apart; the VAD runs on a
    // 32 ms sample grid, not the driver's callback or the worker's wall clock.
    assert!(
        (75..=81).contains(&(pauses[1] - pauses[0])),
        "unexpected pause spacing {pauses:?}"
    );
    assert_eq!(counters.capture_drops.load(Ordering::Acquire), 0);
    assert_eq!(counters.driver_errors.load(Ordering::Acquire), 0);
    assert_eq!(output.counters.driver_errors.load(Ordering::Acquire), 0);
    // Once a driver reports a discontinuity, even an empty poll cannot go
    // back to Waiting or reuse the recurrent state. Inject only after replay.
    counters.driver_errors.fetch_add(1, Ordering::Release);
    assert!(stream.poll("").is_err());
    assert!(stream.poll("").is_err());
    drop(stream);
    drop(output);
    let callbacks = counters.callbacks.load(Ordering::Acquire);
    let samples = counters.frames.load(Ordering::Acquire);
    inference.sort_unstable();
    let n = inference.len();
    let misses = inference.iter().filter(|&&d| d >= Duration::from_millis(1)).count();
    eprintln!(
        "private CPAL {input_rate}Hz: {callbacks} input callbacks, {samples} samples; \
        pauses={pauses:?}, turn_ends={ends:?} (32ms frames); captured RMS >0.0032 to pause \
        (sample-clock ms, worker elapsed)={observed:?}; Silero {n} live frames \
        p50={:?} p95={:?} p99={:?} max={:?}, {misses} >=1ms",
        inference[n / 2],
        inference[n * 95 / 100],
        inference[n * 99 / 100],
        inference[n - 1]
    );
    assert_eq!(
        misses, 0,
        "the <1ms per-frame Linux gate is a maximum, not a percentile"
    );
}

#[test]
fn endpoint_runs_on_released_speech_after_post_reply_session_split() {
    use eon_voice::vad::VadEndpoint;
    let path = std::env::var_os("EON_ZIPFORMER_TEST_WAV_0")
        .map(PathBuf::from)
        .unwrap_or_else(|| fixture("0.wav"));
    let samples = read_pcm16_mono_16k(&path);
    let mut endpoint = VadEndpoint::new(load_model());
    let previous_session = endpoint.session().clone();
    endpoint.reply_finished();
    assert_ne!(&previous_session, endpoint.session());
    let silence = [0.0; VAD_FRAME_SAMPLES];
    for _ in 0..10 {
        assert!(endpoint.push(&silence, "").unwrap().is_none());
    }
    let mut endpoints = 0;
    for frame in samples
        .as_chunks::<VAD_FRAME_SAMPLES>()
        .0
        .iter()
        .chain(std::iter::repeat_n(&silence, 60))
    {
        endpoints += usize::from(endpoint.push(frame, "after early nightfall").unwrap().is_some());
    }
    assert!(endpoints > 0, "real released speech plus silence never endpointed");
    let mut vad = load_model();
    let mut durations = Vec::new();
    let mut voiced = 0;
    for frame in samples.as_chunks::<VAD_FRAME_SAMPLES>().0 {
        let start = std::time::Instant::now();
        let p = vad.speech_probability(frame).unwrap();
        durations.push(start.elapsed());
        voiced += usize::from(p > 0.5);
    }
    assert!(voiced > durations.len() / 3, "missing speech in the released clip");
    durations.sort();
    eprintln!(
        "Silero {} frames: p50={:?} p95={:?} max={:?}",
        durations.len(),
        durations[durations.len() / 2],
        durations[durations.len() * 95 / 100],
        durations.last().unwrap()
    );
}

#[test]
fn silence_scores_low() {
    let mut vad = load_model();
    let silence = vec![0.0f32; VAD_FRAME_SAMPLES];
    let p = vad.speech_probability(&silence).unwrap();
    assert!((0.0..=1.0).contains(&p));
    assert!(p < 0.1, "silence scored {p}");
}

#[test]
fn wrong_frame_size_is_refused_before_inference() {
    let mut vad = load_model();
    assert!(vad.speech_probability(&[0.0; 480]).is_err());
}

#[test]
fn invalid_pcm_is_rejected_during_holdoff_and_preserves_inference_state() {
    use eon_voice::vad::VadEndpoint;
    let samples = read_pcm16_mono_16k(&fixture("en.wav"));
    let frames = samples.as_chunks::<VAD_FRAME_SAMPLES>().0;
    let mut vad = load_model();
    let mut control = load_model();
    let mut endpoint = VadEndpoint::new(load_model());
    endpoint.reply_finished();
    for frame in &frames[..10] {
        assert_eq!(
            vad.speech_probability(frame).unwrap(),
            control.speech_probability(frame).unwrap()
        );
    }
    for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.01, -1.01] {
        let mut frame = frames[10];
        frame[VAD_FRAME_SAMPLES - 1] = invalid;
        assert!(vad.speech_probability(&frame).is_err());
        assert!(endpoint.push(&frame, "").is_err());
    }
    for frame in &frames[10..20] {
        assert_eq!(
            vad.speech_probability(frame).unwrap(),
            control.speech_probability(frame).unwrap()
        );
    }
}

#[test]
fn reset_makes_inference_repeatable() {
    let mut vad = load_model();
    let frame: Vec<f32> = (0..VAD_FRAME_SAMPLES)
        .map(|i| ((i as f32) * 0.05).sin() * 0.3)
        .collect();
    let first = vad.speech_probability(&frame).unwrap();
    vad.reset();
    let again = vad.speech_probability(&frame).unwrap();
    assert_eq!(first, again);
}

#[test]
fn real_speech_is_detected_and_its_pauses_are_scored_as_silence() {
    let mut vad = load_model();
    let samples = read_pcm16_mono_16k(&fixture("en.wav"));
    let probabilities: Vec<f32> = samples
        .as_chunks::<VAD_FRAME_SAMPLES>()
        .0
        .iter()
        .map(|frame| vad.speech_probability(frame).unwrap())
        .collect();

    let speech = probabilities.iter().filter(|&&p| p > 0.5).count();
    let quiet = probabilities.iter().filter(|&&p| p < 0.2).count();
    let total = probabilities.len();
    assert!(
        speech as f32 / total as f32 > 0.3,
        "only {speech}/{total} frames detected as speech"
    );
    assert!(
        quiet > 0,
        "no frames scored as silence; the VAD cannot tell pauses apart"
    );

    let mut endpointer = Endpointer::new();
    let endpoints = probabilities
        .iter()
        .filter(|&&p| endpointer.push_frame(32, p > 0.5, Completeness::Complete).is_some())
        .count();
    assert!(endpoints >= 1, "the endpointer never closed a turn on real speech");
}
