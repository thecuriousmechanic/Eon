//! Linux software keyring. Only serial numbers and lengths live in the broker between calls.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::{CString, c_long};
use std::io;
use std::process::ChildStdin;

use super::{EphemeralSecret, Slot, Store, failed, unavailable, write_to_stdin};
use eon_primitives::{EonResult, IdentityId};

// Linux UAPI: asm/unistd_64.h on x86_64, asm-generic/unistd.h on aarch64/riscv64.
// Only these LP64 ABIs are enabled. No keyutils helper process, argv, or shared library.
#[cfg(target_arch = "x86_64")]
mod nr {
    pub const ADD_KEY: super::c_long = 248;
    pub const KEYCTL: super::c_long = 250;
}
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
mod nr {
    pub const ADD_KEY: super::c_long = 217;
    pub const KEYCTL: super::c_long = 219;
}
const PROCESS_KEYRING: c_long = -2; // Not inherited across fork or exec.
#[repr(i64)]
enum ScalarOp {
    GetKeyringId = 0,
    Revoke = 3,
    SetPerm = 5,
    Unlink = 9,
}
const UPDATE: c_long = 2;
const READ: c_long = 11;
const POS_READ_WRITE_SEARCH: c_long = 0x0e00_0000;

unsafe extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
}

fn checked(result: c_long) -> EonResult<c_long> {
    if result < 0 {
        Err(failed(&format!(
            "Linux keyring: {}",
            io::Error::last_os_error()
        )))
    } else {
        Ok(result)
    }
}

fn scalar_op(op: ScalarOp, key: c_long, argument: c_long) -> EonResult<()> {
    // SAFETY: these four keyctl operations accept only scalar arguments on the enabled LP64 ABIs.
    checked(unsafe {
        syscall(
            nr::KEYCTL,
            op as c_long,
            key,
            argument,
            0 as c_long,
            0 as c_long,
        )
    })?;
    Ok(())
}

struct KernelKey {
    serial: c_long,
    len: usize,
}

impl KernelKey {
    fn create(secret: &EphemeralSecret) -> EonResult<Self> {
        // Never put aliases, owner labels or payloads in OS-visible descriptions.
        let name = CString::new(format!("eon:{}", IdentityId::new())).expect("ASCII key name");
        let placeholder = [0u8];
        // SAFETY: NUL-terminated type/name and one readable byte, all live through add_key.
        let serial = checked(unsafe {
            syscall(
                nr::ADD_KEY,
                c"user".as_ptr(),
                name.as_ptr(),
                placeholder.as_ptr(),
                placeholder.len(),
                PROCESS_KEYRING,
            )
        })?;
        let mut key = Self { serial, len: 0 };
        // Restrict access *before* installing any secret. No user/group/other or link grant.
        scalar_op(ScalarOp::SetPerm, serial, POS_READ_WRITE_SEARCH)?;
        key.update(secret)?;
        Ok(key)
    }

    fn update(&mut self, secret: &EphemeralSecret) -> EonResult<()> {
        // SAFETY: UPDATE copies exactly len readable bytes before the syscall returns.
        checked(unsafe {
            syscall(
                nr::KEYCTL,
                UPDATE,
                self.serial,
                secret.0.as_ptr(),
                secret.0.len(),
                0 as c_long,
            )
        })?;
        self.len = secret.0.len();
        Ok(())
    }

    fn load(&self) -> EonResult<EphemeralSecret> {
        // Allocate once at the authorized size; even a partial/failed read is wiped.
        let mut secret = EphemeralSecret::new(vec![0; self.len]);
        // SAFETY: READ gets an exclusively owned, initialized buffer of exactly len bytes.
        let read = checked(unsafe {
            syscall(
                nr::KEYCTL,
                READ,
                self.serial,
                secret.0.as_mut_ptr(),
                self.len,
                0 as c_long,
            )
        })?;
        if read as usize != self.len {
            return Err(failed("OS credential changed during binding"));
        }
        Ok(secret)
    }

    fn revoke(&self) -> EonResult<()> {
        scalar_op(ScalarOp::Revoke, self.serial, 0)
    }
}

impl Drop for KernelKey {
    fn drop(&mut self) {
        let _ = self.revoke();
        let _ = scalar_op(ScalarOp::Unlink, self.serial, PROCESS_KEYRING);
    }
}

#[derive(Default)]
struct LinuxKeyring(HashMap<Slot, KernelKey>);

pub(super) fn open() -> EonResult<Box<dyn Store>> {
    scalar_op(ScalarOp::GetKeyringId, PROCESS_KEYRING, 1)?;
    Ok(Box::<LinuxKeyring>::default())
}

impl Store for LinuxKeyring {
    fn put(&mut self, slot: Slot, secret: EphemeralSecret) -> EonResult<()> {
        match self.0.entry(slot) {
            Entry::Occupied(mut entry) => entry.get_mut().update(&secret)?,
            Entry::Vacant(entry) => {
                entry.insert(KernelKey::create(&secret)?);
            }
        }
        Ok(())
    }

    fn bind(&self, slot: &Slot, stdin: ChildStdin) -> EonResult<()> {
        let secret = self.0.get(slot).ok_or_else(unavailable)?.load()?;
        write_to_stdin(stdin, &secret.0)
    }

    fn remove(&mut self, slot: &Slot) -> EonResult<()> {
        self.0.get(slot).ok_or_else(unavailable)?.revoke()?;
        self.0.remove(slot);
        Ok(())
    }
}

#[cfg(test)]
mod identity_and_credential_tests {
    use super::*;
    use crate::credentials::identity_and_credential_tests::exercise_owner_contract;
    use crate::credentials::{CredentialBroker, IdentityAuthority};

    #[test]
    #[ignore = "requires an accessible Linux process keyring; software store, not hardware evidence"]
    fn linux_process_keyring_round_trip_and_teardown() {
        let authority = IdentityAuthority::new();
        exercise_owner_contract(&authority, CredentialBroker::new(&authority).unwrap());

        let key = KernelKey::create(&EphemeralSecret::new(vec![42])).unwrap();
        let serial = key.serial;
        drop(key);
        // SAFETY: null buffer + zero size asks only for the size. No payload is copied.
        assert!(unsafe { syscall(nr::KEYCTL, READ, serial, 0usize, 0usize, 0 as c_long) } < 0);
    }
}
