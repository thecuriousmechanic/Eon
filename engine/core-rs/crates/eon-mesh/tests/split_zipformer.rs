//! Real Zipformer over two loopback-only QUIC endpoints. With `native-zipformer`
//! this gate fails (never skips) if the pinned assets or native runtime are missing.
//! Bootstrap: `python3 scripts/download_models.py --ids voice-test-zipformer-0
//! zipformer-en-2023-06-26-{encoder-int8,decoder,joiner-int8,tokens}` (shell brace expansion).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eon_mesh::EonError;
use eon_mesh::split_inference::zipformer::{MAX_SAMPLES, PinnedZipformer, Source, SpeechPeer, Utterance};
use iroh::endpoint::{Endpoint, presets::Minimal};
use iroh::{EndpointAddr, RelayMode, SecretKey};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use tokio::time::timeout;

const V2: &[u8] = b"eon/asr/zipformer-en-2023-06-26/2";

fn asset(env: &str, default: &str) -> PathBuf {
    std::env::var_os(env)
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join(default))
}

fn release_pcm() -> Vec<i16> {
    let path = asset("EON_ZIPFORMER_TEST_WAV_0", "../eon-voice/tests/fixtures/0.wav");
    let wav = std::fs::read(path).expect("fetch voice-test-zipformer-0 from scripts/models.lock.json");
    assert!(
        format!("{:x}", Sha256::digest(&wav)) == "6bc58a4efdf20daac252b6b1502632601a71efe0308f6757dc1eda34891a7e4f",
        "expected the pinned 16 kHz mono PCM16 fixture"
    );
    // This exact RIFF file has a canonical 44-byte header.
    wav[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&s| i16::from_le_bytes(s))
        .collect()
}

fn matches_reference(text: &str) -> bool {
    // SHA-256 of the UTF-8 words on the `0.wav` line in the release's
    // test_wavs/trans.txt (without filename, delimiter, or newline).
    format!("{:x}", Sha256::digest(text)) == "c9dc6ebbeac357ed5b118cf0d8323430b6a8ec5368f64337a780420eaf6a479c"
}

async fn bind(key: SecretKey, paired: &SecretKey) -> SpeechPeer {
    SpeechPeer::bind((Ipv4Addr::LOCALHOST, 0).into(), key, paired.public())
        .await
        .unwrap()
}

fn allow<'a>(pcm: &'a [i16], edge: &'a SpeechPeer, worker: &EndpointAddr) -> Utterance<'a> {
    Utterance::AllowPairedWorker {
        pcm,
        edge,
        worker: worker.clone(),
    }
}

#[tokio::test]
async fn split_inference_tests_release_speech_offloads_then_runs_locally_after_disconnect() {
    let dir = asset("EON_ZIPFORMER_DIR", "../../../../perception/models/asr");
    let missing = dir.join("absent-for-negative-test");
    assert!(PinnedZipformer::load(&missing).is_err());
    let workstation = PinnedZipformer::load(&dir).expect("load the verified Zipformer graphs and linked ORT");
    let pcm = release_pcm();
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let edge = bind(a.clone(), &b).await;
    let mut worker = bind(b, &a).await;
    let addr = worker.addr();
    assert!(addr.ip_addrs().any(|a| a.ip().is_loopback()));

    let mut previous = String::new();
    let mut previous_samples = 0;
    let (remote, served) = timeout(Duration::from_secs(12), async {
        tokio::join!(
            allow(&pcm, &edge, &addr).transcribe_with_previews(None, |p| {
                assert!(previous_samples < p.samples && p.samples < pcm.len());
                assert!(p.text.starts_with(&previous) && p.text.len() > previous.len());
                previous = p.text.to_owned();
                previous_samples = p.samples;
            }), // no edge model exists yet
            worker.serve_one(Some(&workstation))
        )
    })
    .await
    .expect("loopback RPC deadline");
    served.unwrap();
    let remote = remote.unwrap();
    assert_eq!(remote.source, Source::PairedWorker);
    assert!(!previous.is_empty() && remote.text.starts_with(&previous));
    assert!(
        matches_reference(&remote.text),
        "worker ASR disagrees with release transcript"
    );

    drop(workstation);
    let local = PinnedZipformer::load(&dir).expect("independent local ASR");
    // The local session has never seen the recording. It must not reuse the
    // previous remote transcript when the worker cannot serve a different input.
    let silence = [0; 16_000];
    let (fallback, served) = timeout(Duration::from_secs(12), async {
        tokio::join!(
            allow(&silence, &edge, &addr).transcribe(Some(&local)),
            worker.serve_one(None)
        )
    })
    .await
    .unwrap();
    assert!(matches!(served, Err(EonError::CapabilityNotFound { .. })));
    let fallback = fallback.unwrap();
    assert_eq!(fallback.source, Source::LocalOnChannelError);
    assert!(fallback.text.is_empty());

    worker.close().await; // actually close the endpoint; keep the same authorized destination and route
    let offline = timeout(
        Duration::from_secs(12),
        allow(&pcm, &edge, &addr).transcribe(Some(&local)),
    )
    .await
    .expect("bounded fallback")
    .unwrap();
    assert!(matches!(
        offline.source,
        Source::LocalOnTimeout | Source::LocalOnChannelError
    ));
    assert!(
        matches_reference(&offline.text),
        "local ASR disagrees with release transcript"
    );
    let private = Utterance::LocalOnly(&silence).transcribe(Some(&local)).await.unwrap();
    assert_eq!(private.source, Source::LocalOnly);
    assert!(private.text.is_empty());
    edge.close().await;
}

#[tokio::test]
async fn split_inference_tests_partition_after_first_preview_restarts_from_original_pcm() {
    let dir = asset("EON_ZIPFORMER_DIR", "../../../../perception/models/asr");
    let workstation = PinnedZipformer::load(&dir).unwrap();
    let local = PinnedZipformer::load(&dir).unwrap();
    let pcm = release_pcm();
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let edge = bind(a.clone(), &b).await;
    let mut worker = bind(b, &a).await;
    let addr = worker.addr();

    let (cut, preview_seen) = oneshot::channel();
    let mut cut = Some(cut);
    let mut first = None;
    let (result, ()) = timeout(Duration::from_secs(12), async {
        tokio::join!(
            allow(&pcm, &edge, &addr).transcribe_with_previews(Some(&local), |p| {
                if let Some(cut) = cut.take() {
                    assert!(!p.text.is_empty() && p.samples < pcm.len() / 2);
                    first = Some(p.text.to_owned());
                    cut.send(()).unwrap();
                }
            }),
            async {
                tokio::select! {
                    biased;
                    observed = preview_seen => observed.unwrap(),
                    _ = worker.serve_one(Some(&workstation)) => panic!("worker ended before partition"),
                }
                worker.close().await;
            }
        )
    })
    .await
    .expect("mid-utterance fallback deadline");
    let final_result = result.unwrap();
    assert!(matches!(
        final_result.source,
        Source::LocalOnChannelError | Source::LocalOnTimeout
    ));
    assert!(matches_reference(&final_result.text));
    assert!(first.unwrap().len() < final_result.text.len());
    edge.close().await;
}

async fn wire_endpoint(key: SecretKey) -> Endpoint {
    Endpoint::builder(Minimal)
        .secret_key(key)
        .alpns(vec![V2.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap()
}

#[tokio::test]
async fn split_inference_tests_real_worker_emits_text_with_request_suffix_withheld() {
    let model = PinnedZipformer::load(&asset("EON_ZIPFORMER_DIR", "../../../../perception/models/asr")).unwrap();
    let pcm = release_pcm();
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let mut worker = bind(b.clone(), &a).await;
    let addr = worker.addr();
    let edge = wire_endpoint(a).await;
    let ((), served) = timeout(Duration::from_secs(12), async {
        tokio::join!(
            async {
                let conn = edge.connect(addr, V2).await.unwrap();
                assert_eq!(conn.remote_id(), b.public());
                let (mut send, mut recv) = conn.open_bi().await.unwrap();
                send.write_all(&(pcm.len() as u32).to_be_bytes()).await.unwrap();
                let mut hash = blake3::Hasher::new();
                let mut got_words = false;
                // This independent wire client NEVER sends the second half or
                // FIN before reading real nonempty text. Buffered post-FIN
                // callbacks could not pass this test.
                for chunk in pcm[..pcm.len() / 2].as_chunks::<8_000>().0 {
                    let bytes: Vec<_> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                    send.write_all(&bytes).await.unwrap();
                    hash.update(&bytes);
                    let mut header = [0; 35];
                    recv.read_exact(&mut header).await.unwrap();
                    assert_eq!(header[0], 1); // step, not final
                    assert_eq!(&header[1..33], hash.finalize().as_bytes());
                    let len = u16::from_be_bytes([header[33], header[34]]) as usize;
                    assert!(len <= 2048);
                    let mut text = vec![0; len];
                    recv.read_exact(&mut text).await.unwrap();
                    if !text.is_empty() {
                        assert!("AFTER EARLY NIGHTFALL".starts_with(std::str::from_utf8(&text).unwrap()));
                        got_words = true;
                        break;
                    }
                }
                conn.close(0u32.into(), b"partition");
                assert!(got_words, "no upstream speech prefix before the suffix was sent");
            },
            worker.serve_one(Some(&model))
        )
    })
    .await
    .expect("worker waited for withheld input instead of streaming a partial");
    assert!(served.is_err()); // no final for a truncated request
    worker.close().await;
    edge.close().await;
}

#[tokio::test]
async fn split_inference_tests_invalid_authenticated_replies_are_not_results() {
    let local = PinnedZipformer::load(&asset("EON_ZIPFORMER_DIR", "../../../../perception/models/asr")).unwrap();
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let edge = bind(a.clone(), &b).await;
    let worker = wire_endpoint(b).await;
    let addr = worker.addr();
    let silence = [0; 8_001];
    let short = &silence[..1];
    let digest = blake3::hash(&[0, 0]);
    let frame = |kind, hash: blake3::Hash, length: u16, body: &[u8]| {
        [[kind].as_slice(), hash.as_bytes(), &length.to_be_bytes(), body].concat()
    };
    for (pcm, bad) in [
        (short, frame(2, blake3::hash(&[1, 0]), 5, b"HELLO")), // valid text, wrong utterance
        (short, frame(1, digest, 5, b"HELLO")),                // preview in place of final
        (short, frame(2, digest, 1, &[0xff])),                 // malformed UTF-8
        (short, frame(2, digest, 5, b"OPEN\n")),
        (short, frame(2, digest, 2049, b"")),
        (short, frame(2, digest, 0, &[0])),            // extra data after final
        (&silence[..], frame(1, digest, 5, b"HELLO")), // wrong prefix digest
        (&silence[..], frame(2, blake3::hash(&[0; 16_000]), 5, b"HELLO")), // premature final
    ] {
        let (result, ()) = timeout(Duration::from_secs(12), async {
            tokio::join!(
                allow(pcm, &edge, &addr).transcribe_with_previews(Some(&local), |_| panic!("unexpected preview")),
                async {
                    let conn = worker.accept().await.unwrap().await.unwrap();
                    assert_eq!(conn.remote_id(), a.public());
                    let (mut send, mut recv) = conn.accept_bi().await.unwrap();
                    let mut request = vec![0; 4 + 2 * pcm.len().min(8_000)];
                    recv.read_exact(&mut request).await.unwrap();
                    assert_eq!(&request[..4], &(pcm.len() as u32).to_be_bytes());
                    assert!(request[4..].iter().all(|&b| b == 0));
                    send.write_all(&bad).await.unwrap();
                    send.finish().unwrap();
                    conn.closed().await;
                }
            )
        })
        .await
        .expect("invalid reply fallback deadline");
        let result = result.unwrap();
        assert_eq!(result.source, Source::LocalOnInvalidReply);
        assert!(result.text.is_empty());
    }
    worker.close().await;
    edge.close().await;
}

#[tokio::test]
async fn split_inference_tests_zipformer_requires_the_pair_and_bounded_input() {
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let edge = bind(a.clone(), &b).await;
    let mut worker = bind(b, &a).await;
    let addr = worker.addr();
    assert!(matches!(
        allow(&[0], &edge, &edge.addr()).transcribe(None).await,
        Err(EonError::SecurityViolation(_))
    ));
    for bad in [&[][..], &vec![0; MAX_SAMPLES + 1]] {
        assert!(matches!(
            allow(bad, &edge, &addr).transcribe(None).await,
            Err(EonError::ValidationError(_))
        ));
    }
    assert!(matches!(
        Utterance::LocalOnly(&[0]).transcribe(None).await,
        Err(EonError::CapabilityNotFound { .. })
    ));
    let stranger = SpeechPeer::bind((Ipv4Addr::LOCALHOST, 0).into(), SecretKey::generate(), addr.id)
        .await
        .unwrap();
    let (result, served) = timeout(Duration::from_secs(12), async {
        tokio::join!(allow(&[0], &stranger, &addr).transcribe(None), worker.serve_one(None))
    })
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(matches!(served, Err(EonError::SecurityViolation(_))));
    stranger.close().await;
    worker.close().await;
    edge.close().await;
}
