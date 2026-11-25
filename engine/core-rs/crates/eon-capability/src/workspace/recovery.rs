use super::*;
use eon_foundation::journal::Frame;

// A compact v1 record with even two u64::MAX identifiers is under 512 bytes.
const MAX_PREPARATION_BYTES: usize = 512;

impl Workspace {
    /// Restore the file contents from one v1 preparation. The host must select
    /// `prepared` from replay of its trusted journal, not accept a frame from an
    /// untrusted caller: v1 records are not signed. No journal scan is performed here.
    ///
    /// A preparation is not proof of commit. The path in this pinned workspace
    /// must still contain the recorded after-image, whose digest the operator
    /// supplies independently. Missing or divergent targets are never overwritten.
    /// Recovery is itself a normal journaled edit, with its own private preimage
    /// and receipt. It restores neither directories nor historical permissions.
    pub fn restore_preimage(
        &self,
        path: &str,
        expected_current_sha256: [u8; 32],
        prepared: &Frame,
        journal: &GroupCommitJournal,
    ) -> Result<EditReceipt> {
        validate(path)?;
        let invalid = || WorkspaceError::Invalid("invalid or mismatched workspace preparation");
        if prepared.seq_no == 0
            || prepared.flags != 0
            || prepared.payload.len() > MAX_PREPARATION_BYTES
        {
            return Err(invalid());
        }
        let record: serde_json::Value =
            serde_json::from_slice(&prepared.payload).map_err(|_| invalid())?;
        let id = record["snapshot_id"].as_str().ok_or_else(invalid)?;
        let before = record["before_sha256"].as_str().ok_or_else(invalid)?;
        if !id
            .strip_prefix("preimage-")
            .is_some_and(|s| lower_hex(s, 32))
            || !lower_hex(before, 64)
        {
            return Err(invalid());
        }
        let root = self.root.metadata()?;
        owned_unshared(&root)?;
        let expected = preparation(&root, path, before, expected_current_sha256, id);
        // The writer emits compact, unescaped ASCII. Checking the encoded length
        // also rejects duplicate keys lost during JSON decoding, irrespective of key order.
        if record != expected || prepared.payload.len() != expected.to_string().len() {
            return Err(invalid());
        }

        let store = self.open_snapshot_directory()?; // Do not create a missing store.
        let (file, meta) = open_regular_at(&store, id, OFlags::RDONLY, |meta| {
            owned_unshared(meta)?;
            if meta.mode() & 0o7777 != 0o400 {
                return Err(WorkspaceError::Invalid("not a private read-only preimage"));
            }
            Ok(())
        })?;
        let bytes = read_edit_bytes(&file, &meta)?;
        if hex_digest(digest(&bytes)) != before
            || !same_version(&meta, &beneath(&store, id, OFlags::PATH)?.metadata()?)
            || !same_inode(
                &store.metadata()?,
                &beneath(&self.root, STORE, DIR)?.metadata()?,
            )
        {
            return Err(WorkspaceError::Conflict);
        }
        self.edit(path, expected_current_sha256, &bytes, journal)
    }
}

fn lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}
