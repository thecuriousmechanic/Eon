//! Source-only adapter for scripts/eval/polyglot_repair.py. Never runs source.
use std::io::{self, Read};

use eon_cyber::benign_repair::propose;
use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    io::stdin().take(65537).read_to_end(&mut buf)?;
    if buf.len() > 65536 {
        return Err("diagnostic input exceeds budget".into());
    }
    let request: Value = serde_json::from_slice(&buf)?;
    let result = propose(
        request["language"].as_str().ok_or("missing dialect")?,
        request["source"].as_str().ok_or("missing source")?,
        &request["diagnostic"],
    );
    println!(
        "{}",
        result.map_or(Value::Null, |(rule, source)| json!({"rule": rule, "source": source}))
    );
    Ok(())
}
