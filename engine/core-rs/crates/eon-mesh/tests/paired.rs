use std::time::Duration;

use eon_mesh::discovery::Directory;
use eon_mesh::exchange::exchange;
use eon_mesh::paired;
use eon_mesh::split_inference::{DecodeSource, SpeechScores, decode_or_local};
use eon_mesh::sync::{ContextEntry, ContextRecord, Entity, Relation, Replica, Speaker};
use eon_mesh::transport::{MAX_FRAME_BYTES, PeerNode};
use eon_mesh::{EonError, EonResult};
use iroh::{EndpointAddr, SecretKey};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(8);

#[tokio::test]
async fn privacy_unpaired_exchange_transfers_public_data_only_in_both_directions() {
    let a = Replica::new(1).unwrap();
    let b = Replica::new(2).unwrap();
    a.set("edge", "public").unwrap();
    b.set("desk", "public").unwrap();
    a.put_context("edge-context", &note("synthetic private text")).unwrap();
    b.put_entity("desk-entity", &entity("synthetic private label")).unwrap();
    let edge = PeerNode::start().await.unwrap();
    let desk = PeerNode::start().await.unwrap();
    timeout(LIMIT, async {
        tokio::try_join!(
            exchange(&edge, desk.addr(), &a),
            desk.serve_one(|bytes| eon_mesh::exchange::answer(&b, &bytes).unwrap())
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(a.get("desk").as_deref(), Some("public"));
    assert_eq!(b.get("edge").as_deref(), Some("public"));
    assert!(a.knowledge_graph().unwrap().entities.is_empty());
    assert!(b.working_context().unwrap().is_empty());
    edge.close().await;
    desk.close().await;
}

async fn nodes(edge_key: &SecretKey, desk_key: &SecretKey) -> (PeerNode, PeerNode) {
    let edge = PeerNode::start_paired(edge_key.clone(), desk_key.public())
        .await
        .unwrap();
    let desk = PeerNode::start_paired(desk_key.clone(), edge_key.public())
        .await
        .unwrap();
    (edge, desk)
}

async fn round(edge: &PeerNode, a: &Replica, addr: EndpointAddr, desk: &PeerNode, b: &Replica) {
    timeout(LIMIT, async {
        tokio::select! {
            result = exchange(edge, addr, a) => result.unwrap(),
            () = async { loop { paired::serve_one(desk, b).await.unwrap(); } } => unreachable!(),
        }
    }).await.unwrap();
}

fn note(text: &str) -> ContextEntry {
    ContextEntry::Utterance {
        speaker: Speaker::User,
        text: text.into(),
    }
}

fn entity(label: &str) -> Entity {
    Entity {
        kind: "document".into(),
        label: label.into(),
    }
}

fn relation(predicate: &str, target: &str) -> Relation {
    Relation {
        source: "work".into(),
        predicate: predicate.into(),
        target: target.into(),
    }
}

#[tokio::test]
async fn crdt_tests_paired_partition_conflicts_and_deletions() {
    let (edge_key, desk_key) = (SecretKey::generate(), SecretKey::generate());
    let (edge, desk) = nodes(&edge_key, &desk_key).await;
    let (a, b) = (Replica::new(1).unwrap(), Replica::new(2).unwrap());
    let mut directory = Directory::default();
    directory.accept(desk.announce(1).unwrap()).unwrap();

    a.set("work", "legacy projection").unwrap();
    a.put_context("work", &note("baseline")).unwrap();
    a.put_context("obsolete", &note("forget me")).unwrap();
    for id in ["work", "draft", "retired"] {
        a.put_entity(id, &entity(id)).unwrap();
    }
    let obsolete = relation("supersedes", "draft");
    a.put_relation(&obsolete).unwrap();
    let old_addr = directory.lookup(&desk_key.public()).unwrap().clone();
    round(&edge, &a, old_addr.clone(), &desk, &b).await;
    assert!(b.knowledge_graph().unwrap().relations.contains(&obsolete));
    desk.close().await;

    let from_edge = ContextEntry::FocusedApp {
        application: "editor".into(),
    };
    let from_desk = ContextEntry::Clipboard {
        text: "selection".into(),
    };
    a.put_context("work", &from_edge).unwrap();
    b.put_context("work", &from_desk).unwrap();
    a.remove_context("obsolete").unwrap();
    b.put_context("desk-only", &note("offline")).unwrap();
    a.remove_entity("retired").unwrap();
    b.put_relation(&relation("mentions", "retired")).unwrap();
    a.remove_relation(&obsolete).unwrap();
    a.remove_entity("draft").unwrap();
    b.put_entity("draft", &entity("renamed offline")).unwrap();
    a.put_entity("new", &entity("new")).unwrap();
    let surviving = relation("cites", "new");
    b.put_relation(&surviving).unwrap();

    assert_ne!(a.working_context().unwrap(), b.working_context().unwrap());
    assert_ne!(a.knowledge_graph().unwrap(), b.knowledge_graph().unwrap());
    let attempt = timeout(Duration::from_millis(250), exchange(&edge, old_addr, &a)).await;
    assert!(!matches!(attempt, Ok(Ok(()))), "a powered-off peer synchronized");

    let desk = PeerNode::start_paired(desk_key.clone(), edge_key.public())
        .await
        .unwrap();
    directory.accept(desk.announce(2).unwrap()).unwrap();
    let addr = directory.lookup(&desk_key.public()).unwrap().clone();
    round(&edge, &a, addr.clone(), &desk, &b).await;
    let ctx = a.working_context().unwrap();
    let graph = a.knowledge_graph().unwrap();
    assert_eq!(ctx, b.working_context().unwrap());
    assert_eq!(graph, b.knowledge_graph().unwrap());
    assert!(ctx["work"] == ContextRecord::from(&from_edge) || ctx["work"] == ContextRecord::Clipboard);
    assert_eq!(ctx["desk-only"], ContextRecord::Utterance { speaker: Speaker::User });
    assert!(!ctx.contains_key("obsolete"));
    assert!(!graph.entities.contains_key("retired"));
    assert!(
        graph
            .entities
            .get("draft")
            .is_none_or(|v| v == &entity("renamed offline"))
    );
    assert_eq!(graph.relations.into_iter().collect::<Vec<_>>(), vec![surviving.clone()]);
    assert_eq!(b.get("work"), None, "paired sync must use only the protected frontier");

    // Reintroduce the removed endpoint ID: dangling facts become visible, but a
    // deleted relation does not.
    b.put_entity("draft", &entity("restored")).unwrap();
    b.put_entity("retired", &entity("restored")).unwrap();
    round(&edge, &a, addr, &desk, &b).await;
    let after = a.knowledge_graph().unwrap();
    assert_eq!(after, b.knowledge_graph().unwrap());
    assert_eq!(after.relations, [surviving, relation("mentions", "retired")].into());
    assert_eq!(ctx, a.working_context().unwrap());
    assert_eq!(ctx, b.working_context().unwrap());
    desk.close().await;
    edge.close().await;
}

#[tokio::test]
async fn crdt_tests_discovery_is_not_pairing_on_either_end() {
    let (edge, desk) = nodes(&SecretKey::generate(), &SecretKey::generate()).await;
    let other = PeerNode::start_paired(SecretKey::generate(), desk.addr().id)
        .await
        .unwrap();
    let mut directory = Directory::default();
    directory.accept(other.announce(1).unwrap()).unwrap();
    let replica = Replica::new(3).unwrap();
    let wrong = directory.lookup(&other.addr().id).unwrap().clone();
    assert!(matches!(
        exchange(&edge, wrong, &replica).await,
        Err(EonError::SecurityViolation(_))
    ));

    let (server, stranger) = timeout(LIMIT, async {
        tokio::join!(
            paired::serve_one(&desk, &replica),
            other.request(desk.addr(), b"private")
        )
    })
    .await
    .unwrap();
    assert!(matches!(server, Err(EonError::SecurityViolation(_))));
    assert!(stranger.is_err());
    other.close().await;
    desk.close().await;
    edge.close().await;
}

#[tokio::test]
async fn crdt_tests_interrupted_push_resumes_without_resending_shared_history() {
    let (edge_key, desk_key) = (SecretKey::generate(), SecretKey::generate());
    let (edge, desk) = nodes(&edge_key, &desk_key).await;
    let (a, b) = (Replica::new(1).unwrap(), Replica::new(2).unwrap());

    // Build the shared history in chunks that fit the frame cap. The incompressible
    // graph label history in aggregate exceeds the cap even after the label is deleted.
    let mut text = vec![0; MAX_FRAME_BYTES * 2];
    blake3::Hasher::new().finalize_xof().fill(&mut text);
    for c in &mut text {
        *c = b' ' + *c % 90;
    }
    for label in text.chunks(MAX_FRAME_BYTES / 3) {
        a.put_entity("large", &entity(std::str::from_utf8(label).unwrap()))
            .unwrap();
        round(&edge, &a, desk.addr(), &desk, &b).await;
    }
    a.remove_entity("large").unwrap();
    round(&edge, &a, desk.addr(), &desk, &b).await;
    a.put_context("from edge", &note("edge")).unwrap();
    b.put_context("from desk", &note("desk")).unwrap();

    let addr = desk.addr();
    let interrupt = async {
        paired::serve_one(&desk, &b).await.unwrap(); // pull succeeds
        desk.close().await; // never acknowledge the push
    };
    let (attempt, interrupted) = tokio::join!(
        timeout(Duration::from_secs(2), exchange(&edge, addr, &a)),
        timeout(LIMIT, interrupt)
    );
    interrupted.unwrap();
    assert!(!matches!(attempt, Ok(Ok(()))));
    assert!(a.working_context().unwrap().contains_key("from desk"));
    assert!(!b.working_context().unwrap().contains_key("from edge"));

    let desk = PeerNode::start_paired(desk_key, edge_key.public()).await.unwrap();
    round(&edge, &a, desk.addr(), &desk, &b).await;
    assert_eq!(a.working_context().unwrap(), b.working_context().unwrap());
    assert_eq!(a.working_context().unwrap().len(), 2);
    desk.close().await;
    edge.close().await;
}

#[tokio::test]
async fn split_inference_tests_verified_peer_then_verified_local_during_partition() -> EonResult<()> {
    let (edge, desk) = nodes(&SecretKey::generate(), &SecretKey::generate()).await;
    let replica = Replica::new(1)?;
    let addr = desk.addr();
    // Argmax = [1, 1, blank, 1, 2]; equal maxima resolve to the lowest label.
    let scores = SpeechScores {
        vocabulary: 3,
        scores: &[0, 8, 2, 0, 7, 7, 9, 0, 1, 0, 6, 0, 0, 1, 8],
    };
    let (remote, ()) = timeout(LIMIT, async {
        tokio::try_join!(
            decode_or_local(&edge, addr.clone(), scores),
            paired::serve_one(&desk, &replica)
        )
    })
    .await
    .unwrap()?;
    assert_eq!(remote.source, DecodeSource::VerifiedPeer);
    assert_eq!(remote.tokens, [1, 1, 2]);

    let (bad, ()) = timeout(LIMIT, async {
        // A well-formed Tokens response with the wrong first token (same length).
        tokio::try_join!(
            decode_or_local(&edge, addr.clone(), scores),
            desk.serve_one(|_| vec![2, 3, 2, 1, 2])
        )
    })
    .await
    .unwrap()?;
    assert_eq!(bad.source, DecodeSource::LocalOnInvalidReply);
    assert_eq!(bad.tokens, remote.tokens);

    desk.close().await;
    let local = timeout(Duration::from_secs(2), decode_or_local(&edge, addr, scores))
        .await
        .unwrap()?;
    assert!(matches!(
        local.source,
        DecodeSource::LocalOnTransportError | DecodeSource::LocalOnTimeout
    ));
    assert_eq!(local.tokens, remote.tokens);
    edge.close().await;
    Ok(())
}
