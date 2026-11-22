//! Current-user DPAPI. Only this broker's in-memory ciphertext survives between calls.

use std::collections::HashMap;
use std::process::ChildStdin;
use std::ptr::{NonNull, null, null_mut};

use eon_primitives::EonResult;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use zeroize::{Zeroize, Zeroizing};

use super::{EphemeralSecret, Slot, Store, failed, unavailable, write_to_stdin};

enum Operation {
    Protect,
    Unprotect,
}

// DPAPI allocates both kinds of output with the local allocator, not Rust's allocator.
struct LocalBlob(NonNull<[u8]>);

impl LocalBlob {
    fn transform(operation: Operation, bytes: &[u8]) -> EonResult<Self> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: bytes
                .len()
                .try_into()
                .map_err(|_| failed("DPAPI input too large"))?,
            pbData: bytes.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: both APIs only read the input blob/bytes, which live through the call.
        // No description, entropy, reserved pointer, or UI; no machine-wide protection.
        // The zero-initialized out parameter is exclusively borrowed for the call.
        let ok = unsafe {
            match operation {
                Operation::Protect => CryptProtectData(
                    &input,
                    null(),
                    null(),
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                ),
                Operation::Unprotect => CryptUnprotectData(
                    &input,
                    null_mut(),
                    null(),
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                ),
            }
        };
        // Only success transfers an allocation; don't dereference an unspecified failure output.
        if ok == 0 {
            return Err(failed("DPAPI operation failed"));
        }
        let data = NonNull::new(output.pbData).ok_or_else(|| failed("empty DPAPI output"))?;
        let blob = Self(NonNull::slice_from_raw_parts(data, output.cbData as usize));
        if blob.0.is_empty() {
            return Err(failed("empty DPAPI output"));
        }
        Ok(blob)
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: a successful call returned cbData initialized bytes. We only protect
        // bounded secrets and only unprotect blobs made here. The borrow cannot outlive self.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for LocalBlob {
    fn drop(&mut self) {
        // SAFETY: self uniquely owns a successful DPAPI output. Wipe before releasing it
        // with the matching allocator, including on length mismatches and pipe errors.
        unsafe {
            self.0.as_mut().zeroize();
            LocalFree(self.0.cast().as_ptr());
        }
    }
}

struct Protected {
    ciphertext: Zeroizing<Vec<u8>>,
    clear_len: usize,
}

#[derive(Default)]
struct DpapiStore(HashMap<Slot, Protected>);

pub(super) fn open() -> EonResult<Box<dyn Store>> {
    // DPAPI has no vault handle to open. A failed operation never falls back to another store.
    Ok(Box::<DpapiStore>::default())
}

impl Store for DpapiStore {
    fn put(&mut self, slot: Slot, secret: EphemeralSecret) -> EonResult<()> {
        let blob = LocalBlob::transform(Operation::Protect, &secret.0)?;
        let protected = Protected {
            ciphertext: Zeroizing::new(blob.bytes().to_vec()),
            clear_len: secret.0.len(),
        };
        // A failed rotation must not evict the previous credential.
        self.0.insert(slot, protected);
        Ok(())
    }

    fn bind(&self, slot: &Slot, stdin: ChildStdin) -> EonResult<()> {
        let protected = self.0.get(slot).ok_or_else(unavailable)?;
        let blob = LocalBlob::transform(Operation::Unprotect, &protected.ciphertext)?;
        if blob.bytes().len() != protected.clear_len {
            return Err(failed("OS credential changed during binding"));
        }
        // No second plaintext allocation; the native buffer is wiped on write errors too.
        write_to_stdin(stdin, blob.bytes())
    }

    fn remove(&mut self, slot: &Slot) -> EonResult<()> {
        self.0.remove(slot).map(drop).ok_or_else(unavailable)
    }
}

#[cfg(test)]
mod identity_and_credential_tests {
    use super::*;
    use crate::credentials::identity_and_credential_tests::{
        check_binding, exercise_owner_contract,
    };
    use crate::credentials::{CredentialBroker, IdentityAuthority};
    use crate::permissions::Permissions as P;

    #[test]
    fn windows_dpapi_owner_grants_and_child_stdin() {
        let authority = IdentityAuthority::new();
        exercise_owner_contract(&authority, CredentialBroker::new(&authority).unwrap());
    }

    #[test]
    fn windows_dpapi_broker_lifetime_and_invalid_ciphertext() {
        let authority = IdentityAuthority::new();
        let owner = authority.user(P::MANAGE_CREDENTIALS | P::USE_CREDENTIALS | P::EXECUTE_PROCESS);
        let handle = "secret://private-alias".parse().unwrap();
        let mut broker = CredentialBroker::new(&authority).unwrap();
        broker
            .put(&owner, &handle, EphemeralSecret::new(vec![42]))
            .unwrap();
        drop(broker);
        let fresh = CredentialBroker::new(&authority).unwrap();
        assert_eq!(
            check_binding(&fresh, &owner, &handle, b""),
            Err(unavailable())
        );

        let mut store = DpapiStore::default();
        store.0.insert(
            Slot(owner.id().clone(), handle.clone()),
            Protected {
                ciphertext: Zeroizing::new(vec![0; 32]),
                clear_len: 1,
            },
        );
        let broker = CredentialBroker {
            authority,
            store: Box::new(store),
        };
        assert_eq!(
            check_binding(&broker, &owner, &handle, b""),
            Err(failed("DPAPI operation failed"))
        );
    }
}
