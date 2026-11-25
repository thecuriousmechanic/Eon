use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::identity::IdentityKind;
use crate::permissions::Permissions as P;

#[derive(Default)]
struct FixtureStore {
    slots: HashMap<Slot, EphemeralSecret>,
    loads: Arc<AtomicUsize>,
}

impl Store for FixtureStore {
    fn put(&mut self, slot: Slot, secret: EphemeralSecret) -> EonResult<()> {
        self.slots.insert(slot, secret);
        Ok(())
    }

    fn bind(&self, slot: &Slot, stdin: ChildStdin) -> EonResult<()> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        let secret = self.slots.get(slot).ok_or_else(unavailable)?;
        write_to_stdin(stdin, &secret.0)
    }

    fn remove(&mut self, slot: &Slot) -> EonResult<()> {
        self.slots.remove(slot).map(drop).ok_or_else(unavailable)
    }
}

fn fixture_broker(authority: &IdentityAuthority) -> (CredentialBroker, Arc<AtomicUsize>) {
    let store = FixtureStore::default();
    let loads = store.loads.clone();
    (
        CredentialBroker {
            authority: authority.clone(),
            store: Box::new(store),
        },
        loads,
    )
}

#[test]
fn identity_authority_and_delegation_do_not_escalate() {
    let authority = IdentityAuthority::new();
    let system = authority.system(P::default());
    assert_eq!(system.kind(), &IdentityKind::System);
    assert_eq!(system.permissions(), P::NONE);
    assert!(authority.authorize(&system, P::MANAGE_CREDENTIALS).is_err());
    assert!(system.delegate(P::NONE).is_err());

    let user = authority.user(P::SPAWN_SUBAGENT | P::READ_WORKSPACE | P::MUTATE_WORKSPACE);
    let agent = user
        .delegate(P::SPAWN_SUBAGENT | P::READ_WORKSPACE)
        .unwrap();
    let leaf = agent.delegate(P::READ_WORKSPACE).unwrap();
    assert_eq!(user.kind(), &IdentityKind::User);
    assert_eq!(leaf.owner(), user.id());
    assert_ne!(agent.id(), leaf.id());
    assert_eq!(
        leaf.kind(),
        &IdentityKind::EphemeralAgent {
            parent: agent.id().clone()
        }
    );
    assert!(leaf.delegate(P::READ_WORKSPACE).is_err());
    assert!(agent.delegate(P::MUTATE_WORKSPACE).is_err());
    for denied in [
        P::EXECUTE_PROCESS,
        P::CYBER_OPERATIONS,
        P::NETWORK_ACCESS,
        P::DEVICE_CONTROL,
    ] {
        assert!(user.delegate(denied).is_err());
    }
    assert!(IdentityAuthority::new().authorize(&leaf, P::NONE).is_err());
    authority.authorize(&leaf, P::READ_WORKSPACE).unwrap();
}

#[test]
fn opaque_handle_and_argv_checks_never_echo_rejected_input() {
    let handle = "secret://github/deploy_key-1"
        .parse::<CredentialHandle>()
        .unwrap();
    assert_eq!(handle.as_str(), "secret://github/deploy_key-1");
    for uri in [
        "github/token",
        "secret://",
        "secret:///x",
        "secret://x//y",
        "secret://x/..",
        "secret://x/%2e",
        "secret://x?a=b",
        "secret://x\0bad",
        "secret://x\nbad",
    ] {
        let err = uri.parse::<CredentialHandle>().err().unwrap();
        assert_eq!(
            err,
            EonError::ValidationError("invalid credential handle".into())
        );
    }
    assert!(
        format!("secret://{}", "a".repeat(129))
            .parse::<CredentialHandle>()
            .is_err()
    );
    CredentialBroker::check_argv(&["git", "status"]).unwrap();
    let err = CredentialBroker::check_argv(&["tool", "--auth=secret://private-alias"]).unwrap_err();
    assert!(!format!("{err:?} {err}").contains("private-alias"));
}

#[cfg(any(unix, windows))]
mod stdin_probe {
    use super::*;
    use std::io::Read;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    const EXPECTED_DIGEST: &str = "EON_TEST_STDIN_FIXTURE_DIGEST";
    // libtest exits zero if --exact matches nothing. Require proof the reader saw EOF.
    pub(super) const MATCH: i32 = 42;

    #[test]
    #[ignore = "stdin reader subprocess for the credential tests"]
    fn read_fixture() {
        let Ok(expected) = std::env::var(EXPECTED_DIGEST) else {
            return;
        };
        let matched = {
            let mut actual = zeroize::Zeroizing::new(Vec::new());
            std::io::stdin()
                .lock()
                .take(MAX_SECRET_LEN as u64 + 1)
                .read_to_end(&mut actual)
                .unwrap();
            actual.len() <= MAX_SECRET_LEN && blake3::hash(&actual).to_hex().as_str() == expected
        };
        std::process::exit(if matched { MATCH } else { 1 });
    }

    pub(super) struct Probe(Child);

    impl Probe {
        pub(super) fn spawn(expected: &[u8]) -> (Self, ChildStdin) {
            // A Windows ChildStdin needs an asynchronous handle; io::pipe() is synchronous.
            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "credentials::identity_and_credential_tests::stdin_probe::read_fixture",
                ])
                .env(EXPECTED_DIGEST, blake3::hash(expected).to_hex().as_str())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut probe = Self(child);
            let stdin = probe.0.stdin.take().unwrap();
            (probe, stdin)
        }

        pub(super) fn wait(mut self, timeout: Duration) -> Option<ExitStatus> {
            let start = Instant::now();
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    return Some(status);
                }
                if start.elapsed() >= timeout {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    pub(super) fn closed_stdin() -> ChildStdin {
        let (probe, mut stdin) = Probe::spawn(b"");
        // Keep stdin open so the reader cannot see EOF. The deadline must kill and reap it.
        assert!(probe.wait(Duration::ZERO).is_none());
        // A concurrent fork can still hold the read end until exec. Confirm closure before
        // testing the broker, probing with at most 100 bytes (well below pipe capacity).
        for _ in 0..100 {
            if stdin.write_all(&[0]).is_err() {
                return stdin;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("credential stdin pipe did not close");
    }
}

#[cfg(any(unix, windows))]
pub(super) fn check_binding(
    broker: &CredentialBroker,
    caller: &Identity,
    handle: &CredentialHandle,
    expected: &[u8],
) -> EonResult<()> {
    let (probe, stdin) = stdin_probe::Probe::spawn(expected);
    std::thread::scope(|scope| {
        // Start the deadline before binding: the writer can block if the child stops reading.
        scope.spawn(move || {
            let status = probe
                .wait(std::time::Duration::from_secs(10))
                .expect("credential stdin reader timed out");
            assert_eq!(status.code(), Some(stdin_probe::MATCH), "stdin mismatch");
        });
        broker.bind_to_stdin(caller, handle, stdin)
    })
}

#[cfg(any(unix, windows))]
pub(super) fn exercise_owner_contract(authority: &IdentityAuthority, mut broker: CredentialBroker) {
    let bind = P::USE_CREDENTIALS | P::EXECUTE_PROCESS;
    let grants = P::MANAGE_CREDENTIALS | P::SPAWN_SUBAGENT | bind;
    let owner = authority.user(grants);
    let other = authority.system(grants); // No global system override.
    let foreign = IdentityAuthority::new().system(grants);
    let worker = owner.delegate(bind).unwrap();
    let handle = "secret://deploy".parse().unwrap();
    let fixture = || EphemeralSecret::new(b"paper\0one\n".to_vec());
    broker.put(&owner, &handle, fixture()).unwrap();

    check_binding(&broker, &worker, &handle, b"paper\0one\n").unwrap();
    assert!(check_binding(&broker, &other, &handle, b"").is_err());
    assert!(broker.remove(&other, &handle).is_err());
    assert!(broker.put(&foreign, &handle, fixture()).is_err());
    assert!(check_binding(&broker, &foreign, &handle, b"").is_err());
    assert!(broker.put(&worker, &handle, fixture()).is_err());
    assert!(broker.remove(&worker, &handle).is_err());

    broker
        .put(&other, &handle, EphemeralSecret::new(b"paper-two".to_vec()))
        .unwrap();
    broker
        .put(&owner, &handle, EphemeralSecret::new(b"rotated".to_vec()))
        .unwrap();
    check_binding(&broker, &worker, &handle, b"rotated").unwrap();
    check_binding(&broker, &other, &handle, b"paper-two").unwrap();
    broker.remove(&owner, &handle).unwrap();
    assert!(check_binding(&broker, &worker, &handle, b"").is_err());
    check_binding(&broker, &other, &handle, b"paper-two").unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn owner_scoped_put_rotate_bind_remove() {
    let authority = IdentityAuthority::new();
    exercise_owner_contract(&authority, fixture_broker(&authority).0);
}

#[cfg(any(unix, windows))]
#[test]
fn no_resolution_before_authorized_use() {
    let authority = IdentityAuthority::new();
    let (mut broker, loads) = fixture_broker(&authority);
    let owner = authority
        .user(P::MANAGE_CREDENTIALS | P::USE_CREDENTIALS | P::EXECUTE_PROCESS | P::SPAWN_SUBAGENT);
    let handle = "secret://alias".parse().unwrap();
    broker
        .put(
            &owner,
            &handle,
            EphemeralSecret::new(vec![7; MAX_SECRET_LEN]),
        )
        .unwrap();
    for size in [0, MAX_SECRET_LEN + 1] {
        assert!(
            broker
                .put(&owner, &handle, EphemeralSecret::new(vec![0; size]))
                .is_err()
        );
    }
    for grants in [
        P::NONE,
        P::USE_CREDENTIALS,
        P::EXECUTE_PROCESS,
        P::MANAGE_CREDENTIALS,
    ] {
        let denied = owner.delegate(grants).unwrap();
        assert!(check_binding(&broker, &denied, &handle, b"").is_err());
    }
    assert_eq!(loads.load(Ordering::Relaxed), 0);
    check_binding(&broker, &owner, &handle, &[7; MAX_SECRET_LEN]).unwrap();
    assert_eq!(loads.load(Ordering::Relaxed), 1);
}

#[cfg(any(unix, windows))]
#[test]
fn closed_pipe_error_contains_no_alias_or_material() {
    let authority = IdentityAuthority::new();
    let (mut broker, _) = fixture_broker(&authority);
    let owner = authority.user(P::MANAGE_CREDENTIALS | P::USE_CREDENTIALS | P::EXECUTE_PROCESS);
    let handle = "secret://private-alias".parse().unwrap();
    let fixture = b"paper fixture for binding failure";
    broker
        .put(&owner, &handle, EphemeralSecret::new(fixture.to_vec()))
        .unwrap();
    let err = broker
        .bind_to_stdin(&owner, &handle, stdin_probe::closed_stdin())
        .unwrap_err();
    assert!(
        err == failed("cannot bind to child stdin"),
        "unexpected bind error"
    );
    check_binding(&broker, &owner, &handle, fixture).unwrap();
}

#[test]
fn journal_contains_only_alias_and_entropy_redaction() {
    use crate::{group_commit::GroupCommitJournal, journal::replay, scrubber::redaction_marker};
    let dir = tempfile::tempdir().unwrap();
    let authority = IdentityAuthority::new();
    let (mut broker, loads) = fixture_broker(&authority);
    let owner = authority.user(P::MANAGE_CREDENTIALS);
    let handle = "secret://deploy".parse::<CredentialHandle>().unwrap();
    let material = b"paper fixture, deliberately low entropy";
    let noise = "aB0cD1eF2gH3iJ4kL5mN6oP7qR8sT9uVwXyZ";
    broker
        .put(&owner, &handle, EphemeralSecret::new(material.to_vec()))
        .unwrap();

    let journal = GroupCommitJournal::spawn(dir.path(), 1).unwrap();
    let seq = journal
        .append_scrubbed(&format!("dispatch {} output {noise}", handle.as_str()))
        .unwrap();
    drop(journal);
    let expected = format!(
        "dispatch secret://deploy output {}",
        redaction_marker(noise)
    );
    let frames = replay(dir.path()).unwrap();
    assert_eq!(frames.len(), 1);
    assert!(frames[0].payload == expected.as_bytes());
    let disk = std::fs::read(dir.path().join(format!("seg_{seq:020}.eonj"))).unwrap();
    for raw in [material.as_slice(), noise.as_bytes()] {
        assert!(!disk.windows(raw.len()).any(|w| w == raw));
    }
    assert_eq!(loads.load(Ordering::Relaxed), 0);
}
