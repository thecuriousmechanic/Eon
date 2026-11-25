//! A per-process CRDT replica that the UI can write to and export from.
//!
//! The replica's writer ID is chosen at boot, so two server runs never share one. The view lists only
//! keys this process wrote. Keys that arrive through [`Mesh::apply`] are readable by name but are not
//! enumerable, because the replica exposes no key listing.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use eon_mesh::sync::Replica;
use eon_primitives::{EonError, EonResult};
use serde::Serialize;

const MAX_KEY_BYTES: usize = 128;
const MAX_VALUE_BYTES: usize = 4096;

pub struct Mesh {
    replica: Replica,
    written: BTreeSet<String>,
}

#[derive(Debug, Serialize)]
pub struct Entry {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct MeshView {
    pub version: String,
    pub entries: Vec<Entry>,
}

impl Mesh {
    pub fn new() -> EonResult<Self> {
        let peer = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            .max(1);
        Ok(Self {
            replica: Replica::new(peer)?,
            written: BTreeSet::new(),
        })
    }

    pub fn set(&mut self, key: &str, value: &str) -> EonResult<()> {
        validate_key(key)?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(invalid(format!("value is over {MAX_VALUE_BYTES} bytes")));
        }
        self.replica.set(key, value)?;
        self.written.insert(key.to_owned());
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<Entry> {
        self.replica.get(key).map(|value| Entry {
            key: key.to_owned(),
            value,
        })
    }

    pub fn export_hex(&self) -> EonResult<String> {
        Ok(hex_encode(&self.replica.export_all()?))
    }

    pub fn apply_hex(&mut self, hex: &str) -> EonResult<()> {
        let bytes = hex_decode(hex)?;
        self.replica.apply(&bytes)
    }

    pub fn view(&self) -> MeshView {
        let entries = self
            .written
            .iter()
            .filter_map(|key| self.get(key))
            .collect();
        MeshView {
            version: hex_encode(&self.replica.version()),
            entries,
        }
    }
}

fn validate_key(key: &str) -> EonResult<()> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(invalid(format!("key must be 1 to {MAX_KEY_BYTES} bytes")));
    }
    if key.chars().any(char::is_control) {
        return Err(invalid("key must not contain control characters"));
    }
    Ok(())
}

fn invalid(reason: impl Into<String>) -> EonError {
    EonError::ValidationError(reason.into())
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0xf) as usize] as char);
    }
    out
}

pub fn hex_decode(text: &str) -> EonResult<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return Err(invalid("hex input has an odd number of digits"));
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = nibble(pair[0])?;
            let low = nibble(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn nibble(c: u8) -> EonResult<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(invalid("hex input contains a non-hex character")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_rejects_bad_input() {
        assert_eq!(
            hex_decode(&hex_encode(&[0, 15, 255])).unwrap(),
            vec![0, 15, 255]
        );
        assert!(hex_decode("abc").is_err());
        assert!(hex_decode("zz").is_err());
    }

    #[test]
    fn set_validates_keys_and_values_then_reads_back() {
        let mut mesh = Mesh::new().unwrap();
        assert!(mesh.set("", "v").is_err());
        assert!(mesh.set("bad\nkey", "v").is_err());
        assert!(mesh.set("ok", &"x".repeat(MAX_VALUE_BYTES + 1)).is_err());
        mesh.set("greeting", "hello").unwrap();
        assert_eq!(mesh.get("greeting").unwrap().value, "hello");
        assert_eq!(mesh.view().entries.len(), 1);
    }

    #[test]
    fn export_applies_into_a_second_replica() {
        let mut source = Mesh::new().unwrap();
        source.set("a", "1").unwrap();
        let update = source.export_hex().unwrap();
        let mut target = Mesh::new().unwrap();
        target.apply_hex(&update).unwrap();
        assert_eq!(target.get("a").unwrap().value, "1");
    }
}
