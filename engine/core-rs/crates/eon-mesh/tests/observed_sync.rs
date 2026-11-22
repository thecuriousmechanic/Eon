#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{future::Future, net::Ipv4Addr, time::Duration};

use eon_cli::{WatchState, render_watch_snapshot};
use eon_foundation::{group_commit::GroupCommitJournal, journal::replay};
use eon_mesh::observed::sync_with_observer;
use eon_mesh::sync::{Entity, Replica};
use eon_mesh::telemetry::PairedSyncCompleted;
use eon_mesh::transport::PeerNode;
use eon_mesh::{EonError, EonResult, paired};
use iroh::{EndpointAddr, SecretKey, TransportAddr};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(10);

async fn serve_during<T>(
    node: &PeerNode,
    replica: &Replica,
    sync: impl Future<Output = EonResult<T>>,
) -> T {
    timeout(LIMIT, async {
        tokio::select! {
            result = sync => result.unwrap(),
            () = async { loop { paired::serve_one(node, replica).await.unwrap(); } } => unreachable!(),
        }
    })
    .await
    .expect("pinned loopback sync timed out")
}

#[tokio::test]
async fn opted_in_paired_sync_updates_an_open_mesh_page() {
    let mut scratch = tempfile::Builder::new();
    #[cfg(unix)]
    scratch.permissions(std::fs::Permissions::from_mode(0o700));
    let dir = scratch.prefix("observed-sync-").tempdir().unwrap();
    let journal = GroupCommitJournal::spawn(dir.path(), 1).unwrap();
    let mut watch = WatchState::default();
    for key in "/mesh\n".chars() {
        watch.on_key(key);
    }
    let screen = |watch: &WatchState| render_watch_snapshot(watch, 80, 12).unwrap();
    watch.refresh(dir.path());
    assert!(screen(&watch).contains("No completed paired syncs recorded"));

    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let a_id = a.public();
    let client = PeerNode::start_paired(a, b.public()).await.unwrap();
    let server = PeerNode::start_paired(b, a_id).await.unwrap();
    let port = server
        .addr()
        .ip_addrs()
        .find(|s| s.is_ipv4())
        .unwrap()
        .port();
    let addr = EndpointAddr::from_parts(
        server.addr().id,
        [TransportAddr::Ip((Ipv4Addr::LOCALHOST, port).into())],
    );
    let (local, remote) = (Replica::new(1).unwrap(), Replica::new(2).unwrap());
    serve_during(
        &server,
        &remote,
        paired::sync(&client, addr.clone(), &local),
    )
    .await;
    assert!(
        replay(dir.path()).unwrap().is_empty(),
        "ordinary sync logged"
    );

    let record = |m: PairedSyncCompleted| journal.append_durable(m.encode().to_vec());
    let (failed, served) = timeout(LIMIT, async {
        tokio::join!(
            sync_with_observer(&client, addr.clone(), &local, record),
            server.serve_one(|_| b"not a sync reply".to_vec())
        )
    })
    .await
    .unwrap();
    served.unwrap();
    assert!(failed.is_err());
    assert!(replay(dir.path()).unwrap().is_empty(), "failed sync logged");

    let entity = Entity {
        kind: "file".into(),
        label: "private fixture".into(),
    };
    remote.put_entity("fixture", &entity).unwrap();
    let sync = sync_with_observer(&client, addr.clone(), &local, record);
    let completed = serve_during(&server, &remote, sync).await;
    assert_eq!(completed.journal_seq.unwrap(), 1);
    assert_eq!(local.knowledge_graph().unwrap().entities["fixture"], entity);
    let frames = replay(dir.path()).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].payload, PairedSyncCompleted.encode());
    watch.refresh(dir.path());
    let text = screen(&watch);
    assert!(text.contains("Completed paired syncs recorded: 1"));
    assert!(text.contains("Last sync record at journal seq: 1"));
    assert!(!text.contains("private fixture"));

    let full = EonError::InternalError("local sink unavailable".into());
    let sync = sync_with_observer(&client, addr, &local, |_| Err(full.clone()));
    assert_eq!(
        serve_during(&server, &remote, sync).await.journal_seq,
        Err(full)
    );
    assert_eq!(replay(dir.path()).unwrap(), frames);
    client.close().await;
    server.close().await;
}
