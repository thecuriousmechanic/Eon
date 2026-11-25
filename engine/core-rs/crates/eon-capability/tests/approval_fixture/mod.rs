use std::fs;
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;

use eon_capability::{
    EonResult,
    ifc::{
        labels::{ConfidentialityLevel, IntegrityLevel, SecurityLabel},
        rule_of_two::TaskProfile,
    },
    registry::{AllowedTool, CapabilityRegistry, DurableApprovals},
    wasm_tool::{MAX_WORKER_WALL_TIME, WasmWorker},
};
use eon_foundation::group_commit::GroupCommitJournal;

pub struct Fixture {
    pub root: PathBuf,
    pub executable: PathBuf,
    pub journal: Arc<GroupCommitJournal>,
    pub registry: CapabilityRegistry,
    _install: Install,
}

struct Install(PathBuf);

impl Drop for Install {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

pub fn operator() -> SecurityLabel {
    SecurityLabel::new(
        IntegrityLevel::Trusted,
        ConfidentialityLevel::Public,
        vec!["operator".into()],
    )
}

pub fn workflow() -> TaskProfile {
    TaskProfile {
        context: operator(),
        resolves_secret: false,
        reads_outside_sandbox: false,
        capabilities: vec![],
        control_parameters: vec![],
        air_gapped: false,
    }
}

impl Fixture {
    pub fn provisioned<const N: usize>(
        &self,
        entries: [AllowedTool; N],
        store: DurableApprovals,
    ) -> EonResult<CapabilityRegistry> {
        Ok(CapabilityRegistry::with_allowlist(entries, store)?
            .with_dispatch_journal(self.journal.clone())
            .with_wasm_worker(WasmWorker::new(&self.executable, MAX_WORKER_WALL_TIME)?))
    }
}

impl Deref for Fixture {
    type Target = CapabilityRegistry;
    fn deref(&self) -> &Self::Target {
        &self.registry
    }
}

impl DerefMut for Fixture {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.registry
    }
}

pub fn prepared<const N: usize>(entries: [AllowedTool; N]) -> EonResult<Fixture> {
    // Install under the test operator's home, independent of checkout/target
    // permissions. Production never looks up a worker relative to HOME.
    let home = std::env::home_dir().expect("test operator needs a home directory");
    let _home = eon_foundation::owned_dir::OwnedDirectory::open(&home, false).unwrap();
    let install = home.join(format!(".wasm-install-{:016x}", getrandom::u64().unwrap()));
    fs::DirBuilder::new().mode(0o700).create(&install).unwrap();
    let cleanup = Install(install.clone());
    let mut fixture = Fixture {
        root: install.join("approvals"),
        executable: install.join("eon-wasm-worker"),
        journal: Arc::new(GroupCommitJournal::spawn(install.join("journal"), 8)?),
        registry: CapabilityRegistry::new(),
        _install: cleanup,
    };
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&fixture.root)
        .unwrap();
    fs::copy(env!("CARGO_BIN_EXE_eon-wasm-worker"), &fixture.executable).unwrap();
    fs::set_permissions(&fixture.executable, fs::Permissions::from_mode(0o500)).unwrap();
    let store = DurableApprovals::open(&fixture.root)?;
    for entry in &entries {
        store.approve(entry)?;
    }
    fixture.registry = fixture.provisioned(entries, store)?;
    Ok(fixture)
}
