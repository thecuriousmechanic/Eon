use super::*;
use std::collections::BTreeMap;

pub(super) const MAX_ENTRIES: usize = 256;
pub(super) const MAX_DEPTH: usize = 16;
pub(super) const MAX_TOTAL_BYTES: usize = 4 * MAX_EDIT_BYTES;

#[derive(Debug)]
pub struct SnapshotReceipt {
    /// Owner-only object under the private snapshot store, not a read capability.
    pub snapshot_id: String,
    pub journal_seq: u64,
}

impl Workspace {
    /// Checkpoint the working tree of this already pinned root, before a series
    /// of edits. Skips `.git` entries; never runs Git or follows gitdir
    /// indirection. Only operator-owned regular files and directories are
    /// supported. An unsafe or oversized tree fails as a whole (256 entries,
    /// 16 components per path, 1 MiB per file, 4 MiB of input bytes total).
    /// Cooperating edits through handles to this root conflict during capture.
    ///
    /// The private v1 JSON object maps relative names to byte arrays (regular
    /// files) or null (directories, including empty ones). It captures contents,
    /// not permissions, history, or a Git index. Only the opaque object name,
    /// root identity and object digest go into the trusted journal. Retention
    /// and offline recovery are host concerns; there is no bulk read/checkout API.
    pub fn snapshot(&self, journal: &GroupCommitJournal) -> Result<SnapshotReceipt> {
        self.snapshot_with_record(|record| journal.append_durable(record))
    }

    pub(super) fn snapshot_with_record(
        &self,
        append: impl FnOnce(Vec<u8>) -> eon_foundation::EonResult<u64>,
    ) -> Result<SnapshotReceipt> {
        let root = self.root.metadata()?;
        owned_unshared(&root)?;
        let store = self.snapshot_directory()?;
        let _gate = EditLock::acquire(&store)?;
        let mut tree = Capture::default();
        tree.walk(self, &store.metadata()?, ".", 0)?;
        let encoded = serde_json::to_vec(&tree.contents).map_err(io::Error::other)?;
        let mut object = PrivateFile::create(&store, "tree-v1")?;
        let version = object.persist_bytes(&encoded, Mode::RUSR)?;
        store.sync_all()?;

        // Also detect observable changes by non-cooperating writers during the
        // walk. This is not a filesystem-wide atomic snapshot against such writers.
        for (path, before) in &tree.versions {
            self.unchanged(path, before)?;
        }
        self.check_snapshot_directory(&store)?;
        object.check_name(&version)?;
        let event = serde_json::json!({
            "type": "workspace.snapshot.v1",
            "root_dev": root.dev(),
            "root_ino": root.ino(),
            "snapshot_id": object.name,
            "sha256": hex_digest(digest(&encoded)),
        });
        // A lost durable acknowledgement can still leave a replayable frame.
        object.remove_on_drop = false;
        let journal_seq =
            append(event.to_string().into_bytes()).map_err(WorkspaceError::Journal)?;
        // The journal can block. An acknowledgement alone is not evidence that
        // the private name still identifies the checkpoint we are handing back.
        self.check_snapshot_directory(&store)?;
        object.check_name(&version)?;
        Ok(SnapshotReceipt {
            snapshot_id: object.name.clone(),
            journal_seq,
        })
    }
}

#[derive(Default)]
struct Capture {
    contents: BTreeMap<String, Option<Vec<u8>>>,
    versions: BTreeMap<String, Metadata>,
    total_bytes: usize,
}

impl Capture {
    fn walk(
        &mut self,
        workspace: &Workspace,
        store: &Metadata,
        path: &str,
        depth: usize,
    ) -> Result<()> {
        let dir = beneath(&workspace.root, path, DIR)?;
        let meta = dir.metadata()?;
        if same_inode(&meta, store) {
            return Err(WorkspaceError::Invalid("reserved workspace directory"));
        }
        owned_unshared(&meta)?;
        self.versions.insert(path.to_owned(), meta);
        for ent in fs::Dir::read_from(&dir)? {
            let ent = ent?;
            let name = ent
                .file_name()
                .to_str()
                .map_err(|_| WorkspaceError::Invalid("unsupported snapshot name"))?;
            if matches!(name, "." | ".." | ".git") || (depth == 0 && name == STORE) {
                continue;
            }
            if name.eq_ignore_ascii_case(".git") {
                return Err(WorkspaceError::Invalid("reserved snapshot name"));
            }
            if self.contents.len() >= MAX_ENTRIES || depth >= MAX_DEPTH {
                return Err(WorkspaceError::Invalid("snapshot tree exceeds limit"));
            }
            let child = if path == "." {
                name.to_owned()
            } else {
                format!("{path}/{name}")
            };
            validate(&child)?;
            // Resolve every data/dir open from the pinned root, not relative
            // to a traversed directory that could since have moved elsewhere.
            workspace.reject_store_alias(&child)?;
            let meta = beneath(&workspace.root, &child, OFlags::PATH)?.metadata()?;
            let data = if meta.is_dir() {
                None
            } else {
                let (file, before) =
                    open_regular_at(&workspace.root, &child, OFlags::RDONLY, |m| {
                        owned_unshared(m)?;
                        if m.mode() & 0o400 == 0 {
                            return Err(WorkspaceError::Invalid(
                                "snapshot requires readable files",
                            ));
                        }
                        Ok(())
                    })?;
                if !same_version(&meta, &before) {
                    return Err(WorkspaceError::Conflict);
                }
                if before.len() > (MAX_TOTAL_BYTES - self.total_bytes) as u64 {
                    return Err(WorkspaceError::Invalid("snapshot content exceeds limit"));
                }
                let bytes = read_edit_bytes(&file, &before)?;
                self.total_bytes += bytes.len();
                self.versions.insert(child.clone(), before);
                Some(bytes)
            };
            if self.contents.insert(child.clone(), data).is_some() {
                return Err(WorkspaceError::Conflict);
            }
            if meta.is_dir() {
                self.walk(workspace, store, &child, depth + 1)?;
            }
        }
        Ok(())
    }
}
