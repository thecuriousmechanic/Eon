//! Opt-in end-to-end coverage; requires the locked model release and recorded
//! fixture, but no sound device. No transcript, physical or percentile claim.
#![cfg(feature = "native")]

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

#[test]
#[ignore = "requires locked speech, Silero, Zipformer, and release Kokoro"]
fn repeated_recording_with_a_late_continuation_is_an_early_reply_not_a_fast_latency()
-> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let speech = common::read_pcm16_mono_16k(&root.join("tests/fixtures/en.wav"));
    let mut take: Vec<i16> = speech.iter().map(|s| (s * 32768.) as i16).collect();
    take.resize(take.len() + 32_000, 0);
    let mut batch = Vec::new();
    for samples in [&take[..], &[0; 32_000], &take.repeat(2)] {
        batch.extend_from_slice(&u32::try_from(samples.len())?.to_le_bytes());
        for sample in samples {
            let s = sample.to_le_bytes();
            batch.extend_from_slice(&[s[0], s[1], s[0], s[1]]);
        }
    }
    batch.extend_from_slice(&0u32.to_le_bytes());
    let mut child = Command::new(env!("CARGO_BIN_EXE_eon-rig-turn"))
        .arg("--software-timing-session")
        .arg(root.join("../../../../perception/models"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(&batch)?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    let rows: Vec<Value> = std::str::from_utf8(&output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(rows.len(), 3);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row["take"].as_u64(), Some(index as u64 + 1));
    }
    assert_eq!(rows[0]["status"], "ok");
    let last = rows[0]["last_active_end_sample"].as_u64().unwrap();
    let end = rows[0]["endpoint_sample"].as_u64().unwrap();
    // This lower bound alone cannot verify latency, but rules out reporting
    // just the time to publish the prefix after the required silence wait.
    assert!(end >= last);
    assert!(rows[0]["latency_us"].as_u64().unwrap() * 16_000 >= (end - last) * 1_000_000);
    assert_eq!(
        rows[1],
        json!({
            "take":2, "status":"no_reply", "endpoint_sample":null, "last_active_end_sample":null, "latency_us":null
        })
    );
    assert_eq!(rows[2]["status"], "early_endpoint");
    assert_eq!(rows[2]["endpoint_sample"], end);
    assert!(rows[2]["last_active_end_sample"].as_u64().unwrap() > end);
    assert!(rows[2]["latency_us"].is_null());
    Ok(())
}
