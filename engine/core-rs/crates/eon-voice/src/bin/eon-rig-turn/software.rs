//! `--software-timing-session ABS_MODEL_DIRECTORY`: repeat u32 LE frame count
//! (1..=480000) and that many interleaved (dry, mix) i16 LE pairs at 16 kHz.
//! Zero count terminates without a row. Only the mix reaches Silero/Zipformer.
//! Never fabricate a quiet tail: any trailing silence or noise bed is input.
//!
//! One flushed JSON line per completed take: `take` (1-based ordinal),
//! `status`, `last_active_end_sample`, `endpoint_sample`, `latency_us`.
//! A confirmed silent Kokoro bridge during startup instead emits only
//! `{"startup_failure":"silent_bridge"}` and exits nonzero.
//! Sample offsets are exclusive, integers or null. Only `ok` has latency;
//! misses are `no_reply`, `early_endpoint`, `no_input_activity`, `no_render_onset`.
//! All-zero negative control expects `no_reply` and three nulls. Bad framing
//! or inference failure is fatal, not a measured miss.
//! Both activity measurements use RMS >= 100 PCM16 units in 1 ms bins. Bound
//! latency from the start of the last active dry bin to the observed end of
//! the first of five consecutive active software-render bins (not acoustics).

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use eon_primitives::EonResult;
use eon_voice::asr::ZipformerAsr;
use eon_voice::barge_in::BargeIn;
use eon_voice::commitment::{BridgeCache, BridgeWarmup, CommitmentGate};
use eon_voice::geometry::{SAMPLE_RATE_HZ, VAD_FRAME_SAMPLES};
use eon_voice::ring::{PcmConsumer, pcm_ring};
use eon_voice::tts::{self, KokoroTts};
use eon_voice::vad::SileroVad;

use super::{Result, detect_turn, final_reply, start_prefix};

const MAX_FRAMES: u32 = 30 * SAMPLE_RATE_HZ;
const PAIR_BYTES: usize = 4;
const DRY_BIN: usize = SAMPLE_RATE_HZ as usize / 1000;
const OUT_BIN: usize = tts::SAMPLE_RATE_HZ as usize / 1000;
const BIN_TIME: Duration = Duration::from_millis(1);
const SUSTAINED_BINS: usize = 5;
const THRESHOLD: i32 = 100; // RMS, PCM16 units, for both tracks measured

pub(super) fn session(models: &Path) -> Result<()> {
    if !models.is_absolute() {
        return Err("software timing needs an absolute model directory".into());
    }
    let mut vad = SileroVad::load(models.join("vad/silero_vad.onnx"))?;
    let mut asr = ZipformerAsr::load(models.join("asr"))?;
    let mut tts = KokoroTts::load(
        models.join("tts/release/kokoro-v1.0.int8.onnx"),
        models.join("tts/voices/af_heart.bin"),
    )?;
    let mut output = io::stdout().lock();
    let cache = startup_cache(
        BridgeCache::measure_warmup(&mut tts, 1., tts::SAMPLE_RATE_HZ),
        &mut output,
    )?;
    drop(tts);
    serve(&mut io::stdin().lock(), &mut output, |bytes| {
        let (last_active, mix) = split_tracks(bytes);
        // Isolate render state and any unconsumed tail; never open CPAL.
        let (mut tx, mut rx) = pcm_ring(1 << 19);
        let signal = BargeIn::new();
        let gate = CommitmentGate::new(1, signal.clone());
        vad.reset();
        let mut stream = asr.start_utterance()?;
        let clock = Monotonic(Instant::now());
        let end = detect_turn(
            PacedMix::new(&mix, &clock),
            SAMPLE_RATE_HZ,
            || Ok(()),
            |frame| vad.speech_probability(frame),
            |frame| Ok(stream.push(frame)?.map(|update| update.text)),
        )?;
        let render = match final_reply(end, || stream.finish())? {
            Some((bridge, _)) => {
                let len = start_prefix(&mut tx, &gate, &cache, bridge)?;
                Render::Prefix(render_onset(&clock, &signal, &mut rx, len))
            }
            None => Render::NoReply,
        };
        // Only now consult the reference. An early closure with real later
        // foreground cannot be made valid by a slow decoder or renderer.
        Row::score(last_active, end, render)
    })
}

fn startup_cache(warmup: EonResult<BridgeWarmup>, output: &mut impl Write) -> Result<BridgeCache> {
    match warmup? {
        BridgeWarmup::Ready(cache) => Ok(cache),
        BridgeWarmup::SilentBridge => {
            output.write_all(b"{\"startup_failure\":\"silent_bridge\"}\n")?;
            output.flush()?;
            Err("silent Kokoro bridge during startup".into())
        }
    }
}

fn serve(input: &mut impl Read, output: &mut impl Write, mut run: impl FnMut(&[u8]) -> Result<Row>) -> Result<()> {
    let mut bytes = Vec::new();
    let mut take = 0u64;
    loop {
        let mut header = [0; 4];
        input.read_exact(&mut header)?;
        let frames = u32::from_le_bytes(header);
        if frames == 0 {
            return Ok(());
        }
        if frames > MAX_FRAMES {
            return Err("software timing take exceeds 30 seconds".into());
        }
        bytes.resize(frames as usize * PAIR_BYTES, 0);
        input.read_exact(&mut bytes)?;
        take = take.checked_add(1).ok_or("too many takes in session")?;
        let row = run(&bytes)?;
        serde_json::to_writer(
            &mut *output,
            &serde_json::json!({
                "take": take, "status": row.status, "last_active_end_sample": row.last_active_end_sample,
                "endpoint_sample": row.endpoint_sample, "latency_us": row.latency_us,
            }),
        )?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}

fn split_tracks(bytes: &[u8]) -> (Option<u32>, Vec<u8>) {
    let mut mix = Vec::with_capacity(bytes.len() / 2);
    let mut last_active = None;
    let mut end = 0;
    for bin in bytes.chunks(DRY_BIN * PAIR_BYTES) {
        let mut energy = 0i64;
        for &[a, b, c, d] in bin.as_chunks::<PAIR_BYTES>().0 {
            energy += i64::from(i16::from_le_bytes([a, b])).pow(2);
            mix.extend_from_slice(&[c, d]);
        }
        let samples = (bin.len() / PAIR_BYTES) as u32;
        end += samples;
        if energy >= i64::from(THRESHOLD).pow(2) * i64::from(samples) {
            last_active = Some(end);
        }
    }
    (last_active, mix)
}

trait Clock {
    fn elapsed(&self) -> Duration;
    fn wait_until(&self, deadline: Duration);
}

struct Monotonic(Instant);

impl Clock for Monotonic {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }

    fn wait_until(&self, deadline: Duration) {
        let wait = deadline.saturating_sub(self.elapsed());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

fn capture_time(sample: u32) -> Duration {
    Duration::from_nanos(u64::from(sample) * 1_000_000_000 / u64::from(SAMPLE_RATE_HZ))
}

/// Model input cannot run ahead of the capture clock, even if the rig reader
/// buffers ahead. Overdue frames are already available; inference overruns
/// remain wall time and do not silently move the dry reference timestamp.
struct PacedMix<'a, C> {
    pcm: &'a [u8],
    pos: usize,
    clock: &'a C,
}

impl<'a, C: Clock> PacedMix<'a, C> {
    fn new(pcm: &'a [u8], clock: &'a C) -> Self {
        Self { pcm, pos: 0, clock }
    }
}

impl<C: Clock> Read for PacedMix<'_, C> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        const MODEL_BYTES: usize = VAD_FRAME_SAMPLES * 2;
        let n = out
            .len()
            .min(self.pcm.len() - self.pos)
            .min(MODEL_BYTES - self.pos % MODEL_BYTES);
        if n != 0 {
            let end = self.pos + n;
            self.clock.wait_until(capture_time(end.div_ceil(2) as u32));
            out[..n].copy_from_slice(&self.pcm[self.pos..end]);
            self.pos = end;
        }
        Ok(n)
    }
}

/// Software sink consumption ticks at Kokoro's native 24 kHz, 1 ms per bin.
/// Timestamp actual post-render PCM, not publication, scheduled time, or the
/// fifth (confirmation) window. Never backfill a missed output tick.
fn render_onset(clock: &impl Clock, signal: &BargeIn, rx: &mut PcmConsumer, len: usize) -> Option<Duration> {
    let mut pcm = [0.; OUT_BIN];
    let mut deadline = clock.elapsed() + BIN_TIME;
    let mut run = 0;
    let mut first = Duration::ZERO;
    let cutoff = (f64::from(THRESHOLD) / 32768.).powi(2) * OUT_BIN as f64;
    for _ in 0..len.div_ceil(OUT_BIN) {
        clock.wait_until(deadline);
        signal.render(rx, &mut pcm);
        let observed = clock.elapsed();
        deadline += BIN_TIME;
        if observed >= deadline {
            // A full tick went by without a render. Don't stitch together
            // active bins on opposite sides of this scheduling gap.
            run = 0;
            deadline = observed + BIN_TIME;
        }
        if pcm.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() >= cutoff {
            if run == 0 {
                first = observed;
            }
            run += 1;
            if run == SUSTAINED_BINS {
                return Some(first);
            }
        } else {
            run = 0;
        }
    }
    None
}

enum Render {
    NoReply,
    Prefix(Option<Duration>),
}

struct Row {
    status: &'static str,
    last_active_end_sample: Option<u32>,
    endpoint_sample: Option<u32>,
    latency_us: Option<u64>,
}

impl Row {
    fn score(last: Option<u32>, end: Option<u32>, render: Render) -> Result<Self> {
        let (status, latency_us) = match (render, end, last) {
            (Render::NoReply, _, _) => ("no_reply", None),
            (Render::Prefix(_), None, _) => return Err("render without semantic endpoint".into()),
            (Render::Prefix(_), _, None) => ("no_input_activity", None),
            (Render::Prefix(_), Some(end), Some(last)) if end < last => ("early_endpoint", None),
            (Render::Prefix(None), _, _) => ("no_render_onset", None),
            (Render::Prefix(Some(onset)), _, Some(last)) => {
                // Conservative bin bound: start of the last active dry bin to
                // the observed end of the first sustained rendered bin; round up.
                let start = (last - 1) / DRY_BIN as u32 * DRY_BIN as u32;
                let elapsed = onset
                    .checked_sub(capture_time(start))
                    .ok_or("render before foreground")?;
                ("ok", Some(u64::try_from(elapsed.as_nanos().div_ceil(1000))?))
            }
        };
        Ok(Self {
            status,
            last_active_end_sample: last,
            endpoint_sample: end,
            latency_us,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    struct TestClock {
        now: Cell<Duration>,
        stall_at: Option<Duration>,
    }

    impl Clock for TestClock {
        fn elapsed(&self) -> Duration {
            self.now.get()
        }
        fn wait_until(&self, deadline: Duration) {
            let mut wake = self.elapsed().max(deadline);
            if Some(deadline) == self.stall_at {
                wake += Duration::from_millis(20);
            }
            self.now.set(wake);
        }
    }

    fn pair(bytes: &mut Vec<u8>, dry: i16, mixed: i16) {
        bytes.extend_from_slice(&dry.to_le_bytes());
        bytes.extend_from_slice(&mixed.to_le_bytes());
    }

    #[test]
    fn measures_whole_dry_track_including_a_short_final_bin_without_changing_the_bed() -> Result<()> {
        let mut bytes = Vec::new();
        for i in 0..3 * DRY_BIN {
            let dry = match i / DRY_BIN {
                0 => i16::MIN,                // must not overflow
                1 => -100,                    // inclusive at threshold
                _ if i % DRY_BIN == 0 => 350, // peak above threshold, RMS below
                _ => 0,
            };
            pair(&mut bytes, dry, -8192);
        }
        let (end, mix) = split_tracks(&bytes);
        assert_eq!(end, Some(2 * DRY_BIN as u32));
        assert_eq!(mix, (-8192i16).to_le_bytes().repeat(3 * DRY_BIN));
        pair(&mut bytes, 100, -8192);
        let (last, _) = split_tracks(&bytes);
        assert_eq!(last, Some(49));
        // A delayed final/render cannot legitimize a closure before later dry speech.
        let row = Row::score(last, Some(48), Render::Prefix(Some(Duration::from_secs(1))))?;
        assert_eq!((row.status, row.latency_us), ("early_endpoint", None));
        Ok(())
    }

    #[test]
    fn time_includes_input_clock_semantic_wait_inference_and_actual_prefix_bins() -> Result<()> {
        let clock = TestClock::default();
        let mut bytes = Vec::new();
        for block in 1..=20i16 {
            for _ in 0..VAD_FRAME_SAMPLES {
                pair(&mut bytes, if block <= 2 { 1000 } else { 0 }, block);
            }
        }
        let (last, mixed) = split_tracks(&bytes);
        let mut decoded = 0;
        let end = detect_turn(
            PacedMix::new(&mixed, &clock),
            SAMPLE_RATE_HZ,
            || Ok(()),
            |frame| Ok(if frame[0] <= 2. / 32768. { 1. } else { 0. }),
            |frame| {
                decoded += 1;
                // Neither the dry reference nor mix lookahead reaches this model.
                assert!(frame.iter().all(|&s| s == decoded as f32 / 32768.));
                assert_eq!(clock.elapsed(), Duration::from_millis(32 * decoded));
                Ok(Some("stop".into()))
            },
        )?;
        assert_eq!((last, end), (Some(1024), Some(6144)));
        assert_eq!(clock.elapsed(), Duration::from_millis(384)); // not the 640 ms file end
        assert!(
            final_reply(end, || {
                clock.now.set(clock.elapsed() + Duration::from_millis(51));
                Ok("stop".into())
            })?
            .is_some()
        );

        let mut pcm = Vec::new();
        for amp in [0., 0.125, 0.125, 0.125, 0.125, 0., 0.125, 0.125, 0.125, 0.125, 0.125] {
            pcm.extend_from_slice(&[amp; OUT_BIN]);
        }
        let (mut tx, mut rx) = pcm_ring(pcm.len());
        assert_eq!(tx.push_slice(&pcm), pcm.len());
        let onset = render_onset(&clock, &BargeIn::new(), &mut rx, pcm.len());
        assert_eq!(onset, Some(Duration::from_millis(442))); // bin 7, not enqueue or bin 11
        assert_eq!(clock.elapsed(), Duration::from_millis(446));
        let row = Row::score(last, end, Render::Prefix(onset))?;
        assert_eq!((row.status, row.latency_us), ("ok", Some(379_000))); // 442 - 63 ms
        Ok(())
    }

    #[test]
    fn slow_inference_does_not_shift_capture_deadlines_and_muted_pcm_is_not_an_onset() -> Result<()> {
        let clock = TestClock::default();
        let bytes = vec![0; 2 * VAD_FRAME_SAMPLES * 2];
        let mut mix = PacedMix::new(&bytes, &clock);
        let mut out = [1; 2 * VAD_FRAME_SAMPLES];
        mix.read_exact(&mut out)?;
        assert_eq!(clock.elapsed(), Duration::from_millis(32));
        clock.now.set(Duration::from_millis(77));
        mix.read_exact(&mut out)?;
        assert_eq!(clock.elapsed(), Duration::from_millis(77));
        let pcm = [1.; OUT_BIN * SUSTAINED_BINS];
        let (mut tx, mut rx) = pcm_ring(pcm.len());
        tx.push_slice(&pcm);
        let signal = BargeIn::new();
        signal.trigger();
        let onset = render_onset(&clock, &signal, &mut rx, pcm.len());
        assert_eq!(onset, None);
        let row = Row::score(Some(32), Some(512), Render::Prefix(onset))?;
        assert_eq!((row.status, row.latency_us), ("no_render_onset", None));
        Ok(())
    }

    #[test]
    fn a_late_render_tick_does_not_backdate_or_bridge_the_gap_in_a_sustained_onset() {
        let clock = TestClock {
            stall_at: Some(Duration::from_millis(3)),
            ..TestClock::default()
        };
        let pcm = [0.25; OUT_BIN * 7];
        let (mut tx, mut rx) = pcm_ring(pcm.len());
        tx.push_slice(&pcm);
        assert_eq!(
            render_onset(&clock, &BargeIn::new(), &mut rx, pcm.len()),
            Some(Duration::from_millis(23)) // first after stall, not scheduled 3 ms
        );
        assert_eq!(clock.elapsed(), Duration::from_millis(27));
    }

    #[test]
    fn startup_marker_requires_the_measured_silent_variant_not_error_text() {
        let mut output = Vec::new();
        // Even a regular model error with identical wording isn't evidence.
        let failure = eon_primitives::EonError::ValidationError("Kokoro returned a silent bridge".into());
        assert!(startup_cache(Err(failure), &mut output).is_err());
        assert!(output.is_empty());
        assert!(startup_cache(Ok(BridgeWarmup::SilentBridge), &mut output).is_err());
        assert_eq!(output, b"{\"startup_failure\":\"silent_bridge\"}\n");
    }

    #[test]
    fn framing_is_bounded_complete_ordered_and_has_no_startup_or_termination_row() -> Result<()> {
        use serde_json::{Value, json};
        let mut wire = Vec::new();
        for dry in [0, 0, 100] {
            wire.extend_from_slice(&1u32.to_le_bytes());
            pair(&mut wire, dry, 10);
        }
        wire.extend_from_slice(&0u32.to_le_bytes());
        let mut count = 0;
        let mut rows = Vec::new();
        serve(&mut &wire[..], &mut rows, |bytes| {
            count += 1;
            let (last, _) = split_tracks(bytes);
            match count {
                1 => Row::score(last, None, Render::NoReply), // silent control
                2 => Row::score(last, Some(1), Render::Prefix(Some(BIN_TIME))), // false trigger
                _ => Row::score(last, Some(1), Render::NoReply), // empty final
            }
        })?;
        let lines: Vec<Value> = std::str::from_utf8(&rows)?
            .lines()
            .map(|l| {
                assert!(l.len() < 256);
                serde_json::from_str(l)
            })
            .collect::<std::result::Result<_, _>>()?;
        assert_eq!(
            lines,
            [
                json!({"take":1,"status":"no_reply","last_active_end_sample":null,"endpoint_sample":null,"latency_us":null}),
                json!({"take":2,"status":"no_input_activity","last_active_end_sample":null,"endpoint_sample":1,"latency_us":null}),
                json!({"take":3,"status":"no_reply","last_active_end_sample":1,"endpoint_sample":1,"latency_us":null}),
            ]
        );
        for mut bad in [&[1, 0][..], &[1, 0, 0, 0, 1, 2, 3], &(MAX_FRAMES + 1).to_le_bytes()] {
            rows.clear();
            assert!(serve(&mut bad, &mut rows, |_| panic!("processed malformed request")).is_err());
            assert!(rows.is_empty());
        }
        Ok(())
    }
}
