use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use eon_foundation::owned_dir::OwnedDirectory;

use super::{MAX_OPEN, invalid};

pub(super) struct Provision {
    pub group: PathBuf,
    pub state: PathBuf,
    pub image_sha256: [u8; 32],
}

/// Pyright's import resolver enumerates module names on disk even for open
/// overlays. Only zero-byte markers ever go here; Landlock grants ReadDir,
/// not ReadFile. No real project paths, source, configs or packages are linked.
pub(super) struct ModuleNames {
    parent: OwnedDirectory,
    leaf: String,
    pub root: PathBuf,
    names: BTreeSet<String>,
}

impl ModuleNames {
    pub fn new(parent: &Path, workspace: &Path) -> io::Result<Self> {
        let parent = OwnedDirectory::open(parent, false)?;
        let path = parent.path().canonicalize()?;
        if path.starts_with(workspace) || workspace.starts_with(&path) {
            return Err(invalid("Pyright state must be separate from workspace"));
        }
        let leaf = format!(
            "eon-modules-{:016x}",
            getrandom::u64().map_err(|_| io::Error::other("no session nonce"))?
        );
        fs::DirBuilder::new()
            .mode(0o700)
            .create(parent.path().join(&leaf))?;
        Ok(Self {
            root: path.join(&leaf),
            parent,
            leaf,
            names: BTreeSet::new(),
        })
    }

    pub fn insert(&mut self, name: &str) -> io::Result<()> {
        if self.names.len() >= MAX_OPEN || self.names.contains(name) {
            return Err(invalid("duplicate or full Python module catalog"));
        }
        // The server's read-only filesystem case probe cannot create files.
        // Avoid aliases; admit only flat, lowercase ASCII Python module names.
        let stem = name
            .strip_suffix(".py")
            .or_else(|| name.strip_suffix(".pyi"))
            .unwrap_or("");
        if stem.is_empty()
            || stem.len() > 128
            || stem.as_bytes()[0].is_ascii_digit()
            || !stem
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        {
            return Err(invalid("expected a flat lowercase Python module"));
        }
        fs::File::create_new(self.parent.path().join(&self.leaf).join(name))?;
        self.names.insert(name.into());
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> io::Result<()> {
        if !self.names.contains(name) {
            return Err(invalid("unknown Python module"));
        }
        fs::remove_file(self.parent.path().join(&self.leaf).join(name))?;
        self.names.remove(name);
        Ok(())
    }
}

impl Drop for ModuleNames {
    fn drop(&mut self) {
        let root = self.parent.path().join(&self.leaf);
        for name in &self.names {
            let _ = fs::remove_file(root.join(name));
        }
        let _ = fs::remove_dir(root);
    }
}
