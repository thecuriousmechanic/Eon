use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use eon_primitives::{EonError, EonResult};
use rustix::fs::{self, Mode, OFlags, ResolveFlags};

use super::{invalid, protected_name, public_path};

const DIR: OFlags = OFlags::PATH.union(OFlags::DIRECTORY).union(OFlags::CLOEXEC);
const NO_LINKS: ResolveFlags = ResolveFlags::BENEATH.union(ResolveFlags::NO_SYMLINKS);
const WITHIN: ResolveFlags = NO_LINKS.union(ResolveFlags::NO_XDEV);

/// A pinned directory and exact paths the trusted host has classified as public.
/// Never derive this list from the objective. Names alone cannot classify secrets.
/// No glob, recursion, ambient root, or write interface.
pub struct ReadWorkspace {
    root: File,
    public: BTreeSet<String>,
}

impl ReadWorkspace {
    /// Path-based selection requires an absolute path through root/operator-owned,
    /// non-shared-writable directories. Even sticky shared ancestors are refused.
    pub fn open(root: &Path, public: &[&str]) -> EonResult<Self> {
        if !root.is_absolute() {
            return Err(invalid("workspace selection requires an absolute path"));
        }
        let mut dir: File = fs::open("/", DIR, Mode::empty()).map_err(failed)?.into();
        for component in root.components() {
            match component {
                Component::RootDir => continue,
                Component::Normal(name) if name.to_str().is_some_and(|s| !protected_name(s)) => {
                    trusted(&dir.metadata().map_err(failed)?)?;
                    dir = open_at(&dir, name, DIR, NO_LINKS)?;
                }
                _ => return Err(invalid("invalid or protected workspace ancestor")),
            }
        }
        Self::from_directory(dir, public)
    }

    /// Alternatively the host can supply an **independently trusted** directory
    /// capability. Do not obtain it by following an untrusted/shared pathname.
    pub fn from_directory(root: File, public: &[&str]) -> EonResult<Self> {
        let meta = root.metadata().map_err(failed)?;
        trusted(&meta)?;
        if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() {
            return Err(invalid("expected an operator-owned workspace directory"));
        }
        let mut paths = BTreeSet::new();
        for path in public {
            if !public_path(path) {
                return Err(invalid("invalid or protected public file selection"));
            }
            paths.insert((*path).to_owned());
        }
        Ok(Self {
            root,
            public: paths,
        })
    }

    fn authorize(&self, path: &str) -> EonResult<()> {
        if self.public.contains(path) {
            Ok(())
        } else {
            Err(EonError::SecurityViolation(
                "file is not approved for capture".into(),
            ))
        }
    }

    pub(super) fn root_identity(&self) -> EonResult<(u64, u64)> {
        let m = self.root.metadata().map_err(failed)?;
        Ok((m.dev(), m.ino()))
    }

    fn locate<'a>(&self, path: &'a str) -> EonResult<CheckedPath<'a>> {
        self.authorize(path)?;
        let mut dir = self.root.try_clone().map_err(failed)?;
        trusted(&dir.metadata().map_err(failed)?)?;
        let mut ancestors = Vec::new();
        let mut names = path.split('/');
        let leaf = names
            .next_back()
            .ok_or_else(|| invalid("missing file name"))?;
        for name in names {
            let child = open_at(&dir, OsStr::new(name), DIR, WITHIN)?;
            trusted(&child.metadata().map_err(failed)?)?;
            ancestors.push(dir);
            dir = child;
        }
        Ok(CheckedPath {
            parents: Parents { ancestors, dir },
            leaf,
        })
    }

    pub(super) fn source(&self, path: &str) -> EonResult<Source> {
        let source = self.locate(path)?.open_source()?;
        self.revalidate(path, &source)?;
        Ok(source)
    }

    pub(super) fn revalidate(&self, path: &str, source: &Source) -> EonResult<()> {
        let current = self.locate(path)?;
        if current.parents.ancestors.len() != source.parents.ancestors.len() {
            return Err(invalid("workspace ancestry changed during capture"));
        }
        for (now, pinned) in current.parents.iter().zip(source.parents.iter()) {
            let now = now.metadata().map_err(failed)?;
            let pinned = pinned.metadata().map_err(failed)?;
            trusted(&now)?;
            if (now.dev(), now.ino()) != (pinned.dev(), pinned.ino()) {
                return Err(invalid("workspace ancestry changed during capture"));
            }
        }
        source.check(&current.pin()?.metadata().map_err(failed)?)?;
        // Also require that the same leaf is still reachable in one kernel walk
        // beneath the root. This lookup is never used as the source of bytes.
        let anchored = open_at(&self.root, OsStr::new(path), OFlags::PATH, WITHIN)?;
        source.check(&anchored.metadata().map_err(failed)?)
    }
}

struct Parents {
    ancestors: Vec<File>,
    dir: File,
}

impl Parents {
    fn iter(&self) -> impl Iterator<Item = &File> {
        self.ancestors.iter().chain(std::iter::once(&self.dir))
    }
}

struct CheckedPath<'a> {
    parents: Parents,
    leaf: &'a str,
}

impl CheckedPath<'_> {
    fn pin(&self) -> EonResult<File> {
        open_at(
            &self.parents.dir,
            OsStr::new(self.leaf),
            OFlags::PATH,
            WITHIN,
        )
    }

    fn open_source(self) -> EonResult<Source> {
        // Open the leaf *from the checked parent*. O_PATH never invokes a
        // device/FIFO open handler. Reopen only this verified inode for I/O.
        let pin = self.pin()?;
        let before = pin.metadata().map_err(failed)?;
        trusted(&before)?;
        if !before.is_file() || before.nlink() != 1 {
            return Err(invalid("expected a singly linked regular workspace file"));
        }
        let procfd = fs::open("/proc/self/fd", DIR, Mode::empty()).map_err(failed)?;
        if fs::fstatfs(&procfd).map_err(failed)?.f_type != fs::PROC_SUPER_MAGIC {
            return Err(invalid("trusted procfs required to reopen workspace file"));
        }
        let file: File = fs::openat(
            procfd,
            pin.as_raw_fd().to_string(),
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(failed)?
        .into();
        let source = Source {
            file,
            before,
            parents: self.parents,
        };
        source.check(&source.file.metadata().map_err(failed)?)?;
        Ok(source)
    }
}

pub(super) struct Source {
    pub file: File,
    before: Metadata,
    parents: Parents,
}

impl Source {
    pub fn len(&self) -> u64 {
        self.before.len()
    }

    fn check(&self, now: &Metadata) -> EonResult<()> {
        trusted(now)?;
        let version = |m: &Metadata| {
            (
                m.dev(),
                m.ino(),
                m.nlink(),
                m.len(),
                m.ctime(),
                m.ctime_nsec(),
                m.mtime(),
                m.mtime_nsec(),
            )
        };
        if version(now) != version(&self.before) {
            return Err(invalid("workspace file changed during capture"));
        }
        Ok(())
    }
}

fn open_at(dir: &File, path: &OsStr, flags: OFlags, resolve: ResolveFlags) -> EonResult<File> {
    fs::openat2(dir, path, flags | OFlags::CLOEXEC, Mode::empty(), resolve)
        .map(File::from)
        .map_err(failed)
}

fn trusted(meta: &Metadata) -> EonResult<()> {
    if (meta.uid() != 0 && meta.uid() != rustix::process::geteuid().as_raw())
        || meta.mode() & 0o022 != 0
    {
        return Err(invalid(
            "workspace requires trusted ownership and no shared write access",
        ));
    }
    Ok(())
}

pub(super) fn failed(error: impl Into<io::Error>) -> EonError {
    EonError::OperationFailed {
        operation: "compiler.snapshot".into(),
        reason: error.into().to_string(),
    }
}

#[cfg(test)]
mod compiler_tests {
    use super::super::fixture::{Fixture, mkdir, write};
    use super::*;
    use std::fs::{Permissions, rename, set_permissions};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn synchronized_parent_replacement_cannot_redirect_the_leaf_open() {
        let fixture = Fixture::new();
        let root = fixture.root();
        let name = "src/File";
        mkdir(&root.join("src"));
        write(&root.join(name), b"original");
        let original = root.join(name).metadata().unwrap().ino();
        let workspace = ReadWorkspace::from_directory(File::open(&root).unwrap(), &[name]).unwrap();
        let checked = workspace.locate(name).unwrap();
        // Join between the checked directory walk and the leaf open, rather
        // than hoping a scheduling race happens during a stress loop.
        std::thread::scope(|s| {
            s.spawn(|| {
                rename(root.join("src"), root.join("moved")).unwrap();
                mkdir(&root.join("src"));
                write(&root.join(name), b"replacement");
                set_permissions(root.join("src"), Permissions::from_mode(0o770)).unwrap();
            })
            .join()
            .unwrap()
        });
        let source = checked.open_source().unwrap();
        assert_eq!(source.file.metadata().unwrap().ino(), original);
        assert!(workspace.revalidate(name, &source).is_err());
        assert!(workspace.source(name).is_err());
    }

    #[test]
    fn unchanged_leaf_and_parent_do_not_mask_a_changed_grandparent() {
        let fixture = Fixture::new();
        let root = fixture.root();
        let name = "src/sub/File";
        mkdir(&root.join("src"));
        mkdir(&root.join("src/sub"));
        write(&root.join(name), b"public");
        let workspace = ReadWorkspace::from_directory(File::open(&root).unwrap(), &[name]).unwrap();
        let source = workspace.source(name).unwrap();
        set_permissions(root.join("src"), Permissions::from_mode(0o770)).unwrap();
        assert!(workspace.revalidate(name, &source).is_err());
        set_permissions(root.join("src"), Permissions::from_mode(0o700)).unwrap();
        rename(root.join("src"), root.join("old")).unwrap();
        mkdir(&root.join("src"));
        rename(root.join("old/sub"), root.join("src/sub")).unwrap();
        assert!(source.check(&root.join(name).metadata().unwrap()).is_ok());
        assert!(workspace.revalidate(name, &source).is_err());
    }
}
