//! Recorded handoff controls for verification context. No live devices or acoustic timing.

use std::path::PathBuf;

use super::{RecordedEnrollment, common, load_speaker, models};
use eon_voice::TaskId;
use eon_voice::barge_in::BargeIn;
use eon_voice::coordinator::{BargeInCoordinator, CANDIDATE_SAMPLES, DuplexReflex};
use eon_voice::ring::pcm_ring;
use eon_voice::vad::SileroVad;

const BLOCK: usize = 160;

fn rms(pcm: &[f32]) -> f32 {
    (pcm.iter().map(|x| x * x).sum::<f32>() / pcm.len() as f32).sqrt()
}

// Splice into active speech, not a leading silence that would clear the VAD
// history. The rule is the same for both recordings and uses no identity scores.
fn speech_onset(pcm: &[f32]) -> &[f32] {
    let floor = rms(pcm);
    let block = pcm.as_chunks::<BLOCK>().0.iter().position(|x| rms(x) >= floor).unwrap();
    &pcm[block * BLOCK..]
}

// Owner -> stranger -> held-out owner, with uninterrupted render and a single
// VAD, echo path, speaker gate and audit slot across both splices.
fn handoff(enrollment: &RecordedEnrollment, prelude: &[f32], stranger: &[f32], returning: &[f32]) -> Option<(u64, u64, usize)> {
    let signal = BargeIn::new();
    let speaker = load_speaker();
    let coordinator = BargeInCoordinator::new(TaskId::new(), signal.clone(), enrollment.policy());
    let audit = coordinator.audit();
    let vad = SileroVad::load(models().join("vad/silero_vad.onnx")).unwrap();
    let mut reflex = DuplexReflex::new(coordinator, vad, speaker, Some(0)).unwrap();
    let (mut tx, mut rx) = pcm_ring(BLOCK);
    let tone: [f32; BLOCK] = std::array::from_fn(|i| 0.1 * (std::f32::consts::TAU * i as f32 / 32.0).sin());
    let mut out = [0.0; BLOCK];
    let mut journal = Vec::new();
    let mut rejected = [0; 2];
    for (phase, pcm) in [prelude, stranger, returning].into_iter().enumerate() {
        let before = reflex.rejected_candidates();
        for (i, near) in pcm.as_chunks::<BLOCK>().0.iter().enumerate() {
            assert_eq!(tx.push_slice(&tone), BLOCK);
            assert!(
                signal.render(&mut rx, &mut out).samples == BLOCK,
                "handoff failed: playback stopped before decision"
            );
            let mut mic = std::array::from_fn(|j| near[j] + 0.25 * out[j]);
            if reflex.process(&out, &mut mic, phase, &mut journal).unwrap() {
                match phase {
                    0 => return None, // Legitimate owner decision before the cut: not a negative trial.
                    1 => panic!(
                        "handoff failed: playback interrupted {} ms into the stranger after {} ms of owner",
                        (i + 1) * 10,
                        prelude.len() / 16
                    ),
                    _ => {
                        assert!(
                            audit.record().is_some_and(|r| r.token_offset == phase),
                            "handoff failed: missing or incorrect owner audit"
                        );
                        assert_eq!(tx.push_slice(&tone), BLOCK);
                        assert!(
                            signal.render(&mut rx, &mut out).acknowledged,
                            "handoff failed: owner mute not acknowledged"
                        );
                        assert!(out == [0.0; BLOCK], "handoff failed: audible output after owner mute");
                        return Some((rejected[0], rejected[1], (i + 1) * 10));
                    }
                }
            }
        }
        assert!(
            audit.record().is_none() && journal.is_empty(),
            "handoff failed: audit without interruption"
        );
        if phase < 2 {
            rejected[phase] = reflex.rejected_candidates() - before;
            if phase == 1 {
                assert!(rejected[1] > 0, "handoff failed: stranger never reached the speaker gate");
            }
        }
    }
    panic!("handoff failed: returning clean owner never interrupted");
}

// None means the owner latched before the cut; Some counts rejections before an exercised switch.
fn check_switch_coverage<T>(
    clean: T,
    noise: T,
    mut run: impl FnMut(&str, T) -> [Option<u64>; 4],
) -> Result<(), String> {
    // Run both sets of cuts before reporting missing exercise: a noisy cut may actually fail.
    let observations = [("clean", clean), ("noise", noise)].map(|(label, input)| (label, run(label, input)));
    for (label, cuts) in observations {
        if cuts.iter().all(Option::is_none) {
            return Err(format!("all {label} trials already latched before the cut"));
        }
        if !cuts.iter().flatten().any(|&pre| pre > 0) {
            return Err(format!("no {label} switch exercised a previously rejected owner candidate"));
        }
    }
    Ok(())
}

#[test]
#[should_panic(expected = "handoff failed: noisy stranger interrupted")]
fn missing_clean_exercise_does_not_mask_a_noisy_failure() {
    check_switch_coverage((), (), |label, ()| {
        if label == "noise" {
            panic!("handoff failed: noisy stranger interrupted");
        }
        [None; 4]
    }).unwrap();
}

#[test]
fn rejected_owner_history_is_required_in_each_condition() {
    let with = [None, Some(0), Some(1), None];
    let without = [None, Some(0), Some(0), None];
    for (clean, noise, missing) in [(without, with, "clean"), (with, without, "noise")] {
        assert_eq!(
            check_switch_coverage(clean, noise, |_, cuts| cuts),
            Err(format!("no {missing} switch exercised a previously rejected owner candidate"))
        );
    }
    assert_eq!(check_switch_coverage(with, with, |_, cuts| cuts), Ok(()));
}

#[test]
fn active_speaker_switches_do_not_credit_a_departed_owner() {
    let (owner, stranger) = match (
        std::env::var_os("EON_HANDOFF_OWNER_WAV"),
        std::env::var_os("EON_HANDOFF_STRANGER_WAV"),
    ) {
        (None, None) => {
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
            (dir.join("en.wav"), dir.join("0.wav"))
        }
        (Some(a), Some(b)) => {
            let (a, b) = (PathBuf::from(a), PathBuf::from(b));
            assert!(a.is_absolute() && b.is_absolute() && a.is_file() && b.is_file(),
                "supply two absolute paths to authorized WAV files");
            (a, b)
        }
        _ => panic!("handoff requires both EON_HANDOFF_OWNER_WAV and EON_HANDOFF_STRANGER_WAV"),
    };
    let owner = common::read_pcm16_mono_16k(&owner);
    let stranger = common::read_pcm16_mono_16k(&stranger);
    let half = owner.len() / 2;
    let enrollment = RecordedEnrollment::new(&mut load_speaker(), &owner[..half]);
    eprintln!("handoff speaker model: {}", enrollment.id());
    let returning = speech_onset(&owner[half..]);
    let stranger = speech_onset(&stranger);
    // Equal-energy broadband noise on both sides of the switch, normalized by
    // the complete clip, not the score of an individual verification window.
    let mut seed = 1u32;
    let mut bed = |len| -> Vec<f32> {
        (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                seed as f32 / u32::MAX as f32 - 0.5
            })
            .collect()
    };
    let mix = |speech: &[f32], noise: &[f32]| -> Vec<f32> {
        let gain = rms(speech) / rms(noise);
        speech.iter().zip(noise).map(|(s, n)| 0.5 * (s + n * gain)).collect()
    };
    let owner_noisy = mix(returning, &bed(returning.len()));
    let stranger_noisy = mix(stranger, &bed(stranger.len()));
    check_switch_coverage((returning, stranger), (&owner_noisy, &stranger_noisy), |label, (prelude, other)| {
        // Half-window offsets: never align the identities to verification boundaries.
        [1, 3, 5, 7].map(|halves| {
            let cut = (CANDIDATE_SAMPLES * halves / 2 / BLOCK) * BLOCK;
            handoff(&enrollment, &prelude[..cut], other, returning).map(|(pre, negative, owner_ms)| {
                eprintln!(
                    "switch {label} after {} ms: {pre} owner embeddings before cut, \
                     {negative} rejections during stranger, returning owner at {owner_ms} ms",
                    cut / 16
                );
                pre
            })
        })
    }).unwrap_or_else(|reason| panic!("handoff unexercised: {reason}"));
}
