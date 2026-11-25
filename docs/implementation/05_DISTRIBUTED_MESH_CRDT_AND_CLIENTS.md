# Manual 05: Distributed Mesh (`iroh 1.4`), Loro CRDTs, Android Presence, and Native Clients

> **Crates & Directories:** `eon-mesh`, `eon-uniffi`, `proto/eon.proto`, `clients/android/`, `clients/daemon/`, `clients/cli/`, `clients/canvas/`  
> **Companion Specs:** `DISTRIBUTED_NETWORKING_MESH.md`, `ANDROID_INTEGRATION_SPECIFICATION.md`, `MULTIMODAL_VISION_AND_RENDERING.md`, `TECH_STACK_AND_TOOLCHAINS.md`

---

## 1. Crate and Client Directory Trees

```text
Eon/
├── proto/eon.proto
├── engine/core-rs/crates/
│   ├── eon-mesh/src/
│   │   ├── lib.rs
│   │   ├── endpoint.rs         # iroh 1.4 Endpoint, EndpointId, mDNS + relay config
│   │   ├── auth.rs             # Mutual Ed25519 peer allowlist & pairing QR/PAKE handshake
│   │   ├── qos.rs              # 4-Class QUIC lanes (RFC 9221 datagrams + stream priorities)
│   │   ├── spatial.rs          # Clock-skew-immune 15ms SensoryClaim election + hysteresis
│   │   ├── split_inference.rs  # Edge <-> Workstation streaming token/audio offload + 500ms fallback
│   │   └── crdt/
│   │       ├── doc_schema.rs   # Loro 1.4 containers + FIDES-guarded merge rules
│   │       ├── sync_proto.rs   # /eon/crdt/1 VersionVector delta exchange & shallow compaction
│   │       └── sqlite_proj.rs  # Root subscription -> batched SQLite FTS5 & vec0 materializer
│   │
│   └── eon-uniffi/src/
│       ├── lib.rs              # uniffi 0.29 exported objects, callbacks, memory trim hook
│       └── android_hwbuf.rs    # Zero-copy JNI AHardwareBuffer_fromHardwareBuffer bridge
│
└── clients/
    ├── daemon/src/             # Background OS service (UDS 0600 SO_PEERCRED / NamedPipe DACL)
    ├── cli/src/                # ratatui + crossterm + ratatui-image TUI client
    ├── canvas/src/             # wgpu + egui AST diff, Sugiyama CFG, and video scrubber
    └── android/                # Gradle Kotlin 2.0 + Compose + cargo-ndk app
```

---

## 2. `eon-mesh`: Implementing on Stabilized `iroh 1.4` (`noq 1.0`)

### 2.1 Post-1.0 `iroh` API Corrections
`iroh` reached `1.0` in May 2026 and `1.4.0` in October 2026. Do **not** use pre-1.0 APIs (`MagicEndpoint`, `NodeId`, or built-in `MdnsDiscovery` feature flags):
1. **Identity & Addressing**: `NodeId` is replaced by `iroh::EndpointId` (an Ed25519 public key backed by `iroh::SecretKey`, persisted in `eon-foundation`'s `CredentialBroker` under `secret://mesh/node_ed25519`).
2. **Modular Discovery**: Add `iroh-mdns-address-lookup` so co-located phone and workstation nodes discover LAN IPs directly in `< 5 ms` with zero internet or relay dependency (leveraging `iroh 1.4`'s immediate local address advertisements when offline).
3. **Router & ALPN Multiplexing**:
   ```rust
   use iroh::{Endpoint, SecretKey, protocol::Router};
   use iroh_mdns_address_lookup::MdnsAddressLookup;

   pub const ALPN_EON_RPC: &[u8]  = b"/eon/rpc/1";
   pub const ALPN_EON_CRDT: &[u8] = b"/eon/crdt/1";

   pub async fn build_mesh_router(
       secret_key: SecretKey,
       rpc_handler: EonRpcProtocol,
       crdt_handler: EonCrdtProtocol,
       blobs_protocol: iroh_blobs::net_protocol::Blobs<iroh_blobs::store::fs::Store>,
   ) -> anyhow::Result<Router> {
       let endpoint = Endpoint::builder()
           .secret_key(secret_key)
           .alpns(vec![
               ALPN_EON_RPC.to_vec(),
               ALPN_EON_CRDT.to_vec(),
               iroh_blobs::ALPN.to_vec(),
           ])
           .address_lookup(MdnsAddressLookup::builder())
           .bind()
           .await?;

       let router = Router::builder(endpoint)
           .accept(ALPN_EON_RPC, rpc_handler)
           .accept(ALPN_EON_CRDT, crdt_handler)
           .accept(iroh_blobs::ALPN, blobs_protocol)
           .spawn();
       Ok(router)
   }
   ```
4. **Peer Authorization Gate (`auth.rs`)**: Because `iroh` allows any party knowing an `EndpointId` to dial over relays, wrap every `ProtocolHandler::accept(connection)` check with `let remote_id = connection.remote_id()?; if !peer_acl.is_paired(&remote_id) { connection.close(403u32.into(), b"unpaired"); return Ok(()); }` BEFORE reading any application bytes! Initial pairing uses a local QR / 6-digit SPAKE2 pin ceremony that exchanges Ed25519 `EndpointId`s and device capability manifests.

### 2.2 Exact Wire Framing and 4-Class QUIC QoS (`src/qos.rs`)

| Lane | QUIC Transport Mechanism (`noq`) | Max Frame Size | Priority & Congestion Rule |
|---|---|---|---|
| **Class 0 (Audio / Barge-In / Spatial)** | `connection.send_datagram(Bytes)` (RFC 9221) | `<= 1,150 bytes` (checked via `connection.max_datagram_size()`) | Unreliable, zero retransmit head-of-line delay. `20 ms` of `16 kHz` PCM16 (`640 B`) + Protobuf header (`~24 B`) fits in 1 MTU packet! |
| **Class 1 (Control / Epochs / Tokens)** | Bidirectional stream (`open_bi`), `send.set_priority(100)` | Length-prefixed (`u32 LE` + `prost` payload, cap `1 MiB`) | Highest stream scheduler weight; preempts Class 2 and Class 3 immediately. |
| **Class 2 (CRDT Deltas / Keyframes)** | Bidirectional stream, `send.set_priority(50)` | Cap `4 MiB` per frame | Coalesced in `25 ms` batches during active voice turns. |
| **Class 3 (Model Weights / Build Blobs)** | `iroh-blobs` verified BLAKE3 Bao tree streams, `send.set_priority(0)` | Chunked resumable | Application-level `Semaphore` throttles chunk reads to `25%` link rate whenever Class 0 datagrams or Class 1 streams were active in the past `200 ms`. |

#### Private WAN path-change test (Linux)

`WanEndpoint::open` and `open_with_relay_root` remain **relay-only** and check reciprocal Ed25519 pins. The latter trusts only the supplied DER CA for the HTTPS relay. `paths()` reads counters without sending traffic. The original bare-QAD test uses a test-only builder. The scoped constructor uses a restricted custom transport; the managed N0 constructor below requires an independently confined network.

Run `scripts/test_wan_nat.sh` as the build user on a machine that permits `sudo ip netns`, veth, bridge, conntrack/SNAT and private UDP/TCP. The script creates five namespaces with no uplink or default route. Two peers bootstrap through one privately certified relay behind independent stateful NATs without port forwarding. The relay also provides iroh 1.4's QUIC address discovery (QAD, **not** RFC STUN). After verifying relay transit the fixture opens the private peer route, notifies iroh of the network change, and verifies a 64 KiB transfer on the translated peer sockets. It then drops only the direct routes. It requires the same connection to carry the four typed classes after returning to the relay, with peer path byte deltas and independent forwarding counters. This is not a physical 5G/Wi-Fi handoff.

Class 0 sends numbered 640-byte PCM datagrams every 20 ms, even while the reliable streams stall. `cut` spans route impairment through confirmed relay selection and the start of post-cut counter collection. The deadline is **one-way, application send to peer decode, strictly under 5 ms for every attempt during the cut**. A missing datagram misses the deadline. The roles share Linux `CLOCK_MONOTONIC`; echo RTT is measured and reported separately. Percentiles use received samples only; counts include all attempts. The default exit code gates connectivity, not latency. Add `--require-voice-deadline` to gate both. Raw per-sequence logs, stream receipts and packet counters persist in the printed `target/wan-nat-*` directory on success or failure; ephemeral keys are removed.

The WAN sender budgets 1,150 bytes for unsent datagrams. The receiver budgets 1,200 bytes including queue overhead: one maximum-size legal chunk fits, but not two ordinary 640-byte PCM chunks. A slow reader drops the oldest arrivals rather than accumulating a playback backlog. Smaller frames can share either budget. Neither budget checks frame age or sequence order, or retracts in-flight data.

Run the strict test first with default timing, then add `--short-path-timeout`. Only the direct-capable test endpoints use the shorter profile: 250 ms path keepalive and 1 s default path idle timeout instead of 5 s and 15 s. iroh sets the relay path's idle timeout separately. Neither run signals a network change at the cut. Shorter timers can cost idle traffic and abandon a temporarily slow path; neither public constructor changes its timers.

| Cut phase on the same isolated topology | Attempts | Arrived <5 ms | Arrived late | No receipt | One-way p95 among arrivals | Longest peer decode gap overlapping cut |
|---|---:|---:|---:|---:|---:|---:|
| Before bounding the sender | 754 | 4 | 730 | 20 | 13,864.779 ms | Not recorded |
| Bounded sender only | 754 | 5 | 0 | 749 | 2.801 ms | Not recorded |
| Both queues bounded, default path timing | 756 | 6 | 1 | 749 | 13.908 ms | 15,013.569 ms |
| Both queues bounded, short path timing | 54 | 5 | 0 | 49 | 1.747 ms | 1,002.226 ms |

All four runs migrated without reconnecting and failed the strict deadline. The short profile reduced the interruption, not the 20 ms send cadence. The gap is the longest time between consecutive peer decodes that overlaps the cut. The `cut-start` and `counter-ready` marker timestamps bound the cut, not the next sender ticks. This includes the receipts bracketing a cut with no arrivals. A low latency percentile for the few frames received is not continuous voice delivery.

**Opt-in media duplication.** Both operators can call `enable_media_standby` on their existing pinned sessions, supplying a **distinct** private HTTPS relay IP origin and its DER CA. The same Ed25519 peers authenticate on `eon/mesh/wan-media-copy/1`; direction-specific TLS exporter proofs bind the fresh standby handshake to the primary. This is another QUIC transport, not another logical session. It accepts no application streams. `send_audio` never offloads: only `send_redundant_audio` queues copies on both transports and reports which queues accepted them. Class 1–3 streams and receipts stay on the primary. A slow stream does not hold up PCM.

Both copies are valid PCM protobufs. A nine-byte fixed64 extension (field 15, first on the wire) fits **inside** the existing 1,150-byte limit. An ordinary receiver ignores it, including after asymmetric setup failure. A bound receiver suppresses duplicates with a 128-ID sliding window per direction; falling behind the window drops the frame rather than playing a replay. Ending the primary revokes the lane; reconnect requires fresh consent. Duplication uses additional bandwidth and cannot help if both routes fail.

Add `--redundant-media` to the original gate. For `--lose-direct-interface`, the script checks both original TLS socket cookies across removal of `wan0`. Selected-path byte deltas, private relay counters, primary-only PCM echoes and Class 1–3 receipts still witness primary migration. Strict accounting uses **send timestamps** in `[cut-start, counter-ready)`, including absent and late decodes; skipped send slots fail. A separate standby witness must be sent **after** impairment installation and decoded before either endpoint shows recovery.

The following are historical results. The earlier **fixture-only** STUN client released its local port and rebound it for QUIC. Those runs do not establish live-socket Binding.

| Reviewed private cut run (`--require-voice-deadline` throughout) | Attempts | <5 ms | Late | Missing | Standby first, after installation and before recovery | Strict result |
|---|---:|---:|---:|---:|---:|---|
| Interface loss, short timers, QAD, primary only | 57 | 7 | 0 | 50 | 0 | Fail |
| Interface loss, short timers, QAD, redundant | 56 | 56 | 0 | 0 | 49 | Pass |
| Interface loss, default timers, private STUN, redundant | 1,276 | 1,169 | 107 | 0 | 288 | Fail |
| Silent blackhole, default timers, private STUN, redundant | 758 | 750 | 8 | 0 | 746 | Fail |

All four earlier runs migrated and skipped zero send slots. These are not physical 5G/Wi-Fi handoffs or a general 5 ms guarantee.

**Scoped production opt-in (`WanEndpoint::open_private`).** Provision both peers with reciprocal keys, a literal RFC1918 HTTPS relay and private DER CA, a local IPv4 bind (port `0` is fine), and a literal private Binding origin with a separate random short-term credential per identity. `CandidateScope::new` requires an **operator-owned, dedicated** RFC1918 prefix `/24` through `/32` and a range of at most 256 unprivileged ports. Owning the relay does not establish ownership of an arbitrary subnet. No live peer IP or NAT port is entered by either client. A plain Binding server learns only the client's own mapping; this mode requires an operator service that knows the pair.

**Run the paired publisher.** On a private Linux host, run `python3 -I /absolute/path/to/scripts/wan_private_stun.py serve /absolute/path/to/pair.json` as an unprivileged user. It uses only Python's standard library. Put the configuration in an operator-only directory, set `umask 077` **before** creating it, and use an owner-only regular file (mode `0600` or `0400`, no symlinks or hard links). The JSON shape is:

```json
{"listen":["10.77.0.1",3478],"network":"10.77.0.0/29","ports":[41728,41855],
 "members":[{"user":"a","pin":"<A Ed25519 public key: 64 lowercase hex characters>","password":"<A random 64-character lowercase hex secret>"},
            {"user":"b","pin":"<B Ed25519 public key: 64 lowercase hex characters>","password":"<B independent random 64-character lowercase hex secret>"}]}
```

Replace placeholders offline. Configure each client with **its own** user and password (64 ASCII bytes, not hex-decoded), its peer's pin, and the same scope. Do not copy the full service configuration to either client. Bind only on a private unicast interface and dedicate the source range to this pair, excluding other services; RFC1918 addresses alone do not prove network ownership. Keep the listener off public ingress. No DNS, directory service, registration, arbitrary target field or origin redirect. Stopping the foreground process stops publication, not existing QUIC sessions. To rotate credentials, restart with the new file and reprovision the affected clients. Logs contain only startup status and aggregate counts per slot.

Binding uses a fresh 96-bit transaction ID per attempt, USERNAME, zero PADDING and MESSAGE-INTEGRITY-SHA256. An authenticated challenge returns XOR-MAPPED-ADDRESS and a random 16-byte return cookie (private attribute `0xc0e5`). The client echoes the cookie on the same socket and transaction with a new MAC before the publisher will advertise the **observed source**. Only one 3-second challenge and one confirmed 5-second observation are retained per identity, timed by the server's monotonic clock.

Each confirmed request pays for **one** paired reply to its proven source. The signed cookie echo specifies the remaining receive window (attribute `0xc0e6`: four big-endian bytes, 1–1,000 ms); the publisher conservatively measures it from challenge issuance. Update the publisher first: it accepts the original echo with a 1-second bound, but older publishers do not understand the new attribute. When both observations are live it answers each still-waiting transaction immediately, under that member's own key, with the private `0xc0e4` attribute (length 38: peer pin, IPv4 address, big-endian port; two padding bytes). The two replies consume two separate requests. An expired, replaced or spent request cannot trigger a delayed send. Replaying a proof cannot renew an observation or earn another reply; loss recovery requires a fresh challenge. No reply exceeds its authenticated request size; the per-identity limit remains burst 8 and 4 requests/s. The client independently checks the origin, transaction, MAC, pin and both mappings.

The **same bound UDP socket** then connects to exactly the approved peer mapping. It becomes an iroh 1.4.1 `CustomTransport`; `CustomSender` rejects anything other than that one address even if the new address would be in the pool. There is no unrestricted IP sender, QAD, portmapper or address lookup. Only the operator's relay is supplied to rendezvous; QUIC still authenticates the peer key and the scoped ALPN. Discovery failure or an unreachable direct path permits relay-only startup. Optional Binding gets at most one quarter of the caller's open budget and never more than 1.75 s; the original overall deadline is not extended. A bad authenticated pair assertion fails closed. The custom address represents the translated UDP tuple; it is not a public discovery record.

If direct wins establishment, iroh keeps a standby path on the same connection. For scoped Class 1–3 streams the sender first exchanges a one-byte offer/ready handshake. If iroh selects the relay before the offer completes, the sender can discard that **empty** stream and offer once on the same connection; it never replays application data or extends the 30 s receipt timeout. This costs a round trip and does not guarantee delivery of a payload already in flight. `reconnect` discards direct and media consent. A fresh scoped open is needed for new mappings. In particular, iroh's custom transport does **not** automatically re-punch a new translated address after a relay-first startup. Do not equate this scoped direct → relay result with the QAD relay → direct → relay test above.

Run `scripts/test_wan_nat.sh --private-stun --redundant-media --require-voice-deadline`, then repeat with `--lose-direct-interface`. This profile exercises the production constructor with no timing overrides. For each peer the fixture checks the kernel socket cookie from authenticated Binding through direct QUIC delivery, uses a count-only OUTPUT rule to witness **zero** sends to an authentically published out-of-pool port, and checks direct/fallback byte deltas, all reliable receipts, and zero QAD connections. The interface test also checks the independent established primary and media TLS socket cookies. After scoring, it verifies relay-only reconnect and fresh startup with the publisher unavailable.

With the operator publisher on five private Linux namespaces and two SNATs, **each identity confirmed one observation and received one paired reply in each run**. All three reliable lanes migrated on the original connection. Both clients retained their Binding socket for QUIC and sent zero packets to the separate negative control's disallowed candidate. After publisher shutdown, fresh opens used only the relay.

| Operator publisher, default timers, redundant media | Cut decodes <5 ms / attempts | Max one-way | Missing / skipped slots | Longest in-cut Class 1–3 receipt |
|---|---:|---:|---:|---:|
| Silent blackhole | 750/750 | 1.777 ms | 0 / 0 | 14.977 s |
| Direct interface removed | 290/290 | 1.830 ms | 0 / 0 | 5.771 s |

| Earlier one-shot publisher, default timers, redundant media | Cut attempts | <5 ms | Late | Missing | Skipped slots | Max one-way | Longest in-cut Class 1–3 receipt |
|---|---:|---:|---:|---:|---:|---:|---:|
| Silent blackhole, first scored run | 752 | 717 | 35 | 0 | 0 | 9.504 ms | 14.994 s |
| Direct interface removed, first scored run | 290 | 289 | 1 | 0 | 0 | 6.595 ms | 5.779 s |
| Silent blackhole, final verification | 752 | 752 | 0 | 0 | 0 | 3.056 ms | 15.011 s |
| Direct interface removed, final verification | 293 | 293 | 0 | 0 | 0 | 2.531 ms | 5.782 s |

All four runs migrated on the original connection. **Both final runs passed the original strict gate; the earlier misses show it is not a repeatable latency guarantee.** None of the four lost a cut frame or skipped a send slot. In the final runs the separate media relay delivered 750 and 288 first decodes, respectively, after impairment installation and before primary recovery. The original QAD relay-first regression also migrated in both modes but missed the deadline on 12/755 and 99/1,271 frames, respectively. These measurements cover an isolated Linux topology, not a carrier roam.

#### ID-only WAN rendezvous on a managed network (Linux)

`WanEndpoint::open_managed_n0(secret, peer_pin, provider, bytes_per_sec, wait)` is a separate, per-session opt-in. Both devices supply reciprocal Ed25519 pins; neither enters a peer ticket, IP, NAT port or home relay. `N0Provider::Public` authorizes publishing **this identity and its home relay** to n0's production pkarr service and looking up the peer's signed record. The service and relays can observe source addresses, presence and timing. Closing ends local activity; it does **not** erase records already published or cached. Reconnect consumes the session and refuses to republish: open again to grant fresh consent.

The implementation uses iroh 1.4.1's production relay map, relay-only pkarr publisher, verifying pkarr resolver and native QUIC address discovery (QAD, not RFC STUN). Only the lower ID dials. It uses built-in relay origins from the pinned record, never a discovery IP or arbitrary URL. On a failed or timed-out dial it fetches and verifies a new signed record. The first dial gets at most a third of `wait` or 3 seconds, whichever is shorter; subsequent attempts grow to at most 10 seconds. The original deadline covers registration, lookups, backoff and handshakes. The other peer accepts the pin only via a **currently connected, provider-approved** home relay. Both check the QUIC identity and managed ALPN. No DNS TXT peer hints, DHT, port mapping, captive-portal check or environment-selected staging. `PrivateMirror { pkarr, relay, root_der }` resolves only the pkarr and NA-East relay names to two operator-provisioned RFC1918 addresses; it trusts only the supplied DER CA.

**Required egress boundary.** A peer signature does not prove that a nominated direct address belongs to the peer. An administrator must confine the unprivileged process in a dedicated Linux network namespace **before launch**, with default-deny IPv4/IPv6 egress for its entire lifetime. Allow only the authorized provider, approved DNS if needed, and direct UDP destinations independently known to belong to this pair. Never name the host namespace, authorize an arbitrary advertised candidate, or grant network administration/namespace switching to the process. If the operator cannot enforce this policy, use the existing relay-only API. Identity-only rendezvous is automatic; safely provisioning this boundary on an arbitrary network is **not**.

The grant is the root-owned nsfs mount `/run/netns/eon-n0-<public|mirror>-<digest>`. Hash the following ASCII text with SHA-256 and use lowercase hex; keys are full lowercase hex, IPv4 is dotted decimal, and there is **no trailing newline**.

* Public: `<local-public-key>:<peer-public-key>` (unchanged).
* Mirror: `<local-public-key>:<peer-public-key>:relay=<relay-ip>:pkarr=<pkarr-ip>:ca=<lowercase-sha256-of-CA-DER-bytes>`.

Both devices need new mirror grants when upgrading from the old pair-only name or rotating the CA. The constructor checks directory, mount and all process threads before binding. Swapping the service roles, changing the trust anchor, or requesting public mode in a mirror namespace is denied before lookup or connection. The mount identifies authorization; it does not install firewall rules. To revoke a live grant, stop the process or deny its egress; removing the name denies **future** opens only.

**Reproduce without WAN contact:** run `scripts/test_wan_nat.sh --isolated-n0 --redundant-media --require-voice-deadline`, then repeat with `--lose-direct-interface`. The five-namespace fixture invokes the shipping constructor, including the three grant rejection cases. A private TLS 1.3 store gives the elected dialer a wrong-signer record, a correctly signed stale record naming an unreachable default relay, then the current pinned record. Per-peer egress logs, translated UDP path byte deltas, relay forwarding counters, class receipts and the shared-clock cut score are retained in the printed directory. This emulates one reachable provider relay; it does not test n0's public servers, carrier NAT behavior or a physical radio handoff.

| Earlier managed mirror, v1 ALPN, default timers, redundant media | Cut decodes <5 ms / attempts | Max one-way | Missing / skipped | Longest in-cut Class 1–3 receipt |
|---|---:|---:|---:|---:|
| Silent blackhole | 755/755 | 0.930 ms | 0 / 0 | 15.062 s |
| Direct interface removed | 1,232/1,268 | 13.842 ms | 0 / 0 | 25.330 s |
| Direct interface removed, repeat | 1,266/1,269 | 7.503 ms | 0 / 0 | 25.345 s |

All three runs migrated relay → direct → relay on the original pinned QUIC connection. The primary relay forwarded 109,625, 110,814 and 110,937 bytes respectively during the measured fallback interval. Both interface-loss runs retained the established primary and media relay sockets, but **failed** the strict voice deadline (36 and 3 late frames).

All 39 late decodes in those two runs occurred **after both peers had selected a relay**, while reliable receipts were still outstanding. They included first decodes via both primary and standby. The observations do not identify a single cause of audio jitter. Managed ALPN **v2** now uses the scoped stream offer/ready exchange for Class 1–3: a direct-path offer stranded at fallback can be replaced once on the same connection, **before** any application bytes are submitted. Upgrade both peers; there is no v1 downgrade or change to PCM, path timers, or the 5 ms threshold. To avoid hiding late frames in a shorter cut window, the interface-loss test now also measures 20 seconds of stable fallback.

| Managed v2, default timers, redundant media | Cut decodes <5 ms / attempts | Cut max one-way | Longest in-cut Class 1–3 receipt | Stable fallback <5 ms / attempts (max) |
|---|---:|---:|---:|---:|
| Silent blackhole | 750/750 | 1.436 ms | 14.976 s | 31/31 (0.887 ms) |
| Direct interface removed | 291/291 | 1.911 ms | 5.769 s | 1,000/1,000 (1.299 ms) |
| Direct interface removed, repeat | 290/290 | 1.711 ms | 5.768 s | 1,000/1,000 (1.119 ms) |

All three passed the unchanged cut gate, with zero missing cut decodes or skipped cut send slots. The separate media relay delivered 749, 288 and 287 first decodes before primary recovery respectively. These are private, shared-kernel measurements, not a WAN latency guarantee.

#### Field capture on two provisioned Linux peers

`eon-wan-continuity` measures **one direction on one freshly opened primary session**. Use the receiving device at the transition so the operator can mark it there. Both peers must already have the **private mirror grant and lifetime egress policy above**. The executable opens its primary with `open_managed_n0(..., PrivateMirror { .. }, ...)`; it cannot install a grant, select public n0, accept a peer address, or retry via another constructor. Without the grants, stop here.

Provision a separate local profile directory on each device:

| File | Contents |
|---|---|
| `identity.key` | This device's secret (64 hex digits, owner-only readable, e.g. mode `0600`). |
| `peer.pin` | The other device's full Ed25519 public key in hex. |
| `relay.ip`, `pkarr.ip` | One literal RFC1918 IPv4 address in each file, in the service roles bound by the grant. |
| `ca.der` | The grant's private mirror trust anchor, in binary DER (max 64 KiB). |

Use an absolute profile path through real directories owned by the operator or root; neither directories nor files may be group/world writable. Files must be regular, singly linked and not symlinks. Keys never go on the command line. Use an unprivileged shell **already confined by the supervisor** on each device, with no proxies, `SSLKEYLOGFILE` or `QLOGDIR`. Build from `engine/core-rs` with `cargo build --locked -p eon-mesh --bin eon-wan-continuity` and deploy that binary to both devices.

```sh
# Start within 30 seconds of each other; both operators choose the same duration.
# Receiver: keep stdin on the local terminal. Only the readout goes to stdout.
eon-wan-continuity receive "$HOME/field-profile" 60 > field-receipts.txt
# Sender (on the other device):
eon-wan-continuity send "$HOME/field-profile" 60
```

By default no probe is copied. To compare an independent relay path, provision a second HTTPS relay at port `8444` on the exact `relay.ip` address, with a certificate valid for that IP under the same `ca.der`. Explicitly allow that TCP destination in the supervisor's egress policy. Append `--redundant-media` to **both** commands for this run. Both peers must bind the standby to the same authenticated primary session before capture. An unreachable or mismatched standby fails setup; the tool does not silently run a primary-only trial. The standby sees encrypted traffic sizes and timing, and is never used for reliable frames.

For **clock-independent timing**, append `--timing` to **both** commands and redirect sender stdout to `field-timing.txt`. Both peers must agree on the duration and timing plan on the same fresh session. Primary-only timing uses the v2 challenge handshake. Timed `--redundant-media` uses distinct v3 READY/START tags: both peers must authorize **bidirectional Class 0 copies**, not just forward probes. Upgrade both peers for that mode; a mismatch fails rather than downgrading. Without `--timing`, v1 remains compatible with older peers and produces no timing samples.

Wait for the receiver's first prompt (it has decoded a probe). Press **Enter at the beginning of the actual transition**. Wait for the second prompt, then press **Enter when the transition ends**. Each prompt discards queued input; do not type ahead or hold down Enter. Leave time for receipts after the second mark. Do not mark a transition that did not happen. The tool neither changes interfaces nor detects a radio or path switch. To measure the reverse direction, start a fresh run with exchanged roles.

The sender queues synthetic 640-byte PCM probes, numbered from 1, for 1–120 seconds (max 6,000 frames). Legacy probes remain all silence. The timed sender schedules one opportunity per **nominal** 20 ms slot on a fixed sender-local grid after validating STARTED. It also preserves a **20 ms minimum between actual send starts**. The Linux field tool uses a nonblocking monotonic timerfd to avoid millisecond timer-wheel rounding. It does not restart the nominal period after each enqueue, compress actual sends, or replay elapsed slots. Genuine scheduler delay can accumulate against that hard spacing floor; it is charged to the slot, not erased. Each timed probe carries a fixed prefix, a fresh unpredictable 128-bit challenge and zero padding. Only after the first valid decode does the receiver queue that sequence and challenge back: on the pinned primary in the primary-only plan, or on both bound datagram transports in the bidirectional plan. Only the first valid return counts. Neither side retries or prints a challenge, and sending the next probe never waits for a return.

Setup, finish and shutdown have separate bounded waits. In timed mode the receiver starts its minimum capture interval **when it validates START**, then sends STARTED before the sender begins its interval. The sender waits out the entire interval, including the final slot. Negotiation time cannot satisfy the duration check. The reliable finish declares the actual number queued; `limit` is only a ceiling. An `early_finish` retains partial rows but not the total. After a valid finish the receiver allows 500 ms for overtaken datagrams, then returns a reliable drain receipt. All reliable exchanges remain on the primary. Neither a timeout nor a reconnect can extend a capture.

* `rx_us` for marks and first decodes is relative to the **receiver's monotonic capture start**. `gap_us` is the interval between consecutive unique application decodes in arrival order, not network propagation time. `phase` labels those decodes, **not send attempts**. `max_mark_gap_us` is the longest observed receipt interval overlapping the two marks, including a gap that spans both; a leading/trailing silence without two receipts is not a measured interval.
* `missing` counts declared sequences without a decode by the end of this readout. `skipped = limit − queued` counts nominal send slots never queued (including the unused part of an aborted run), not datagram loss. Without a valid finish the receiver reports `queued`, `missing` and `skipped` as `unknown`; partial receipt rows remain useful. No payloads, addresses, peer identifiers, wall-clock timestamps or key material appear in the readout.
* In redundant mode, `queued` counts a sequence once if at least one local queue accepted it. `via=primary_copy` or `via=standby_copy` identifies the first decoded copy, not the path of both transmissions. The transport suppresses the other copy before application duplicate counting. Silence on a lane is not proof that it delivered anything.
* Sender report v3 records the 1-based nominal `slot`, the delay from that slot to **before the send call** (`send_lag_ns`), and the time from there to the first valid return (`decode_receipt_rtt_ns`). All three come from the same sender monotonic clock. The two durations added together bound slot-to-first-peer-decode time, including scheduling, encoding, both network directions and receipt observation. This is not a one-way estimate. Never halve it.
* No matching return means `unavailable`, not zero. `invalid_returns` counts malformed return datagrams and receipts with the wrong sequence, format or challenge. They do not stop later valid receipts being counted; valid duplicates do not replace the first bound. The sender reports `acknowledged` and `unacknowledged` out of **all queued** probes. After a valid timed drain, `unacknowledged = peer_missing + decoded_without_receipt`; without it both components are `unknown`. The maximum covers acknowledged samples only. Match sender and receiver rows by sequence **within this run**, not by subtracting clocks. A sequence missing at the receiver has no observed decode phase.
* Receiver exit zero (`bracketed=true`) requires a completed exchange, both marks and a sequence after the end higher than all sequences before the beginning. **Not a lossless or latency pass**: there may be no decodes during the marked interval. Timing does not change this exit condition or establish a `<5 ms` one-way verdict.

**Local harness:** `cargo test --offline --locked -p eon-mesh --test wan iroh_and_qos_tests_continuity` exercises the capture routines on pinned loopback peers, including timing consent, marks, missing/duplicate/reordered probes, a tail arriving after finish, malformed PCM/finish totals and a reconnect attempt. `cargo test --offline --locked -p eon-mesh --lib iroh_and_qos_tests_continuity` checks timing arithmetic and receipt validation. `cargo test --offline --locked -p eon-mesh --bin eon-wan-continuity` checks profile handling. These do not provision privileges or exercise public discovery, managed traversal, or a physical handoff.

**Full CLI comparison:** From a checkout whose path meets the profile ownership rules above, run `bash scripts/test_wan_nat.sh --field-continuity`, then repeat with `--redundant-media`. Both unprivileged CLI processes run under the private mirror grants. The fixture waits for data-sized UDP packets to reach the receiver's direct interface and uses a PTY to mark its removal. The end mark means the kernel removed the interface, not that the primary recovered. Read `field-receipts.log` in the printed evidence directory for loss and gaps. The baseline may pass the capture check despite substantial loss. The redundant regression also requires zero missing probes and a first decode via the standby after removal. Neither run measures one-way latency or physical roaming.

**Software-only timing verification (two NATs, no public WAN):** On Linux, as the unprivileged build user with passwordless `sudo` for the fixture, run one or both comparison commands. Each invocation starts a fresh 30-second capture; `--timing` reaches both shipping CLI processes. Omit it for the unchanged untimed capture.

```sh
bash /home/runner/work/Eonba/Eonba/scripts/test_wan_nat.sh --field-continuity --timing
bash /home/runner/work/Eonba/Eonba/scripts/test_wan_nat.sh --field-continuity --timing --redundant-media
```

**Strict deadline command:** append `--require-voice-deadline` to either timed comparison. In field mode this gates **all 1,500 nominal slots across the capture**, not just decodes between the marks. Unlike the separate shared-clock cut gate, it never assigns a sender phase or measures one-way time.

The fixture requires direct peer UDP arrivals before it marks and deletes the receiver's `wan0`, and relay forwarding after removal. In the printed `WAN evidence:` directory, `field-sent.log` and `field-receipts.log` are the raw sender and receiver stdout from **that run**. `field-timing-report.log` retains the sequence join and verdict even when strict mode exits nonzero for incomplete proof. To regenerate just the report, run `python3 -I /home/runner/work/Eonba/Eonba/scripts/wan_field_timing_report.py "$evidence_dir"` with the same optional `--redundant-media` and `--require-voice-deadline` flags. The joiner's focused regressions run separately with `python3 /home/runner/work/Eonba/Eonba/scripts/test_wan_field_timing_report.py`; they need no namespaces.

The join requires sender report v3 and timed receiver report v2. Both headers must match the requested negotiated `mode` and the same `capture_id`: a public hash of a dedicated TLS exporter on the fresh authenticated primary, scoped to the plan and duration. This pairs the two logs without exposing peer IDs or exporter material. It is not a signature on log contents. Legacy unbound logs are insufficient. Bound return copies pass the same per-direction copy-ID replay window as forward media. Primary closure revokes standby reads, not an already-dequeued primary return.

Each queued `seq` (1 through `queued`) names a distinct increasing `slot` (1 through `limit`), with receiver **decode** `phase` and first-copy `via`. If A's echo precedes B's send on the sender clock, the receiver cannot have decoded B before A. The join rejects that contradiction, but permits reordering when intervals overlap or touch; it does not compare sender and receiver timestamps. It reports both `decode_receipt_rtt_upper_us` and `slot_to_receipt_upper_us`. Only the latter is gated, calculated by adding launch delay and RTT in nanoseconds **before rounding up** to microseconds:

| `deadline` | Evidence |
|---|---|
| `proven_under_5ms` | A challenge-matched **slot-to-return** upper bound **< 5000 µs** proves this forward decode met the nominal deadline. |
| `bound_ge_5ms_inconclusive` | The bound is **≥ 5000 µs**, including equality. The return does not prove a timely forward decode. If `send_lag_ns` alone is ≥ 5,000,000, a timely decode was already impossible (`launch_ge_5ms` counts such sends, including unacknowledged ones). |
| `decoded_without_receipt` | The peer decoded the sequence but the sender observed no matching return; the bound is `unavailable`. |
| `peer_missing` | No decode by drain and no return: `phase=no_decode via=none`, not an inferred send phase. |

Tallies for `before`, `during`, `after` and `no_decode` partition the queued probes; `all` gives their totals. A zero `during` count does not prove uninterrupted delivery. No calculation subtracts clocks or halves RTT.

Read the two verdicts separately:

* **`evidence_verification=verified`** requires complete negotiated reports, one sender row per queued sequence, matching counts and maxima, zero invalid returns, and a receiver decode for every numeric bound. The ordered marks must end no later than 10 seconds into capture. An acknowledged pre-mark decode and a fresh acknowledged post-mark decode must bracket removal. The redundant run also requires every queued probe decoded and a post-removal standby-first decode. Invalid reports instead yield `evidence_verification=invalid deadline_verdict=not_evaluated` and a nonzero exit in both modes.
* **`deadline_verdict=proven|not_proven`** quantifies `proven_slots` out of `total_slots=1500`. Only a decoded, acknowledged sequence with a bound **strictly below 5000 µs** counts as proven. `unproven_slots = skipped + unacknowledged + bound_ge_5ms_inconclusive`; `unacknowledged` includes both missing decodes and decodes without a return. `all_queued_under_5ms` is a queued-only diagnostic: it can be `proven` while skipped slots keep the nominal-slot verdict `not_proven`. Neither verdict measures physical roaming.

With verified evidence, the default comparison exits **0 even if the deadline is not proven**. Strict mode preserves the same report but exits **3** for incomplete proof and **0** only for `deadline_verdict=proven`. Exit 3 does not assert a late forward decode.

### 2.3 Schema Reconciliation and Clock-Skew-Immune 15ms Spatial Election (`src/spatial.rs`)

**Protobuf Reconciliation**: Update `SensoryClaim` in `proto/eon.proto` with two optional fields so it matches `DISTRIBUTED_NETWORKING_MESH.md` §8.1 without breaking existing field numbers `1..8`:
```protobuf
message SensoryClaim {
  string claim_id = 1;
  string device_id = 2;
  int64 timestamp_ns = 3;
  float audio_rms_db = 4;
  float snr_score = 5;
  bool is_screen_unlocked = 6;
  bool has_active_hid_input = 7;
  string current_mode = 8;
  uint64 last_hid_activity_ms = 9; // Exact HID recency for continuous decay scoring
  float speaker_confidence = 10;   // WeSpeaker owner cosine similarity [0.0..1.0]
}
```

**Distributed Clock-Skew Trap & Fix**: Never compare `claim.timestamp_ns` across phone and desktop, because their monotonic clocks measure time since each device's respective boot, and NTP wall clocks can skew by `5 to 80 ms`!
- Instead, when Node $A$ detects local VAD onset (or receives a peer's `SensoryClaim` datagram while idle), Node $A$ starts a **local `15.0 ms` `MonotonicClock` timer** and immediately broadcasts its own `SensoryClaim` (even if its local VAD has not crossed threshold yet, reporting its instantaneous `snr_score` and `last_hid_activity_ms`!).
- At `T_local + 15.0 ms`, every node deterministically evaluates the identical normalized score over received claims:
  $$S_i = 0.40 \cdot \text{clamp}\!\left(\frac{\text{SNR}_i - 5.0}{30.0}, 0, 1\right) + 0.30 \cdot \mathbb{I}(\text{HID}_i < 2000\text{ms}) + 0.15 \cdot \mathbb{I}(\text{Unlocked}_i) + 0.15 \cdot \text{SpeakerConf}_i + 0.10 \cdot \mathbb{I}(\text{PrevLeader}_i)$$
- The `+0.10` **stickiness hysteresis term** (active for `15 seconds` after a completed turn unless a different device registers fresh touch/HID input `< 1,000 ms`!) prevents conversational Ping-Pong if the user shifts posture slightly mid-dialogue.

---

## 3. Loro (`1.4`) CRDT Schema, FIDES Merge Guard, and SQLite Projector (`src/crdt/`)

### 3.1 Container Mapping in `LoroDoc`
Use **one `LoroDoc` per logical domain** (`context.loro` and `knowledge.loro`) rather than stuffing millions of historical log lines into a single CRDT state tree:
1. `context.loro`:
   - `LoroMap("nodes")`: Per-peer live presence (`peer_id -> { mode, battery_pct, active_app, leader_lease_until }`).
   - `LoroMap("objectives")`: Active distributed workflow headers and epoch counters (`max_epoch` resolved via monotonic max check on import).
2. `knowledge.loro`:
   - `LoroMap("entities")`: Keyed by `entity_id`, storing `LoroMap` of typed fields.
   - `LoroMap("relations")`: Keyed by `rel_hash = hex(blake3(src || "\0" || pred || "\0" || dst)[..16])`. Each entry stores `{ src, pred, dst, confidence, provenance, integrity_level, updated_lamport }`.

### 3.2 FIDES Integrity Guard Across CRDT Replication
If a compromised or untrusted web-browsing turn on Node B writes a poisoned triple into `knowledge.loro`, standard CRDT Last-Write-Wins would replicate it to Node A! Enforce **Projector-Level Integrity Partitioning**:
- Every CRDT write signs `(container_key, value_digest, integrity_level, peer_id)` or stamps `integrity_level`.
- In `sqlite_proj.rs`, the local SQLite materializer projects `IntegrityLevel::Trusted` triples into `relations_trusted` and `IntegrityLevel::Untrusted` triples into `relations_quarantined`. An update arriving over CRDT sync CANNOT overwrite an existing row in `relations_trusted` if the incoming entry has `integrity_level < Trusted`!
- **Tombstone Compaction**: Every `5,000` local commits during `DORMANT_MODE` when all paired peers have acknowledged a common frontier $F_{\text{acked}}$, compact history via `doc.export(ExportMode::shallow_snapshot(&F_acked))` so CRDT memory stays `< 15 MB` across years of operation.

---

## 4. Android 14/15/16 Presence (`clients/android`) and Zero-Copy FFI (`eon-uniffi`)

### 4.1 Android 14+ (`SDK 35/36`) Foreground Service Type Trap
The baseline `AndroidManifest.xml` declares `android:foregroundServiceType="microphone|connectedDevice|mediaProjection"` on `EonCoreService`. **Critical Android 14+ Crash Trap**: Calling `startForeground(id, notif, FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)` at boot *before* the user has interacted with `MediaProjectionManager.createScreenCaptureIntent()` throws an immediate fatal `SecurityException: Media projections require a foreground service of type ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION` (because the system validates an active projection token inside `ActivityManagerService`!).

**Mandatory Two-Step Service Lifecycle**:
1. On boot / `ROLE_ASSISTANT` wake, call:
   ```kotlin
   ServiceCompat.startForeground(
       this,
       NOTIF_ID,
       notification,
       ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE or ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
   )
   ```
2. Only *after* the user grants screen capture and `onActivityResult` delivers `(resultCode == RESULT_OK, data: Intent)`, upgrade the running foreground service mask in-place by calling `ServiceCompat.startForeground(...)` a second time adding `or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION`, and *then* invoke `mediaProjectionManager.getMediaProjection(resultCode, data)`!

### 4.2 Zero-Copy `AHardwareBuffer` Screen Ingestion (`eon-uniffi/src/android_hwbuf.rs`)
To achieve `< 30ms` OmniParser-v2 + SoM vision grounding on Android without copying 12 MB `RGBA_8888` Bitmaps across the JVM heap:
1. Create `ImageReader.newInstance(width, height, PixelFormat.RGBA_8888, 3, HardwareBuffer.USAGE_CPU_READ_OFTEN or HardwareBuffer.USAGE_GPU_SAMPLED_IMAGE)`.
2. On `OnImageAvailableListener`, extract `val hwBuf: HardwareBuffer = image.hardwareBuffer ?: return`, pass `hwBuf` to a dedicated 15-line JNI entry point `Java_com_eon_assistant_vision_NativeVision_ingestHardwareBuffer(JNIEnv*, jobject, jobject hwBufObj, jlong timestampNs)`.
3. In Rust (`ndk-sys`), call `AHardwareBuffer_fromHardwareBuffer(env, hwBufObj)`, inspect stride via `AHardwareBuffer_describe`, lock planes read-only with `AHardwareBuffer_lock(ahb, AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN, -1, ptr::null(), &mut ptr)`, compute the $32 \times 32$ luma delta in SIMD directly over the strided buffer, and unlock immediately if $\Delta \le 5\%$ (`< 0.3 ms` total CPU cost!).

---

## 5. Desktop Daemon, `ratatui` CLI, and `wgpu` Artifact Canvas (`clients/`)

### 5.1 Secure Local IPC in `clients/daemon`
Never bind an unauthenticated localhost TCP socket or world-writable `/tmp/eon.sock` (any browser SSRF or unprivileged local process could submit `TaskExecutionRequest`s to EON!).
- **Linux / macOS**: Bind Unix Domain Socket at `$XDG_RUNTIME_DIR/eon/daemon.sock` (directory `0700`, socket `0600`), and verify `stream.peer_cred()?.uid() == unsafe { libc::geteuid() }` (`SO_PEERCRED` / `LOCAL_PEERCRED`) on every accepted connection!
- **Windows**: Create Named Pipe `\\.\pipe\LOCAL\eon-daemon-<UserSID>` with `FILE_FLAG_FIRST_PIPE_INSTANCE | PIPE_REJECT_REMOTE_CLIENTS` and an explicit `SECURITY_ATTRIBUTES` SDDL string `"D:P(A;;GA;;;OW)"` (Owner-Only Generic All), rejecting network SMB pipe clients and other local accounts.
- **Framing**: Use `tokio_util::codec::LengthDelimitedCodec` (`max_frame_length = 16 MiB`, little-endian 4-byte length header) wrapping a single top-level `IpcEnvelope { oneof payload { ... } }` message added to `proto/eon.proto`.

### 5.2 Terminal UI (`clients/cli`) and GPU Workbench (`clients/canvas`)
- **`clients/cli`**: Built with `ratatui` + `crossterm` + `ratatui-image` (auto-detecting Kitty graphics protocol -> Sixel -> halfblock Unicode fallback). Split into 4 live panes: (1) Duplex Audio Spectrum + Operating Mode pill, (2) Active DAG Topological Tree with live `TaskState` badges and Epoch counter, (3) Streaming `<think>` dim-italic scratchpad + Ciel `Report:` output, and (4) Slash-command prompt (`/mode`, `/shadow accept|reject`, `/cyber audit`, `/mesh peers`, `/pty attach <id>`).
- **`clients/canvas`**: Built in Rust on `wgpu` + `egui` (`eframe`), rendering at 120 Hz:
  1. **AST Structural Diff Pane**: Side-by-side syntax-highlighted source with matched Tree-sitter symbol blocks connected by Bezier ribbons and 1-click NACK/Apply controls.
  2. **P-Code / Disassembly CFG Explorer**: Layered Sugiyama directed graph layout (`petgraph` rank assignment + barycenter crossing minimization) displaying basic blocks, `CBRANCH` true/false edge colors, Z3 solved concrete inputs overlaid on branch edges, and live DAP instruction-pointer (`RIP`/`PC`) highlight!
