//! AndroidKeyStore protects the key; only this Rust broker retains encrypted entries.

use std::collections::HashMap;
use std::sync::OnceLock;

use jni::JavaVM;
use jni::objects::{GlobalRef, JValue};
use zeroize::Zeroizing;

use super::*;

// 96-bit provider-generated nonce and 128-bit GCM tag, neither supplied by a caller.
const OVERHEAD: usize = 12 + 16;
const CIPHER: &str = "com/eon/assistant/FoundationVaultCipher";
const SIGNATURE: &str = "([BLjava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)V";

struct Host {
    vm: JavaVM,
    cipher: GlobalRef,
}

static HOST: OnceLock<Host> = OnceLock::new();

#[cfg(debug_assertions)]
#[path = "android_acceptance.rs"]
mod acceptance;
#[cfg(debug_assertions)]
pub use acceptance::check_android_vault_fixture;

fn os_error() -> EonError {
    failed("Android credential store unavailable")
}

/// Trusted packaged library bootstrap, called from JNI_OnLoad on ART's loading thread.
/// Resolve the fixed app class here, not with FindClass on an attached native worker.
/// No host object, class name, callback, identity or sink is supplied by a client.
///
/// # Safety
/// The VM must outlive the process's brokers. Call only in the trusted app loader's
/// JNI_OnLoad context: the pinned class must be the packaged synchronous implementation,
/// which does not retain or share the borrowed native buffers.
pub unsafe fn register_android_vm(vm: JavaVM) -> EonResult<()> {
    let mut env = vm.get_env().map_err(|_| os_error())?;
    let class = env
        .find_class(CIPHER)
        .and_then(|class| env.new_global_ref(class));
    // NewGlobalRef is allowed to return NULL (e.g. low memory), including without throwing.
    let cipher = match class {
        Ok(class) if !class.as_obj().is_null() && !env.exception_check().unwrap_or(true) => class,
        _ => {
            let _ = env.exception_clear();
            return Err(os_error());
        }
    };
    HOST.set(Host { vm, cipher }).map_err(|_| os_error())
}

struct AndroidStore {
    host: &'static Host,
    namespace: IdentityId,
    entries: HashMap<Slot, Zeroizing<Vec<u8>>>,
}

pub(super) fn open() -> EonResult<Box<dyn Store>> {
    Ok(Box::new(AndroidStore::new()?))
}

enum Operation {
    Seal,
    Unseal,
}

impl AndroidStore {
    fn new() -> EonResult<Self> {
        Ok(Self {
            host: HOST.get().ok_or_else(os_error)?,
            namespace: IdentityId::new(),
            entries: HashMap::new(),
        })
    }

    fn transform(
        &self,
        op: Operation,
        slot: &Slot,
        input: &[u8],
        output: &mut [u8],
    ) -> EonResult<()> {
        let mut env = self
            .host
            .vm
            .attach_current_thread()
            .map_err(|_| os_error())?;
        if env.exception_check().unwrap_or(true) {
            return Err(os_error()); // Don't consume a pre-existing host exception.
        }

        let result: jni::errors::Result<()> = env.with_local_frame(12, |env| {
            // None of these canonical labels can contain NUL. Include the broker as well as
            // the owner and alias: even two brokers using the same issuer have disjoint slots.
            let aad = env.byte_array_from_slice(
                format!(
                    "eon:foundation:vault:v1\0{}\0{}\0{}",
                    self.namespace,
                    slot.0,
                    slot.1.as_str()
                )
                .as_bytes(),
            )?;
            // SAFETY: slices are non-empty, live and disjoint through this local frame.
            // Only a read-only view of `input` leaves here. The pinned, fixed Java helper
            // synchronously copies bytes; it never retains/views them after return, calls
            // client code, or passes these native buffers on to a crypto provider.
            let source =
                unsafe { env.new_direct_byte_buffer(input.as_ptr().cast_mut(), input.len())? };
            let source = env
                .call_method(source, "asReadOnlyBuffer", "()Ljava/nio/ByteBuffer;", &[])?
                .l()?;
            let dest = unsafe { env.new_direct_byte_buffer(output.as_mut_ptr(), output.len())? };
            let method = match op {
                Operation::Seal => "seal",
                Operation::Unseal => "unseal",
            };
            env.call_static_method(
                &self.host.cipher,
                method,
                SIGNATURE,
                &[
                    JValue::Object(&aad),
                    JValue::Object(&source),
                    JValue::Object(&dest),
                ],
            )?
            .v()
        });
        result.map_err(|_| {
            // Do not describe/log/propagate a provider exception or poison an attached thread.
            let _ = env.exception_clear();
            os_error()
        })
    }
}

impl Store for AndroidStore {
    fn put(&mut self, slot: Slot, secret: EphemeralSecret) -> EonResult<()> {
        let mut record = Zeroizing::new(vec![0; secret.0.len() + OVERHEAD]);
        self.transform(Operation::Seal, &slot, &secret.0, &mut record)?;
        self.entries.insert(slot, record); // A failed rotation leaves the old entry intact.
        Ok(())
    }

    fn bind(&self, slot: &Slot, stdin: ChildStdin) -> EonResult<()> {
        let record = self.entries.get(slot).ok_or_else(unavailable)?;
        let len = record
            .len()
            .checked_sub(OVERHEAD)
            .filter(|n| (1..=MAX_SECRET_LEN).contains(n))
            .ok_or_else(os_error)?;
        let mut clear = Zeroizing::new(vec![0; len]);
        self.transform(Operation::Unseal, slot, record, &mut clear)?;
        write_to_stdin(stdin, &clear)
    }

    fn remove(&mut self, slot: &Slot) -> EonResult<()> {
        self.entries.remove(slot).map(drop).ok_or_else(unavailable)
    }
}
