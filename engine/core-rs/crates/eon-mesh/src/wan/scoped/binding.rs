use super::*;
use ring::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};

const COOKIE: u32 = 0x2112_a442;
const INTEGRITY: [u8; 4] = [0, 0x1c, 0, 32];
// Private comprehension-optional attribute: peer Ed25519 pin + six-byte
// IPv4/port (network order), two zero padding bytes. Both attributes precede MI.
const PAIRED: u16 = 0xc0e4;
const PROOF: [u8; 4] = [0xc0, 0xe5, 0, 16];
const WAIT_MS: [u8; 4] = [0xc0, 0xe6, 0, 4];
type Mapping = (SocketAddrV4, SocketAddrV4);

#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Challenge([u8; 16]),
    Pair(Mapping),
}

fn request(user: &[u8], tx: &[u8; 12], key: &hmac::Key, proof: Option<([u8; 16], u32)>) -> Vec<u8> {
    let padded = user.len().next_multiple_of(4);
    let mut msg = vec![0, 1];
    msg.extend_from_slice(&[0; 2]);
    msg.extend_from_slice(&COOKIE.to_be_bytes());
    msg.extend_from_slice(tx);
    msg.extend_from_slice(&[0, 6, 0, user.len() as u8]);
    msg.extend_from_slice(user);
    msg.resize(24 + padded, 0);
    if let Some((token, wait_ms)) = proof {
        msg.extend_from_slice(&PROOF);
        msg.extend_from_slice(&token);
        msg.extend_from_slice(&WAIT_MS);
        msg.extend_from_slice(&wait_ms.to_be_bytes());
    }
    // RFC 5780 PADDING keeps even the 112-byte pair response within the
    // request's wire budget. The publisher never accepts an alternate target.
    let padding = 112usize.saturating_sub(msg.len() + 4 + 36);
    msg.extend_from_slice(&[0, 0x26, 0, padding as u8]);
    msg.resize(msg.len() + padding, 0);
    let length = (msg.len() - 20 + 36) as u16;
    msg[2..4].copy_from_slice(&length.to_be_bytes());
    let tag = hmac::sign(key, &msg);
    msg.extend_from_slice(&INTEGRITY);
    msg.extend_from_slice(tag.as_ref());
    msg
}

fn response(
    packet: &[u8],
    tx: &[u8; 12],
    key: &hmac::Key,
    pin: EndpointId,
) -> EonResult<Option<Answer>> {
    // Fixed, bounded profile. Ignore unrelated/malformed/unauthenticated
    // datagrams; never honor redirect, alternate server or a trailing attribute.
    if !(56..=256).contains(&packet.len())
        || packet[..2] != [1, 1]
        || usize::from(u16::from_be_bytes([packet[2], packet[3]])) != packet.len() - 20
        || packet[4..8] != COOKIE.to_be_bytes()
        || packet[8..20] != *tx
    {
        return Ok(None);
    }
    let end = packet.len() - 36;
    if packet[end..end + 4] != INTEGRITY
        || hmac::verify(key, &packet[..end], &packet[end + 4..]).is_err()
    {
        return Ok(None);
    }
    let attrs = &packet[20..end];
    if attrs.len() == 12 {
        return Ok(None);
    } // standard server: no private publication
    if attrs.len() == 32 && attrs[..6] == [0, 0x20, 0, 8, 0, 1] && attrs[12..16] == PROOF {
        return Ok(Some(Answer::Challenge(attrs[16..32].try_into().unwrap())));
    }
    if attrs.len() != 56
        || attrs[..6] != [0, 0x20, 0, 8, 0, 1]
        || attrs[12..14] != PAIRED.to_be_bytes()
        || attrs[14..16] != [0, 38]
        || attrs[16..48] != *pin.as_bytes()
    {
        return Err(invalid("invalid authenticated paired Binding response"));
    }
    let port = u16::from_be_bytes(attrs[6..8].try_into().unwrap()) ^ (COOKIE >> 16) as u16;
    let ip = u32::from_be_bytes(attrs[8..12].try_into().unwrap()) ^ COOKIE;
    let peer = SocketAddrV4::new(
        Ipv4Addr::from(<[u8; 4]>::try_from(&attrs[48..52]).unwrap()),
        u16::from_be_bytes(attrs[52..54].try_into().unwrap()),
    );
    Ok(Some(Answer::Pair((
        SocketAddrV4::new(ip.into(), port),
        peer,
    ))))
}

pub(super) async fn discover(
    socket: &UdpSocket,
    cfg: &PrivateWanConfig,
    pin: EndpointId,
    discovery_end: Instant,
) -> EonResult<Option<Mapping>> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, &cfg.stun_password);
    let mut buf = [0; 257];
    for ms in [250, 500, 1000] {
        let now = Instant::now();
        if now >= discovery_end {
            break;
        }
        let deadline = discovery_end.min(now + Duration::from_millis(ms));
        // A confirmed request pays for at most one pair reply. Recover from
        // packet loss with a fresh transaction/proof, never a replay credit.
        let mut tx = [0; 12];
        SystemRandom::new()
            .fill(&mut tx)
            .map_err(|_| transport_error("STUN randomness unavailable"))?;
        let mut proved = false;
        let req = request(cfg.stun_username.as_bytes(), &tx, &key, None);
        if socket.send_to(&req, cfg.stun).await.is_err() {
            return Ok(None);
        }
        while let Ok(Ok((n, source))) =
            tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await
        {
            if source == cfg.stun.into()
                && let Some(answer) = response(&buf[..n], &tx, &key, pin)?
            {
                match answer {
                    Answer::Pair(mapping) => return Ok(Some(mapping)),
                    Answer::Challenge(token) if !proved => {
                        let wait_ms = deadline
                            .saturating_duration_since(Instant::now())
                            .as_millis() as u32;
                        if wait_ms == 0 {
                            break;
                        }
                        proved = true;
                        let req = request(
                            cfg.stun_username.as_bytes(),
                            &tx,
                            &key,
                            Some((token, wait_ms)),
                        );
                        if socket.send_to(&req, cfg.stun).await.is_err() {
                            return Ok(None);
                        }
                    }
                    Answer::Challenge(_) => {}
                }
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iroh_and_qos_tests_paired_binding_checks_transaction_integrity_pin_and_scope() {
        let pin = SecretKey::generate().public();
        let key = hmac::Key::new(hmac::HMAC_SHA256, &[23; 32]);
        let tx = [8; 12];
        let own: SocketAddrV4 = "10.77.0.2:41770".parse().unwrap();
        let peer: SocketAddrV4 = "10.77.0.3:41771".parse().unwrap();
        let mut reply = vec![1, 1, 0, 92];
        reply.extend(COOKIE.to_be_bytes());
        reply.extend(tx);
        reply.extend([0, 0x20, 0, 8, 0, 1]);
        reply.extend((own.port() ^ 0x2112).to_be_bytes());
        reply.extend((u32::from(*own.ip()) ^ COOKIE).to_be_bytes());
        reply.extend(PAIRED.to_be_bytes());
        reply.extend([0, 38]);
        reply.extend(pin.as_bytes());
        reply.extend(peer.ip().octets());
        reply.extend(peer.port().to_be_bytes());
        reply.extend([0; 2]);
        let tag = hmac::sign(&key, &reply);
        reply.extend(INTEGRITY);
        reply.extend(tag.as_ref());
        assert_eq!(
            response(&reply, &tx, &key, pin).unwrap(),
            Some(Answer::Pair((own, peer)))
        );
        assert!(response(&reply, &[9; 12], &key, pin).unwrap().is_none());
        assert!(response(&reply, &tx, &key, SecretKey::generate().public()).is_err());
        for i in 0..reply.len() {
            let mut changed = reply.clone();
            changed[i] ^= 0x01;
            assert!(response(&changed, &tx, &key, pin).unwrap().is_none());
            assert!(response(&reply[..i], &tx, &key, pin).unwrap().is_none());
        }
        let cfg = PrivateWanConfig {
            bind: "192.168.1.2:0".parse().unwrap(),
            stun: "10.77.0.1:3478".parse().unwrap(),
            relay: "https://10.77.0.1:8443".parse().unwrap(),
            relay_root_der: vec![1],
            stun_username: "alice".into(),
            stun_password: vec![1; 32],
            candidates: CandidateScope::new("10.77.0.0".parse().unwrap(), 29, 41770..=41771)
                .unwrap(),
        };
        cfg.admit(own, peer).unwrap();
        for bad in [
            "10.77.0.7:41770",
            "10.77.0.0:41770",
            "10.77.0.1:41770",
            "10.77.0.10:41770",
            "10.77.0.3:41772",
            "192.168.1.2:41770",
            "127.0.0.1:41770",
            "203.0.113.1:41770",
            "224.0.0.1:41770",
        ] {
            let bad = bad.parse().unwrap();
            assert!(cfg.admit(own, bad).is_err());
            assert!(cfg.admit(bad, peer).is_err());
        }
        assert!(cfg.admit(own, own).is_err());
        for (ip, prefix, ports) in [
            ("203.0.113.0", 24, 41770..=41771),
            ("10.0.0.0", 8, 41770..=41771),
            ("10.0.0.3", 24, 41770..=41771),
            ("10.0.0.0", 24, 0..=65535),
        ] {
            assert!(CandidateScope::new(ip.parse().unwrap(), prefix, ports).is_err());
        }
    }
}
