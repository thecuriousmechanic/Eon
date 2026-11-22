//! Verified streaming of a blob with iroh-blobs (BLAKE3 Bao) over an iroh connection on loopback. The provider serves from an
//! in-memory store; the client streams the blob and checks the bytes against the hash it asked for.

use iroh::endpoint::presets::Minimal;
use iroh::endpoint::{NetReportConfig, PortmapperConfig};
use iroh::protocol::Router;
use iroh::{Endpoint, RelayMode};
use iroh_blobs::store::mem::MemStore;
use iroh_blobs::{BlobsProtocol, Hash};

const SIZE: usize = 3 * 1024 * 1024 + 123;

async fn loopback_endpoint() -> Endpoint {
    Endpoint::builder(Minimal)
        .clear_ip_transports()
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .portmapper_config(PortmapperConfig::Disabled)
        .net_report_config(NetReportConfig::minimal())
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_multi_chunk_blob_streams_verified_over_loopback() {
    let data: Vec<u8> = (0..SIZE)
        .map(|i| (i.wrapping_mul(131) % 251) as u8)
        .collect();
    let expected_hash = Hash::new(&data);

    let store = MemStore::new();
    let tag = store.blobs().add_bytes(data.clone()).await.unwrap();
    assert_eq!(tag.hash, expected_hash);

    let server_endpoint = loopback_endpoint().await;
    let server_addr = server_endpoint.addr();
    let _router = Router::builder(server_endpoint)
        .accept(iroh_blobs::ALPN, BlobsProtocol::new(&store, None))
        .spawn();

    let client = loopback_endpoint().await;
    let connection = client.connect(server_addr, iroh_blobs::ALPN).await.unwrap();
    let (bytes, _stats) = iroh_blobs::get::request::get_blob(connection, expected_hash)
        .bytes_and_stats()
        .await
        .unwrap();

    assert_eq!(bytes.len(), SIZE);
    assert_eq!(bytes.as_ref(), data.as_slice());
    client.close().await;
}

#[tokio::test]
async fn asking_for_a_blob_the_provider_does_not_hold_fails() {
    let store = MemStore::new();
    let server_endpoint = loopback_endpoint().await;
    let server_addr = server_endpoint.addr();
    let _router = Router::builder(server_endpoint)
        .accept(iroh_blobs::ALPN, BlobsProtocol::new(&store, None))
        .spawn();

    let client = loopback_endpoint().await;
    let connection = client.connect(server_addr, iroh_blobs::ALPN).await.unwrap();
    let missing = Hash::new(b"never stored");
    let result = iroh_blobs::get::request::get_blob(connection, missing)
        .bytes_and_stats()
        .await;
    assert!(
        result.is_err(),
        "a blob the provider does not hold must not download"
    );
    client.close().await;
}
