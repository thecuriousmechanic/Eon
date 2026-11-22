//! Recorded speech and a private in-memory render callback; no default devices.
//! Opt-in with `--features token-readback`. Asset absence is a test failure.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eon_mesh::split_inference::zipformer::readback::{Readback, TokenPeer, TokenRecognizer, Vocabulary};
use eon_mesh::split_inference::zipformer::{MAX_SAMPLES, Source};
use eon_voice::asr::ZipformerAsr;
use eon_voice::barge_in::BargeIn;
use eon_voice::commitment::ReplyRenderer;
use eon_voice::resample::resample_mono;
use eon_voice::ring::{PcmConsumer, pcm_ring};
use eon_voice::tts::{KokoroTts, SAMPLE_RATE_HZ};
use eon_voice::{EonError, LeaseId};
use iroh::endpoint::{Endpoint, presets::Minimal};
use iroh::{EndpointAddr, RelayMode, SecretKey};
use sha2::{Digest, Sha256};
use tokio::time::timeout;

#[cfg(target_os = "linux")]
#[path = "../../eon-voice/tests/common/private_monitor.rs"]
mod private_monitor;

const REFERENCE: &str =
    "AFTER EARLY NIGHTFALL THE YELLOW LAMPS WOULD LIGHT UP HERE AND THERE THE SQUALID QUARTER OF THE BROTHELS";
const EPOCH: u64 = 38;

fn models() -> PathBuf {
    std::env::var_os("EON_VOICE_MODELS")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../perception/models"))
}

fn checked(path: &Path, digest: &str) -> Vec<u8> {
    let data = std::fs::read(path).expect("fetch the release assets from scripts/models.lock.json");
    assert_eq!(format!("{:x}", Sha256::digest(&data)), digest);
    data
}

fn recording() -> Vec<i16> {
    let path = std::env::var_os("EON_ZIPFORMER_TEST_WAV_0")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../eon-voice/tests/fixtures/0.wav"));
    checked(
        &path,
        "6bc58a4efdf20daac252b6b1502632601a71efe0308f6757dc1eda34891a7e4f",
    )[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| i16::from_le_bytes(pair))
        .collect()
}

fn tts() -> KokoroTts {
    let dir = models();
    let graph = dir.join("tts/release/kokoro-v1.0.int8.onnx");
    let voice = dir.join("tts/voices/af_heart.bin");
    checked(
        &graph,
        "6e742170d309016e5891a994e1ce1559c702a2ccd0075e67ef7157974f6406cb",
    );
    checked(
        &voice,
        "d583ccff3cdca2f7fae535cb998ac07e9fcb90f09737b9a41fa2734ec44a8f0b",
    );
    KokoroTts::load(graph, voice).unwrap()
}

async fn bind(key: SecretKey, paired: &SecretKey) -> TokenPeer {
    TokenPeer::bind((Ipv4Addr::LOCALHOST, 0).into(), key, paired.public())
        .await
        .unwrap()
}

fn allow<'a>(pcm: &'a [i16], edge: &'a TokenPeer, worker: &EndpointAddr) -> Readback<'a> {
    Readback::AllowTokenPeer {
        pcm,
        edge,
        worker: worker.clone(),
    }
}

fn renderer() -> (ReplyRenderer, PcmConsumer) {
    let (tx, rx) = pcm_ring(1024);
    (
        ReplyRenderer::new(EPOCH, BargeIn::new(), tx, SAMPLE_RATE_HZ).unwrap(),
        rx,
    )
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "output-only CPAL; requires the explicit isolated eon_vad null server and ALSA configuration"]
async fn split_inference_tests_final_reaches_private_cpal_output() {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    assert_eq!(std::env::var("EON_VOICE_TEST_PRIVATE_OUTPUT").as_deref(), Ok("1"));
    private_monitor::require_null_sink(); // before asking CPAL for any output
    let pcm = recording();
    let dir = models().join("asr");
    let local = TokenRecognizer::load(&dir).unwrap();
    let vocab = Vocabulary::load(&dir).unwrap();
    let mut synth = tts();
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let edge = bind(a.clone(), &b).await;
    let mut worker = bind(b, &a).await;
    let addr = worker.addr();
    let (mut output, device, mut reference) = ReplyRenderer::open_default(EPOCH).unwrap();
    let (result, served) = tokio::join!(
        allow(&pcm, &edge, &addr).queue(&local, &mut output, |_| {}),
        worker.serve_one(&vocab)
    );
    served.unwrap();
    assert_eq!(result.unwrap().source, Source::PairedWorker);
    assert_eq!(output.pump(&mut synth, 1.).unwrap(), 0);
    assert_eq!(device.counters.frames.load(Ordering::Acquire), 0);
    reference.flush();
    let baseline_drops = reference.dropped();
    // Drain the callback's post-render reference even during blocking Kokoro
    // inference. Do not open the Pulse source or any other capture device.
    let (stop, done) = std::sync::mpsc::channel::<()>();
    let rate = device.sample_rate;
    let recording = std::thread::spawn(move || {
        let mut buf = [0.; 2048];
        let mut heard = Vec::new();
        loop {
            let n = reference.pop_slice(&mut buf);
            heard.extend_from_slice(&buf[..n]);
            assert!(heard.len() < 30 * rate as usize);
            if n == 0 {
                if done.try_recv() != Err(std::sync::mpsc::TryRecvError::Empty) {
                    return (heard, reference.dropped());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    });
    output.lease_acquired(EPOCH, LeaseId::new());
    let mut offered = output.pump(&mut synth, 1.).unwrap();
    assert!(offered > 0);
    let started = Instant::now();
    loop {
        offered += output.pump(&mut synth, 1.).unwrap();
        if device.counters.frames.load(Ordering::Acquire) == offered as u64 {
            let tail = output.pump(&mut synth, 1.).unwrap();
            offered += tail;
            if tail == 0 {
                break;
            }
        }
        assert!(started.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(device.counters.driver_errors.load(Ordering::Acquire), 0);
    drop(device);
    stop.send(()).unwrap();
    let (played, drops) = recording.join().unwrap();
    assert_eq!(drops, baseline_drops);
    assert!(played.iter().all(|s| (-1.0..=1.0).contains(s)));
    // The driver samples are normalized; bandlimited test resampling can
    // overshoot full scale. Convert back to normalized ASR input.
    let mut mono = resample_mono(&played, rate, 16_000).unwrap();
    mono.iter_mut().for_each(|s| *s = s.clamp(-1., 1.));
    let words = ZipformerAsr::load_ort(&dir).unwrap().transcribe(&mono).unwrap();
    assert!(words.starts_with("I HEARD") && words.contains("YELLOW") && words.contains("QUARTER"));
    eprintln!("private output callback consumed {offered} readback samples at {rate} Hz");
    worker.close().await;
    edge.close().await;
}

#[tokio::test]
async fn split_inference_tests_real_tokens_to_verified_pronunciation_and_edge_playback() {
    let pcm = recording();
    let dir = models().join("asr");
    let edge_model = TokenRecognizer::load(&dir).unwrap();
    let vocab = Vocabulary::load(&dir).unwrap();
    let mut synth = tts();
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let edge = bind(a.clone(), &b).await;
    let mut worker = bind(b, &a).await;
    let (mut output, mut ring) = renderer();
    let signal = output.playback_signal();
    let mut chunks = 0;
    let mut ids = 0;
    let mut previous = 0;
    let addr = worker.addr();
    let (result, served) = timeout(Duration::from_secs(15), async {
        tokio::join!(
            allow(&pcm, &edge, &addr).queue(&edge_model, &mut output, |p| {
                assert_eq!(*p.source, Source::PairedWorker);
                assert!(REFERENCE.starts_with(p.text) && p.samples > previous);
                assert!(p.new_tokens > 0);
                previous = p.samples;
                chunks += 1;
                ids += p.new_tokens;
                assert_eq!(ring.available(), 0, "a partial reached playback");
            }),
            worker.serve_one(&vocab)
        )
    })
    .await
    .unwrap();
    served.unwrap();
    let result = result.unwrap();
    assert_eq!(result.source, Source::PairedWorker);
    assert_eq!(result.text, REFERENCE);
    assert!(chunks >= 2 && ids > chunks);

    assert_eq!(output.pump(&mut synth, 1.).unwrap(), 0);
    output.lease_acquired(EPOCH - 1, LeaseId::new());
    assert_eq!(output.pump(&mut synth, 1.).unwrap(), 0);
    assert_eq!(ring.available(), 0);
    output.lease_acquired(EPOCH, LeaseId::new());
    let mut played = Vec::new();
    let mut buf = [0.; 1024];
    loop {
        let offered = output.pump(&mut synth, 1.).unwrap();
        let n = signal.render(&mut ring, &mut buf).samples;
        assert_eq!(n, offered);
        if n == 0 {
            break;
        }
        played.extend_from_slice(&buf[..n]);
        assert!(played.len() <= 60 * SAMPLE_RATE_HZ as usize);
    }
    assert!(ring.dropped() == 0 && played.iter().any(|&s| f32::abs(s) > 0.05));
    let mono = resample_mono(&played, SAMPLE_RATE_HZ, 16_000).unwrap();
    let heard = ZipformerAsr::load_ort(&dir).unwrap().transcribe(&mono).unwrap();
    // Independent inference on the PCM actually consumed, not producer accounting.
    assert!(
        heard.starts_with("I HEARD") && heard.contains("YELLOW") && heard.contains("QUARTER"),
        "readback words missing from rendered Kokoro samples"
    );
    eprintln!(
        "verified {chunks} incremental token chunks ({ids} IDs before final), rendered {} samples",
        played.len()
    );
    worker.close().await;
    edge.close().await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    Cut,
    Stall,
    Caption,
    Digest,
    Pronunciation,
    Tail,
}

#[tokio::test]
async fn split_inference_tests_invalidation_wakes_a_blocked_peer_read_without_a_reply_or_timer() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    #[derive(Default)]
    struct Woken(AtomicBool);
    impl Wake for Woken {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Release);
        }
    }
    let pcm = recording();
    let model = TokenRecognizer::load(&models().join("asr")).unwrap();
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let edge = bind(a, &b).await;
    let worker = Endpoint::builder(Minimal)
        .secret_key(b)
        .alpns(vec![b"eon/readback/zipformer-en-2023-06-26/kokoro-v1/1".to_vec()])
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let (mut output, ring) = renderer();
    let revoke = output.revocation();
    let mut readback =
        Box::pin(allow(&pcm, &edge, &worker.addr()).queue(&model, &mut output, |_| panic!("no verified reply")));
    let (conn, _send, _recv) = timeout(Duration::from_secs(8), async {
        tokio::select! {
            result = &mut readback => panic!("readback ended before withheld reply: {}", result.is_ok()),
            streams = async {
                let conn = worker.accept().await.unwrap().await.unwrap();
                let (send, mut recv) = conn.accept_bi().await.unwrap();
                let mut header = [0; 18]; // nonce + first token frame header
                recv.read_exact(&mut header).await.unwrap();
                assert!(header[16] == 1 && (1..=64).contains(&header[17]));
                recv.read_exact(&mut vec![0; usize::from(header[17]) * 2]).await.unwrap();
                (conn, send, recv)
            } => streams
        }
    })
    .await
    .unwrap();

    // Replace the task waker while the authenticated peer withholds its first
    // reply. No yield, socket close, timer advance or preview callback can
    // rescue the pending read between these two manual polls.
    let notified = Arc::new(Woken::default());
    let waker = Waker::from(notified.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(readback.as_mut().poll(&mut cx).is_pending());
    revoke.invalidate(EPOCH);
    assert!(notified.0.load(Ordering::Acquire));
    assert!(matches!(
        readback.as_mut().poll(&mut cx),
        Poll::Ready(Err(EonError::ValidationError(_)))
    ));
    drop(readback);
    timeout(Duration::from_secs(2), conn.closed()).await.unwrap();
    assert_eq!(ring.available(), 0);
    worker.close().await;
    edge.close().await;
}

// Independent wire implementation. Parses ONLY the declared ID frames, checks
// the reference sentence as it grows, and rejects trailing bytes before final.
// Generates correctly bound but deliberately untrustworthy responses.
async fn faulty_peer(endpoint: &Endpoint, edge: &SecretKey, fault: Fault) -> usize {
    let table = std::fs::read_to_string(models().join("asr/tokens.txt")).unwrap();
    let table: HashMap<u16, _> = table
        .lines()
        .map(|l| {
            let (piece, id) = l.rsplit_once(' ').unwrap();
            (id.parse().unwrap(), piece.replace('▁', " "))
        })
        .collect();
    let conn = endpoint.accept().await.unwrap().await.unwrap();
    assert_eq!(conn.remote_id(), edge.public());
    let (mut send, mut recv) = conn.accept_bi().await.unwrap();
    let mut nonce = [0; 16];
    recv.read_exact(&mut nonce).await.unwrap();
    let mut digest = blake3::Hasher::new();
    digest.update(&nonce);
    let mut text = String::new();
    for frame in 1..=17 {
        let mut header = [0; 2];
        recv.read_exact(&mut header).await.unwrap();
        let [kind, count] = header;
        assert!((1..=2).contains(&kind) && count <= 64 && (kind == 2 || count > 0));
        digest.update(&header);
        for _ in 0..count {
            let mut bytes = [0; 2];
            recv.read_exact(&mut bytes).await.unwrap();
            digest.update(&bytes);
            text.push_str(&table[&u16::from_be_bytes(bytes)]);
        }
        let mut caption = text.trim().as_bytes().to_vec();
        assert!(REFERENCE.as_bytes().starts_with(&caption));
        let final_step = kind == 2;
        let mut phones = Vec::new();
        if final_step {
            assert_eq!(caption, REFERENCE.as_bytes());
            assert!(recv.read(&mut [0]).await.unwrap().is_none());
            // Same length and format as a legitimate pronunciation; one ID differs.
            phones = eon_voice::tts::KokoroFrontend::load()
                .unwrap()
                .prepare(&format!("I heard {}.", REFERENCE.to_ascii_lowercase()))
                .unwrap()
                .ids()
                .to_vec();
            if fault == Fault::Pronunciation {
                phones[0] ^= 1;
            }
        }
        let mut hash = *digest.finalize().as_bytes();
        if frame == 2 {
            match fault {
                Fault::Cut => {
                    conn.close(0u32.into(), b"partition");
                    return frame;
                }
                Fault::Stall => {
                    // The authenticated connection stays up, but a worker stops
                    // responding mid-sentence. The edge must close it and finish.
                    conn.closed().await;
                    return frame;
                }
                Fault::Digest => hash[0] ^= 1,
                Fault::Caption => caption[0] = b'Z',
                _ => {}
            }
        }
        let mut reply = hash.to_vec();
        reply.extend_from_slice(&(caption.len() as u16).to_be_bytes());
        reply.extend_from_slice(&(phones.len() as u16).to_be_bytes());
        reply.extend(caption);
        reply.extend(phones);
        if final_step && fault == Fault::Tail {
            reply.push(0);
        }
        send.write_all(&reply).await.unwrap();
        if final_step || (frame == 2 && matches!(fault, Fault::Digest | Fault::Caption)) {
            send.finish().unwrap();
            conn.closed().await;
            return frame;
        }
    }
    panic!("worker request exceeded frame limit");
}

#[tokio::test]
async fn split_inference_tests_discard_mismatch_partition_and_revoke_queued_pcm() {
    let pcm = recording();
    let model = TokenRecognizer::load(&models().join("asr")).unwrap();
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let edge = bind(a.clone(), &b).await;
    let worker = Endpoint::builder(Minimal)
        .secret_key(b)
        .alpns(vec![b"eon/readback/zipformer-en-2023-06-26/kokoro-v1/1".to_vec()])
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let addr = worker.addr();
    let mut synth = tts();
    for fault in [
        Fault::Cut,
        Fault::Stall,
        Fault::Caption,
        Fault::Digest,
        Fault::Pronunciation,
        Fault::Tail,
    ] {
        let (mut output, mut ring) = renderer();
        let mut verified = 0;
        let (result, frames) = timeout(Duration::from_secs(15), async {
            tokio::join!(
                allow(&pcm, &edge, &addr).queue(&model, &mut output, |p| {
                    if *p.source == Source::PairedWorker {
                        verified += 1;
                    }
                    assert!(REFERENCE.starts_with(p.text));
                }),
                faulty_peer(&worker, &a, fault)
            )
        })
        .await
        .unwrap();
        let result = result.unwrap();
        assert_eq!(result.text, REFERENCE);
        assert!(verified > 0 && frames >= 2);
        assert_eq!(
            result.source,
            match fault {
                Fault::Cut => Source::LocalOnChannelError,
                Fault::Stall => Source::LocalOnTimeout,
                _ => Source::LocalOnInvalidReply,
            }
        );
        if fault == Fault::Pronunciation {
            output.lease_acquired(EPOCH, LeaseId::new());
            let n = output.pump(&mut synth, 1.).unwrap();
            assert!(n > 0 && n == ring.available());
            output.revocation().invalidate(EPOCH);
            let mut buf = [1.; 1024];
            let rendered = output.playback_signal().render(&mut ring, &mut buf);
            assert!(rendered.acknowledged && rendered.flushed == n && rendered.samples == 0);
            assert_eq!(buf, [0.; 1024]);
            assert_eq!(output.pump(&mut synth, 1.).unwrap(), 0);
        }
    }
    worker.close().await;
    let (mut output, _) = renderer();
    let offline = allow(&pcm, &edge, &addr)
        .queue(&model, &mut output, |_| {})
        .await
        .unwrap();
    assert!(matches!(
        offline.source,
        Source::LocalOnChannelError | Source::LocalOnTimeout
    ));
    assert_eq!(offline.text, REFERENCE);
    edge.close().await;
}

#[tokio::test]
async fn split_inference_tests_pairing_is_not_a_recording_or_playback_grant() {
    let pcm = recording();
    let dir = models().join("asr");
    let model = TokenRecognizer::load(&dir).unwrap();
    let vocab = Vocabulary::load(&dir).unwrap();
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let edge = bind(a.clone(), &b).await;
    let mut worker = bind(b, &a).await;
    let addr = worker.addr();
    let (mut output, ring) = renderer();
    for bad in [&[][..], &vec![0; MAX_SAMPLES + 1]] {
        assert!(matches!(
            allow(bad, &edge, &addr).queue(&model, &mut output, |_| {}).await,
            Err(EonError::ValidationError(_))
        ));
    }
    assert!(matches!(
        allow(&pcm, &edge, &edge.addr())
            .queue(&model, &mut output, |_| {})
            .await,
        Err(EonError::SecurityViolation(_))
    ));
    // Pinning the workstation on an unrelated edge does not authorize that
    // edge to use the service. Refuse it before sending even a caption.
    let stranger = TokenPeer::bind((Ipv4Addr::LOCALHOST, 0).into(), SecretKey::generate(), addr.id)
        .await
        .unwrap();
    let (stranger_result, refused) = timeout(Duration::from_secs(10), async {
        tokio::join!(
            allow(&[0; 1600], &stranger, &addr).queue(&model, &mut output, |_| panic!("unexpected caption")),
            worker.serve_one(&vocab)
        )
    })
    .await
    .unwrap();
    assert!(matches!(refused, Err(EonError::SecurityViolation(_))));
    let silent = stranger_result.unwrap();
    assert!(matches!(
        silent.source,
        Source::LocalOnChannelError | Source::LocalOnTimeout
    ));
    assert!(silent.text.is_empty());
    stranger.close().await;
    let private = timeout(Duration::from_secs(15), async {
        tokio::select! {
            r = Readback::LocalOnly(&pcm).queue(&model, &mut output, |_| {}) => r.unwrap(),
            _ = worker.serve_one(&vocab) => panic!("sent private utterance to paired peer"),
        }
    })
    .await
    .unwrap();
    assert_eq!(private.source, Source::LocalOnly);
    assert_eq!(private.text, REFERENCE);

    // The operator interrupts during an in-flight authorized utterance. Even
    // an already-held lease must never turn a verified *partial* into speech.
    let (mut fresh, fresh_ring) = renderer();
    fresh.lease_acquired(EPOCH, LeaseId::new());
    let revoke = fresh.revocation();
    let (interrupted, served) = timeout(Duration::from_secs(15), async {
        tokio::join!(
            allow(&pcm, &edge, &addr).queue(&model, &mut fresh, |p| {
                assert_eq!(*p.source, Source::PairedWorker);
                revoke.invalidate(EPOCH);
            }),
            worker.serve_one(&vocab)
        )
    })
    .await
    .unwrap();
    assert!(interrupted.is_err() && served.is_err());
    assert_eq!(fresh.pump(&mut tts(), 1.).unwrap(), 0);
    assert_eq!(fresh_ring.available(), 0);
    assert_eq!(ring.available(), 0);
    worker.close().await;
    edge.close().await;
}
