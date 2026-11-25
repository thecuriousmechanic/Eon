use super::*;
use iroh::endpoint::transports::{
    CustomEndpoint, CustomSender, CustomTransport, RecvInfo, Transmit,
};
use iroh_base::CustomAddr;
use n0_watcher::{Direct as AddrWatcher, Watchable};
use std::{
    io,
    net::SocketAddr,
    task::{Context, Poll},
};
use tokio::io::ReadBuf;

pub(in crate::wan) fn address(addr: SocketAddrV4) -> CustomAddr {
    let mut bytes = [0; 6];
    bytes[..4].copy_from_slice(&addr.ip().octets());
    bytes[4..].copy_from_slice(&addr.port().to_be_bytes());
    CustomAddr::from_parts(0x454f4e84, &bytes)
}

#[derive(Debug, Clone)]
pub(super) struct Direct {
    socket: Arc<UdpSocket>,
    own: CustomAddr,
    peer: CustomAddr,
    source: SocketAddr,
    local: Watchable<Vec<CustomAddr>>,
}

impl Direct {
    pub(super) fn new(socket: UdpSocket, own: SocketAddrV4, peer: SocketAddrV4) -> Self {
        let own = address(own);
        Self {
            socket: Arc::new(socket),
            local: Watchable::new(vec![own.clone()]),
            own,
            peer: address(peer),
            source: peer.into(),
        }
    }
}

impl CustomTransport for Direct {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        Ok(Box::new(self.clone()))
    }
}

impl CustomSender for Direct {
    fn is_valid_send_addr(&self, addr: &CustomAddr) -> bool {
        addr == &self.peer
    }

    fn poll_send(
        &self,
        cx: &mut Context,
        dst: &CustomAddr,
        src: Option<&CustomAddr>,
        tx: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        if tx.segment_size.is_some_and(|n| n < tx.contents.len()) {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::InvalidInput)));
        }
        self.send_to_peer(cx, dst, src, tx.contents)
    }
}

impl Direct {
    fn send_to_peer(
        &self,
        cx: &mut Context,
        dst: &CustomAddr,
        src: Option<&CustomAddr>,
        data: &[u8],
    ) -> Poll<io::Result<()>> {
        if !self.is_valid_send_addr(dst) || src.is_some_and(|a| a != &self.own) {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::PermissionDenied)));
        }
        self.socket.poll_send(cx, data).map_ok(|_| ())
    }
}

impl CustomEndpoint for Direct {
    fn watch_local_addrs(&self) -> AddrWatcher<Vec<CustomAddr>> {
        self.local.watch()
    }
    fn create_sender(&self) -> Arc<dyn CustomSender> {
        Arc::new(self.clone())
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [noq_udp::RecvMeta],
        infos: &mut [RecvInfo],
    ) -> Poll<io::Result<usize>> {
        let mut buf = ReadBuf::new(&mut bufs[0]);
        // Connected UDP may report asynchronous route/ICMP errors on recv.
        // They mean a lost packet on *this path*, not a fatal QUIC endpoint.
        match self.socket.poll_recv_from(cx, &mut buf) {
            Poll::Ready(Ok(src)) if src == self.source && !buf.filled().is_empty() => {
                let len = buf.filled().len();
                metas[0] = noq_udp::RecvMeta::default();
                metas[0].len = len;
                metas[0].stride = len;
                infos[0] = RecvInfo::new(self.peer.clone(), Some(self.own.clone()));
                Poll::Ready(Ok(1))
            }

            Poll::Pending => Poll::Pending,
            _ => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn iroh_and_qos_tests_custom_sender_rejects_even_in_pool_substitution_before_send() {
        let bind = async || UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (socket, peer, other) = tokio::join!(bind(), bind(), bind());
        let v4 = |s: &UdpSocket| match s.local_addr().unwrap() {
            SocketAddr::V4(addr) => addr,
            _ => unreachable!(),
        };
        let (own, allowed, foreign) = (v4(&socket), v4(&peer), v4(&other));
        socket.connect(allowed).await.unwrap();
        let direct = Direct::new(socket, own, allowed);
        let attempted = async |dst: CustomAddr| {
            std::future::poll_fn(|cx| direct.send_to_peer(cx, &dst, None, b"one")).await
        };
        assert_eq!(
            attempted(address(foreign)).await.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            attempted(CustomAddr::from_parts(7, b"any"))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let mut bytes = [0; 16];
        assert_eq!(
            peer.try_recv(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        attempted(address(allowed)).await.unwrap();
        let (n, src) = timeout(Duration::from_secs(2), peer.recv_from(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(src, SocketAddr::V4(own));
        assert_eq!(&bytes[..n], b"one");
        assert_eq!(
            other.try_recv(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
