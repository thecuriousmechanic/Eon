use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{kokoro_release, load_speaker, models, owner_recording, private_monitor};
use eon_voice::barge_in::BargeIn;
use eon_voice::commitment::ReplyRenderer;
use eon_voice::coordinator::{BargeInCoordinator, DuplexReflex};
use eon_voice::device::play_default_output_with_reference;
use eon_voice::resample::CaptureFrames;
use eon_voice::ring::pcm_ring;
use eon_voice::speaker::SpeakerGate;
use eon_voice::vad::SileroVad;
use eon_voice::{LeaseId, TaskId};

const RATE: u32 = 48_000;

struct Packet {
    received: Instant,
    pcm: [i16; 48],
}

struct Monitor {
    child: Child,
    reader: Option<JoinHandle<Vec<Packet>>>,
}

impl Monitor {
    fn start(server: &str) -> Self {
        let mut child = Command::new("parec")
            .args([
                "--server",
                server,
                "--device=eon_vad.monitor",
                "--raw",
                "--format=s16le",
                "--rate=48000",
                "--channels=1",
                "--latency-msec=1",
                "--process-time-msec=1",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stream = child.stdout.take().unwrap();
        let (ready, wait) = std::sync::mpsc::sync_channel(1);
        let reader = thread::spawn(move || {
            let mut packets = Vec::new();
            let mut pcm = [0; 96]; // 1 ms; timestamp arrival, never extrapolate from sample counts.
            while stream.read_exact(&mut pcm).is_ok() {
                let received = Instant::now();
                if packets.is_empty() {
                    let _ = ready.send(pcm == [0; 96]);
                }
                packets.push(Packet {
                    received,
                    pcm: std::array::from_fn(|i| i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]])),
                });
            }
            packets
        });
        let monitor = Self {
            child,
            reader: Some(reader),
        };
        assert_eq!(
            wait.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "private monitor not live and silent"
        );
        monitor
    }

    // Locate the callback's entire Kokoro utterance *in the independent sink capture*.
    // A natural pause, lost recording or callback acknowledgement is not proof of stop.
    fn observed_stop(packets: &[Packet], rendered: &[f32], decision: Instant) -> (Duration, usize) {
        let pcm: Vec<_> = packets.iter().flat_map(|p| p.pcm).collect();
        let last = pcm.iter().rposition(|s| *s != 0).expect("no monitored output");
        assert!(
            packets
                .iter()
                .any(|p| p.received < decision && p.pcm.iter().any(|s| s.unsigned_abs() > 100))
        );
        assert!(
            packets[last / 48].received >= decision,
            "reply was already silent before verification"
        );
        let silent = (pcm.len() - last - 1) / 48;
        assert!(silent >= 200, "monitor did not deliver sustained digital silence");
        let landmark = rendered
            .iter()
            .rposition(|s| s.abs() > 0.05)
            .expect("no speech landmark");
        // Even a quiet phoneme can end with PCM16 zeros. Match the entire tail
        // including those zeros, and time the *latest* possible first muted frame
        // from the packet that actually delivered it, not from a sample clock.
        let agrees = |f: f32, s: i16| ((f * 32768.).clamp(-32768., 32767.) - f32::from(s)).abs() <= 2.;
        let boundary = (last + 1..=(last + rendered.len() - landmark).min(pcm.len() - 1))
            .rev()
            .find(|&end| {
                let Some(start) = end.checked_sub(rendered.len()) else {
                    return false;
                };
                agrees(rendered[landmark], pcm[start + landmark])
                    && rendered.iter().zip(&pcm[start..end]).all(|(&f, &s)| agrees(f, s))
            })
            .expect("monitor did not reproduce the interrupted waveform");
        (packets[boundary / 48].received.duration_since(decision), silent)
    }

    fn finish(mut self) -> Vec<Packet> {
        self.child.kill().unwrap();
        self.reader.take().unwrap().join().unwrap()
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
#[ignore = "run vad_private_monitor.py --private-monitor --barge-in with pinned models and a recorded owner"]
fn private_monitor_stop() {
    let server = private_monitor::require_null_sink();
    let speech = owner_recording();
    let (enrollment, held_out) = speech.split_at(speech.len() / 2);
    let mut speaker = load_speaker();
    let owner = SpeakerGate::enroll(&speaker.embed(enrollment).unwrap(), 0.35).unwrap();
    let vad = SileroVad::load(models().join("vad/silero_vad.onnx")).unwrap();
    let signal = BargeIn::new();
    let task = TaskId::new();
    let coordinator = BargeInCoordinator::new(task.clone(), signal.clone(), owner);
    let audit = coordinator.audit();
    let mut reflex = DuplexReflex::new(coordinator, vad, speaker, Some(0)).unwrap();
    let journal_path = Path::new(server.strip_prefix("unix:").unwrap()).with_file_name("interruption.jsonl");
    let mut journal = std::fs::File::create_new(&journal_path).unwrap();

    let mut tts = kokoro_release::load(&models());
    let (tx, rx) = pcm_ring(1 << 15);
    let mut worker = ReplyRenderer::new(1, signal.clone(), tx, RATE).unwrap();
    let clause = "The test report is ready for the team to review before we begin the next round of testing.";
    let cursor = clause.split_whitespace().count();
    worker.queue(clause).unwrap();
    assert_eq!(worker.pump(&mut tts, 1.0).unwrap(), 0);
    worker.lease_acquired(1, LeaseId::new());
    let monitor = Monitor::start(&server);
    // Prime the committed clause before opening the callback: no unobserved
    // reference samples can be lost while synthesis is running.
    let mut submitted = worker.pump(&mut tts, 1.0).unwrap();
    let (ref_tx, mut ref_rx) = pcm_ring(1 << 15);
    let device = play_default_output_with_reference(rx, signal, ref_tx).unwrap();
    assert_eq!(device.sample_rate, RATE);
    let (mut resample_tx, resample_rx) = pcm_ring(1 << 15);
    let mut rendered = CaptureFrames::<160>::new(resample_rx, RATE).unwrap();
    let mut raw = Vec::new();
    let mut buf = [0.; 1024];
    let mut near = held_out.as_chunks::<160>().0.iter();
    let mut speaking = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for a verified owner");
        let n = ref_rx.pop_slice(&mut buf);
        raw.extend_from_slice(&buf[..n]);
        assert_eq!(resample_tx.push_slice(&buf[..n]), n);
        if let Some(reference) = rendered.try_read().unwrap() {
            // Fixture-only near end with a digital echo, paced by the actual CPAL
            // output, not by a synthetic clock. No system input device is opened.
            let mut mic = reference.map(|s| s * 0.25);
            speaking |= reference.iter().any(|s| *s != 0.0);
            if speaking {
                for (s, n) in mic.iter_mut().zip(near.next().expect("owner not verified")) {
                    *s = (*s + n).clamp(-1.0, 1.0);
                }
            }
            if reflex.process(&reference, &mut mic, cursor, &mut journal).unwrap() {
                break;
            }
        } else {
            thread::sleep(Duration::from_micros(250));
        }
        submitted += worker.pump(&mut tts, 1.0).unwrap();
    }
    let decision = audit.decision_instant().unwrap();
    assert_eq!(worker.pump(&mut tts, 1.0).unwrap(), 0);
    thread::sleep(Duration::from_millis(300)); // keep the stream open while observing the silence
    let packets = monitor.finish();
    assert_eq!(device.startup_retries, 0, "interrupted reference at startup");
    assert_eq!(device.counters.underruns.load(Ordering::Acquire), 0);
    assert_eq!(device.counters.driver_errors.load(Ordering::Acquire), 0);
    assert_eq!(device.counters.reference_drops.load(Ordering::Acquire), 0);
    assert_eq!(device.counters.barge_in_flushes.load(Ordering::Acquire), 1);
    let flushed = device.counters.flushed_samples.load(Ordering::Acquire);
    assert!(flushed >= RATE as u64 / 10, "no substantial backlog to interrupt");
    let frames = device.counters.frames.load(Ordering::Acquire) as usize;
    assert_eq!(frames + flushed as usize, submitted);
    while raw.len() < frames {
        let n = ref_rx.pop_slice(&mut buf);
        assert!(n > 0, "missing final callback reference");
        raw.extend_from_slice(&buf[..n]);
    }

    let event: serde_json::Value = serde_json::from_slice(&std::fs::read(journal_path).unwrap()).unwrap();
    assert_eq!(event["event"], "INTERRUPTED_BY_USER");
    assert_eq!(event["task_id"], task.as_str());
    assert_eq!(event["token_offset"], cursor);
    assert!(event["timestamp_ms"].as_u64().unwrap() > 0);
    let (cutoff, silent) = Monitor::observed_stop(&packets, &raw[..frames], decision);
    eprintln!(
        "{}: {RATE} Hz null sink, {} ms of held-out speech consumed; {} queued samples flushed; \
         decision to observed sink silence <= {cutoff:?}, {} consecutive silent monitor packets",
        event["event"],
        (held_out.len() / 160 - near.len()) * 10,
        flushed,
        silent
    );
    assert!(
        cutoff < Duration::from_millis(5),
        "virtual output stop not demonstrated within 5 ms"
    );
}
