//! Device loop test against the host's default audio devices (PulseAudio through ALSA in this codespace).
//!
//! Microphone tests require explicit opt-in; missing devices and driver errors fail, never silently pass.
//! ALSA's first period can take over a second to arrive, so the waits are long enough to see the first callback.

use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The device tests share one audio server and sink, and concurrent streams on it produce driver errors. Each test
/// holds this lock for its whole duration.
static DEVICE: Mutex<()> = Mutex::new(());

fn device_guard() -> MutexGuard<'static, ()> {
    DEVICE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

use eon_voice::barge_in::BargeIn;
use eon_voice::device::{capture_default_input, play_default_output};
use eon_voice::ring::pcm_ring;

#[test]
#[ignore = "opens the default microphone; run explicitly on a consenting device or private virtual source"]
fn default_input_delivers_frames_without_driver_errors() {
    let _device = device_guard();
    let (producer, consumer) = pcm_ring(1 << 16);
    let handle = capture_default_input(producer).expect("a working default input device is required");
    std::thread::sleep(Duration::from_millis(2500));
    let frames = handle.counters.frames.load(Ordering::Relaxed);
    let errors = handle.counters.driver_errors.load(Ordering::Relaxed);
    let callbacks = handle.counters.callbacks.load(Ordering::Relaxed);
    drop(handle);
    drop(consumer);
    assert_eq!(errors, 0, "driver reported errors during capture");
    assert!(frames > 0, "no frames captured in 2.5 s; callbacks={callbacks}");
}

#[test]
fn default_output_drains_queued_audio_and_counts_underruns_when_empty() {
    let _device = device_guard();
    let (mut producer, consumer) = pcm_ring(1 << 16);
    let tone: Vec<f32> = (0..8_000)
        .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0).sin() * 0.2)
        .collect();
    assert_eq!(producer.push_slice(&tone), tone.len());
    let handle = play_default_output(consumer, BargeIn::new()).expect("a working default output device is required");
    std::thread::sleep(Duration::from_millis(2500));
    let errors = handle.counters.driver_errors.load(Ordering::Relaxed);
    let underruns = handle.counters.underruns.load(Ordering::Relaxed);
    assert_eq!(errors, 0, "driver reported errors during playback");
    assert_eq!(handle.counters.frames.load(Ordering::Relaxed) as usize, tone.len());
    assert!(
        underruns > 0,
        "an empty ring must be counted as underrun, not silently skipped"
    );
    drop(handle);
}

#[test]
fn barge_in_flushes_queued_speech_during_playback() {
    let _device = device_guard();
    let (mut producer, consumer) = pcm_ring(1 << 20);
    let speech: Vec<f32> = (0..160_000)
        .map(|i| (2.0 * std::f32::consts::PI * 220.0 * i as f32 / 16_000.0).sin() * 0.2)
        .collect();
    assert_eq!(producer.push_slice(&speech), speech.len());
    let barge = BargeIn::new();
    let handle = play_default_output(consumer, barge.clone()).expect("a working default output device is required");
    assert!(barge.is_playing(), "must interrupt active playback");
    let start = Instant::now();
    barge.trigger();
    while handle.counters.barge_in_flushes.load(Ordering::Acquire) == 0 && start.elapsed() < Duration::from_secs(1) {
        std::thread::sleep(Duration::from_micros(100));
    }
    let elapsed = start.elapsed();
    let flushes = handle.counters.barge_in_flushes.load(Ordering::Relaxed);
    let dropped = handle.counters.flushed_samples.load(Ordering::Relaxed);
    let errors = handle.counters.driver_errors.load(Ordering::Relaxed);
    assert_eq!(errors, 0, "driver reported errors during barge-in");
    assert_eq!(flushes, 1, "the trigger must flush exactly once");
    assert!(dropped > 0, "the flush must drop the queued speech, not play it out");
    assert!(!barge.is_pending(), "the request must be consumed");
    eprintln!(
        "CPAL {} Hz request to first observed callback mute <= {elapsed:?}; not acoustic latency",
        handle.sample_rate
    );
    drop(handle);
}
