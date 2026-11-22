//! Fixed, synthetic instrumentation fixture. Not compiled into release builds.

use std::process::{Command, Stdio};

use super::*;
use crate::permissions::Permissions as P;

const FIRST: &[u8] = b"first fixture\0\n";
const ROTATED: &[u8] = b"rotated fixture\0\n";
const FIRST_HASH: &[u8] = b"a613cb2eec666c3d29634d5f0b17563ac6cf2209f87a8a9823738a603fda67c5";
const ROTATED_HASH: &[u8] = b"afd10d7540e2ba275dd7a0c125ce310d459256c8048176d730c99ceac32ea7b2";
const EMPTY_HASH: &[u8] = b"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn bind_check(
    broker: &CredentialBroker,
    who: &Identity,
    handle: &CredentialHandle,
    expected: Option<&[u8]>,
) -> Option<()> {
    let mut child = Command::new("/system/bin/toybox")
        .arg("sha256sum")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let result = broker.bind_to_stdin(who, handle, child.stdin.take()?);
    let output = child.wait_with_output().ok()?;
    (result.is_ok() == expected.is_some()
        && output.status.success()
        && output.stdout.split(u8::is_ascii_whitespace).next()
            == Some(expected.unwrap_or(EMPTY_HASH)))
    .then_some(())
}

fn fixture() -> Option<()> {
    let authority = IdentityAuthority::new();
    let mut broker = CredentialBroker::new(&authority).ok()?;
    let bind = P::USE_CREDENTIALS | P::EXECUTE_PROCESS;
    let owner = authority.user(P::SPAWN_SUBAGENT | P::MANAGE_CREDENTIALS | bind);
    let worker = owner.delegate(bind).ok()?;
    let other = authority.user(bind);
    let foreign = IdentityAuthority::new().user(bind);
    let handle: CredentialHandle = "secret://native-fixture".parse().ok()?;
    let secret = |bytes: &[u8]| EphemeralSecret::new(bytes.to_vec());

    broker.put(&owner, &handle, secret(FIRST)).ok()?;
    bind_check(&broker, &worker, &handle, Some(FIRST_HASH))?;
    bind_check(&broker, &other, &handle, None)?;
    bind_check(&broker, &foreign, &handle, None)?;
    broker.put(&worker, &handle, secret(ROTATED)).err()?;
    broker.put(&owner, &handle, secret(ROTATED)).ok()?;
    bind_check(&broker, &worker, &handle, Some(ROTATED_HASH))?;
    let fresh = CredentialBroker::new(&authority).ok()?;
    bind_check(&fresh, &worker, &handle, None)?;
    broker.remove(&owner, &handle).ok()?;
    bind_check(&broker, &worker, &handle, None)?;

    // Transplant actual authenticated records, then go through the public bind path.
    // A denial must write *zero* bytes and close the child's stdin, even with valid grants.
    let mut store = AndroidStore::new().ok()?;
    let slot = || Slot(owner.owner().clone(), handle.clone());
    store.put(slot(), secret(FIRST)).ok()?;
    let mut record = store.entries.remove(&slot())?;
    let different: CredentialHandle = "secret://other-fixture".parse().ok()?;
    store.entries.insert(
        Slot(owner.owner().clone(), different.clone()),
        record.clone(),
    );
    store
        .entries
        .insert(Slot(other.owner().clone(), handle.clone()), record.clone());
    let mut transplanted = AndroidStore::new().ok()?;
    transplanted.entries.insert(slot(), record.clone());
    *record.last_mut()? ^= 1;
    store.entries.insert(slot(), record);
    let short: CredentialHandle = "secret://truncated-fixture".parse().ok()?;
    store.entries.insert(
        Slot(owner.owner().clone(), short.clone()),
        Zeroizing::new(vec![0; OVERHEAD - 1]),
    );
    let test_broker = |store| CredentialBroker {
        authority: authority.clone(),
        store: Box::new(store),
    };
    bind_check(&test_broker(transplanted), &owner, &handle, None)?;
    let broker = test_broker(store);
    bind_check(&broker, &owner, &short, None)?;
    bind_check(&broker, &owner, &different, None)?;
    bind_check(&broker, &other, &handle, None)?;
    bind_check(&broker, &owner, &handle, None)
}

/// Debug instrumentation only. No inputs, persistent entries, byte output or caller-chosen child.
pub fn check_android_vault_fixture() -> bool {
    // The production worker need not be a Java thread. Verify attach + cached class lookup.
    std::thread::Builder::new()
        .spawn(|| fixture().is_some())
        .ok()
        .and_then(|worker| worker.join().ok())
        .unwrap_or(false)
}
