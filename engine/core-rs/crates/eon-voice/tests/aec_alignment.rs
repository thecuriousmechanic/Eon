//! Delay alignment of the AEC reference. The same echo is cancelled twice: once with the playback reference delayed by
//! the echo delay (aligned), and once with that delay applied twice (late).
//! AEC3 can search its history for an early reference, but cannot use future audio.

use eon_voice::aec::AcousticEchoCanceller;
use eon_voice::delay::DelayLine;

const RATE: u32 = 16_000;
const DELAY_MS: usize = 400;

fn erle_db(aligned_reference: bool) -> f32 {
    let delay = DELAY_MS * RATE as usize / 1000;
    let total = (RATE as f32 * 3.0) as usize;
    // At 440 Hz, 400 ms is exactly 176 periods: that tone cannot distinguish
    // the two timelines. Broadband input makes the alignment observable.
    let mut rng = 91u32;
    let render: Vec<f32> = (0..total)
        .map(|i| {
            rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
            // Include pauses: a continuously active far end can cause nonlinear
            // suppression even when its reference does not match the echo.
            if (i / delay).is_multiple_of(2) {
                (rng as f32 / u32::MAX as f32 - 0.5) * 0.6
            } else {
                0.0
            }
        })
        .collect();
    let mic: Vec<f32> = (0..total)
        .map(|i| if i >= delay { render[i - delay] * 0.5 } else { 0.0 })
        .collect();

    // External alignment compensates the full delay; don't compensate twice.
    let aec = AcousticEchoCanceller::new(RATE, Some(0)).unwrap();
    let frame = aec.frame_samples();
    let mut reference = DelayLine::new(if aligned_reference { delay } else { delay * 2 });
    let mut capture = mic.clone();
    let mut frame_reference = vec![0.0f32; frame];
    for start in (0..total).step_by(frame) {
        let end = start + frame;
        if end > total {
            break;
        }
        frame_reference.copy_from_slice(&render[start..end]);
        reference.process_block(&mut frame_reference);
        aec.analyze_render(&frame_reference).unwrap();
        aec.process_capture(&mut capture[start..end]).unwrap();
    }
    let tail = total * 2 / 3..total;
    let before: f32 = mic[tail.clone()].iter().map(|s| s * s).sum();
    let after: f32 = capture[tail].iter().map(|s| s * s).sum();
    10.0 * (before / after.max(1e-12)).log10()
}

#[test]
fn aligning_the_playback_reference_is_what_makes_cancellation_work() {
    let aligned = erle_db(true);
    let misaligned = erle_db(false);
    eprintln!("ERLE aligned={aligned:.1} dB late={misaligned:.1} dB");
    assert!(aligned > 15.0, "aligned reference cancels only {aligned:.1} dB");
    assert!(
        aligned > misaligned + 10.0,
        "alignment must matter: aligned {aligned:.1} dB, misaligned {misaligned:.1} dB"
    );
}
