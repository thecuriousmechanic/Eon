//! Owner-scoped late binding. Handles are safe to schedule; only the execution worker binds bytes.

use std::io::Write;
use std::process::ChildStdin;
use std::str::FromStr;

use eon_primitives::{EonError, EonResult, IdentityId};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::identity::{Identity, IdentityAuthority};
use crate::permissions::Permissions;

#[cfg(all(
    target_os = "linux",
    target_pointer_width = "64",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
#[path = "credentials/linux.rs"]
mod platform;

#[cfg(windows)]
#[path = "credentials/windows.rs"]
mod platform;

#[cfg(target_os = "android")]
#[path = "credentials/android.rs"]
mod platform;
#[cfg(all(target_os = "android", debug_assertions))]
pub use platform::check_android_vault_fixture;
#[cfg(target_os = "android")]
pub use platform::register_android_vm;

#[cfg(not(any(
    windows,
    target_os = "android",
    all(
        target_os = "linux",
        target_pointer_width = "64",
        any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "riscv64"
        )
    )
)))]
mod platform {
    use super::*;

    pub(super) fn open() -> EonResult<Box<dyn Store>> {
        Err(failed("no supported OS credential store"))
    }
}

const SECRET_SCHEME: &str = "secret://";
const MAX_SECRET_LEN: usize = 32_767; // Linux "user" key payload limit.

/// A vault alias, not a path or a bearer capability. Never put secret material in an alias.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CredentialHandle(String);

impl CredentialHandle {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CredentialHandle {
    type Err = EonError;

    fn from_str(uri: &str) -> EonResult<Self> {
        if uri.strip_prefix(SECRET_SCHEME).is_some_and(|key| {
            key.len() <= 128
                && key.split('/').all(|segment| {
                    !matches!(segment, "" | "." | "..")
                        && segment
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
                })
        }) {
            Ok(Self(uri.to_owned()))
        } else {
            Err(EonError::ValidationError(
                "invalid credential handle".into(),
            ))
        }
    }
}

/// Owned provisioned bytes. No formatting, cloning, serialization or public byte accessor.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct EphemeralSecret(Vec<u8>);

impl EphemeralSecret {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

#[derive(PartialEq, Eq, Hash)]
struct Slot(IdentityId, CredentialHandle);

// Private: callers cannot inject a plaintext store. Even the store only binds to child stdin.
trait Store: Send + Sync {
    fn put(&mut self, slot: Slot, secret: EphemeralSecret) -> EonResult<()>;
    fn bind(&self, slot: &Slot, stdin: ChildStdin) -> EonResult<()>;
    fn remove(&mut self, slot: &Slot) -> EonResult<()>;
}

fn write_to_stdin(mut stdin: ChildStdin, secret: &[u8]) -> EonResult<()> {
    stdin
        .write_all(secret)
        .map_err(|_| failed("cannot bind to child stdin"))
}

fn unavailable() -> EonError {
    EonError::SecurityViolation("credential unavailable to this identity".into())
}

fn failed(reason: &str) -> EonError {
    EonError::OperationFailed {
        operation: "credential broker".into(),
        reason: reason.into(),
    }
}

pub struct CredentialBroker {
    authority: IdentityAuthority,
    store: Box<dyn Store>,
}

impl CredentialBroker {
    /// Opens a process-lifetime OS vault. Failure never selects a plaintext fallback.
    pub fn new(authority: &IdentityAuthority) -> EonResult<Self> {
        Ok(Self {
            authority: authority.clone(),
            store: platform::open()?,
        })
    }

    fn slot(
        &self,
        caller: &Identity,
        required: Permissions,
        handle: &CredentialHandle,
    ) -> EonResult<Slot> {
        self.authority
            .authorize(caller, required)
            .map_err(|_| unavailable())?;
        Ok(Slot(caller.owner().clone(), handle.clone()))
    }

    /// Provision or rotate only in the authenticated owner's namespace.
    pub fn put(
        &mut self,
        caller: &Identity,
        handle: &CredentialHandle,
        secret: EphemeralSecret,
    ) -> EonResult<()> {
        let slot = self.slot(caller, Permissions::MANAGE_CREDENTIALS, handle)?;
        if !(1..=MAX_SECRET_LEN).contains(&secret.0.len()) {
            return Err(EonError::ValidationError("invalid credential size".into()));
        }
        self.store.put(slot, secret)
    }

    pub fn remove(&mut self, caller: &Identity, handle: &CredentialHandle) -> EonResult<()> {
        let slot = self.slot(caller, Permissions::MANAGE_CREDENTIALS, handle)?;
        self.store.remove(&slot)
    }

    /// The trusted worker chooses the child. Both the transient buffer and pipe are dropped
    /// on success or error; the broker cannot control what the receiving program does.
    pub fn bind_to_stdin(
        &self,
        caller: &Identity,
        handle: &CredentialHandle,
        stdin: ChildStdin,
    ) -> EonResult<()> {
        let slot = self.slot(
            caller,
            Permissions::USE_CREDENTIALS | Permissions::EXECUTE_PROCESS,
            handle,
        )?;
        self.store.bind(&slot, stdin)
    }

    /// Rejects any argv entry that names a secret; never echo a rejected argument.
    pub fn check_argv(args: &[&str]) -> EonResult<()> {
        match args.iter().find(|arg| arg.contains(SECRET_SCHEME)) {
            None => Ok(()),
            Some(_) => Err(EonError::SecurityViolation(
                "secret:// references are not allowed in argv; bind via stdin or fd".into(),
            )),
        }
    }
}

#[cfg(test)]
mod identity_and_credential_tests;
