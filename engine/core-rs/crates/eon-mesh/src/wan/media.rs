//! A second transport, not a second application session. Only the binder
//! stream is ever accepted here; no reliable-frame dispatcher or reconnect.
use super::*;

#[cfg(test)]
mod tests;

const MEDIA_ALPN: &[u8] = b"eon/mesh/wan-media-copy/1";
// An unknown fixed64 protobuf field (15), not an envelope. A receiver
// without a bound lane still decodes precisely the original AudioPcmChunk.
const COPY_ID_KEY: u8 = (15 << 3) | 1;
const PREFIX: usize = 1 + 8;

pub(super) fn tagged(packet: &[u8]) -> bool {
    packet.first() == Some(&COPY_ID_KEY)
}

fn decode_copy(packet: Bytes, window: &Mutex<Window>) -> EonResult<Option<AudioPcmChunk>> {
    if !tagged(&packet) || !(PREFIX..=MAX_AUDIO).contains(&packet.len()) {
        return Err(invalid("invalid redundant datagram"));
    }
    let id = u64::from_le_bytes(packet[1..PREFIX].try_into().unwrap());
    let payload = packet.slice(PREFIX..);
    if tagged(&payload) {
        return Err(invalid("nested media copy tag"));
    }
    let pcm = wire::decode_audio(payload)?;
    Ok(window.lock().unwrap().admit(id).then_some(pcm))
}

/// Which local DATAGRAM queues accepted an explicitly duplicated frame.
/// Neither flag is a delivery receipt.
#[derive(Debug, PartialEq, Eq)]
pub struct AudioCopies {
    pub primary: bool,
    pub standby: bool,
}

pub(super) struct MediaLane {
    endpoint: Endpoint,
    conn: Connection,
    next: Mutex<u64>,
    received: Mutex<Window>,
}

#[derive(Default)]
struct Window {
    high: u64,
    bits: u128,
}

impl Window {
    fn admit(&mut self, id: u64) -> bool {
        if id > self.high {
            let jump = id - self.high;
            self.bits = if jump < 128 { self.bits << jump } else { 0 };
            self.high = id;
        }
        let age = self.high - id;
        if age >= 128 || self.bits & (1 << age) != 0 {
            return false;
        }
        self.bits |= 1 << age;
        true
    }
}

fn check_open(primary: &Connection) -> EonResult<()> {
    match primary.close_reason() {
        Some(e) => Err(transport_error(e)),
        None => Ok(()),
    }
}

fn after_dequeue(primary: &Connection, packet: Bytes, standby: bool) -> EonResult<(Bytes, bool)> {
    // Primary closure revokes the standby, not data already read from the primary.
    if standby {
        check_open(primary)?;
    }
    Ok((packet, standby))
}

// The fully handshaken media TLS connection provides a fresh shared nonce.
// Export a different proof from the *primary* session for each prover. Old
// proofs can't authorize another transport, another primary, or reflect back.
fn proof(primary: &Connection, media: &Connection, prover: EndpointId) -> EonResult<[u8; 32]> {
    let mut context = [0; 64];
    media
        .export_keying_material(&mut context[..32], b"EXPORTER-eon-media-nonce-v1", b"")
        .map_err(|_| invalid("cannot export media nonce"))?;
    context[32..].copy_from_slice(prover.as_bytes());
    let mut out = [0; 32];
    primary
        .export_keying_material(&mut out, b"EXPORTER-eon-media-binder-v1", &context)
        .map_err(|_| invalid("cannot bind media to primary"))?;
    Ok(out)
}

async fn authenticate(primary: &Connection, media: &Connection, me: EndpointId) -> EonResult<()> {
    let peer = primary.remote_id();
    if media.remote_id() != peer || media.alpn() != MEDIA_ALPN {
        return Err(invalid("media peer or protocol mismatch"));
    }
    let send = async |mut stream: iroh::endpoint::SendStream| -> EonResult<()> {
        stream
            .write_all(&proof(primary, media, me)?)
            .await
            .map_err(transport_error)?;
        stream.finish().map_err(transport_error)
    };
    let verify = async |mut stream: iroh::endpoint::RecvStream| -> EonResult<()> {
        let bytes = stream.read_to_end(32).await.map_err(transport_error)?;
        let expected = blake3::Hash::from(proof(primary, media, peer)?);
        if !expected.eq(bytes.as_slice()) {
            return Err(EonError::SecurityViolation(
                "wrong primary session binder".into(),
            ));
        }
        Ok(())
    };
    if me < peer {
        let (tx, rx) = media.open_bi().await.map_err(transport_error)?;
        send(tx).await?;
        verify(rx).await?;
    } else {
        let (tx, rx) = media.accept_bi().await.map_err(transport_error)?;
        verify(rx).await?;
        send(tx).await?;
    }
    check_open(primary)
}

impl WanEndpoint {
    /// Opt in on **both** already-connected peers before starting redundant
    /// traffic. The standby must be a separate operator-provisioned HTTPS
    /// relay (private or loopback IP literal, custom DER CA, no DNS or QAD).
    /// There is no caller-supplied peer ID, direct socket or background retry.
    /// Failure leaves the primary usable. Reconnect discards this consent.
    pub async fn enable_media_standby(
        &mut self,
        relay: RelayUrl,
        root_der: Vec<u8>,
        wait: Duration,
    ) -> EonResult<()> {
        validate_relay(&relay)?;
        let private = relay
            .host_str()
            .and_then(|h| h.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(|ip| match ip {
                IpAddr::V4(v) => v.is_loopback() || v.is_private(),
                IpAddr::V6(v) => v.is_loopback() || v.is_unique_local(),
            });
        if relay == self.relay || !private || relay.scheme() != "https" || root_der.is_empty() {
            return Err(invalid(
                "expected a distinct private HTTPS standby and relay CA",
            ));
        }
        let builder = datagram_builder(QuicTransportConfig::builder(), 1)
            .ca_tls_config(iroh::tls::CaTlsConfig::custom_roots([root_der.into()]));
        self.attach_media(relay, builder, wait).await
    }

    async fn attach_media(
        &mut self,
        relay: RelayUrl,
        builder: Builder,
        wait: Duration,
    ) -> EonResult<()> {
        if self.media.is_some() {
            return Err(invalid("standby already attached"));
        }
        check_open(&self.conn)?;
        let setup = async {
            let ep = bind(
                builder,
                self.endpoint.secret_key().clone(),
                RelayConfig::new(relay.clone(), None),
                MEDIA_ALPN,
            )
            .await?;
            let conn = rendezvous(&ep, self.peer_id(), &relay, MEDIA_ALPN).await?;
            if let Err(err) = authenticate(&self.conn, &conn, ep.id()).await {
                conn.close(1u8.into(), b"media binding failed");
                return Err(err);
            }
            Ok(MediaLane {
                endpoint: ep,
                conn,
                next: Mutex::default(),
                received: Mutex::default(),
            })
        };
        let lane = tokio::select! {
            biased;
            error = self.conn.closed() => return Err(transport_error(error)),
            result = timeout(wait, setup) => result.map_err(|_| transport_error("media rendezvous timed out"))??,
        };
        check_open(&self.conn)?;
        self.media = Some(lane);
        Ok(())
    }

    /// Explicitly duplicate this Class 0 frame. Always attempt the primary;
    /// neither datagram waits for the other, a stream write, or a receipt.
    /// Plain `send_audio` and all reliable classes remain primary-only.
    pub fn send_redundant_audio(&self, pcm: &AudioPcmChunk) -> EonResult<AudioCopies> {
        let lane = self
            .media
            .as_ref()
            .ok_or_else(|| invalid("standby not enabled"))?;
        check_open(&self.conn)?;
        pcm.validate()?;
        let len = PREFIX + pcm.encoded_len();
        if len > MAX_AUDIO {
            return Err(invalid("redundant audio exceeds datagram budget"));
        }
        let id = {
            let mut next = lane.next.lock().unwrap();
            let id = *next;
            *next = id
                .checked_add(1)
                .ok_or_else(|| invalid("media sequence exhausted"))?;
            id
        };
        let mut wire = Vec::with_capacity(len);
        wire.push(COPY_ID_KEY);
        wire.extend_from_slice(&id.to_le_bytes());
        pcm.encode_raw(&mut wire);
        let copies = lane.enqueue(&self.conn, Bytes::from(wire))?;
        self.budget.observe(TrafficClass::Audio);
        Ok(copies)
    }
}

impl MediaLane {
    fn enqueue(&self, conn: &Connection, packet: Bytes) -> EonResult<AudioCopies> {
        let primary = conn.send_datagram(packet.clone());
        if let Err(iroh::endpoint::SendDatagramError::ConnectionLost(reason)) = primary {
            return Err(transport_error(reason));
        }
        check_open(conn)?;
        let standby = self.conn.send_datagram(packet);
        if primary.is_err() && standby.is_err() {
            return Err(transport_error("both media queues rejected the datagram"));
        }
        Ok(AudioCopies {
            primary: primary.is_ok(),
            standby: standby.is_ok(),
        })
    }
    pub(super) async fn close(self) {
        self.conn.close(0u8.into(), b"primary session ended");
        self.endpoint.close().await;
    }

    async fn read_packet(&self, primary: &Connection) -> EonResult<(Bytes, bool)> {
        loop {
            check_open(primary)?;
            let (packet, standby) = tokio::select! {
                p = primary.read_datagram() => (p.map_err(transport_error)?, false),
                p = self.conn.read_datagram(), if self.conn.close_reason().is_none() => {
                    // A dead standby must not end a healthy primary session.
                    let Ok(p) = p else { continue };
                    (p, true)
                }
            };
            return after_dequeue(primary, packet, standby);
        }
    }

    pub(super) async fn read_return(
        &self,
        primary: &Connection,
    ) -> Option<EonResult<Option<AudioPcmChunk>>> {
        let packet = match self.read_packet(primary).await {
            Ok((packet, _)) => packet,
            // Drain buffered primary returns only, never a revoked standby.
            Err(_) => primary.read_datagram().await.ok()?,
        };
        Some(decode_copy(packet, &self.received))
    }

    pub(super) async fn recv(&self, primary: &Connection) -> EonResult<(AudioPcmChunk, Arrival)> {
        loop {
            let (packet, standby) = self.read_packet(primary).await?;
            if !tagged(&packet) {
                if standby {
                    return Err(invalid("untagged standby datagram"));
                }
                return Ok((wire::decode_audio(packet)?, Arrival::Primary));
            }
            if let Some(pcm) = decode_copy(packet, &self.received)? {
                return Ok((
                    pcm,
                    if standby {
                        Arrival::StandbyCopy
                    } else {
                        Arrival::PrimaryCopy
                    },
                ));
            }
        }
    }
}
