use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures::{ready, FutureExt, Sink, SinkExt, Stream};
use pin_project_lite::pin_project;
use tracing::{debug, error, warn};

use crate::errors::CodecError;
use crate::packet::connected::Frames;
use crate::packet::{connected, unconnected, Packet};
use crate::Peer;

pub(super) trait HandleOffline: Sized {
    fn handle_offline(self, config: Config) -> OfflineHandler<Self>;
}

impl<F> HandleOffline for F
where
    F: Stream<Item = (Packet<Frames<BytesMut>>, SocketAddr)>
        + Sink<(Packet<Frames<Bytes>>, SocketAddr), Error = CodecError>,
{
    fn handle_offline(self, config: Config) -> OfflineHandler<Self> {
        OfflineHandler {
            frame: self,
            pending: lru::LruCache::new(
                NonZeroUsize::new(config.max_pending).expect("max_pending > 0"),
            ),
            config,
            connected: HashMap::new(),
            sending: None,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Config {
    sever_guid: u64,
    advertisement: Bytes,
    min_mtu: u16,
    max_mtu: u16,
    // Supported raknet versions, sorted
    support_version: Vec<u8>,
    max_pending: usize,
}

type SendSelfRef<F> = futures::sink::Send<
    'static,
    Pin<&'static mut F>,
    (Packet<Vec<connected::Frame<Bytes>>>, SocketAddr),
>;

pin_project! {
    /// OfflineHandler takes the codec frame and perform offline handshake.
    #[project(!Unpin)] // OfflineHandler is !Unpin even F is Unpin
    pub(super) struct OfflineHandler<F: 'static> {
        #[pin]
        frame: F,
        config: Config,
        pending: lru::LruCache<SocketAddr, u8>,
        connected: HashMap<SocketAddr, Peer>,
        // refer to self.frame, send notification to client
        sending: Option<SendSelfRef<F>>,
    }
}

impl<F> OfflineHandler<F> {
    pub(super) fn disconnect(self: Pin<&mut Self>, addr: &SocketAddr) {
        let this = self.project();
        this.pending.pop(addr);
        this.connected.remove(addr);
    }
}

impl<F> OfflineHandler<F>
where
    F: Sink<(Packet<Frames<Bytes>>, SocketAddr), Error = CodecError>,
{
    fn make_incompatible_version(config: &Config) -> Packet<Frames<Bytes>> {
        Packet::Unconnected(unconnected::Packet::IncompatibleProtocol {
            server_protocol: *config.support_version.last().unwrap(),
            magic: (),
            server_guid: config.sever_guid,
        })
    }

    fn make_already_connected(config: &Config) -> Packet<Frames<Bytes>> {
        Packet::Unconnected(unconnected::Packet::AlreadyConnected {
            magic: (),
            server_guid: config.sever_guid,
        })
    }

    fn make_connection_request_failed(config: &Config) -> Packet<Frames<Bytes>> {
        Packet::Unconnected(unconnected::Packet::ConnectionRequestFailed {
            magic: (),
            server_guid: config.sever_guid,
        })
    }
}

impl<F> Stream for OfflineHandler<F>
where
    F: Stream<Item = (Packet<Frames<BytesMut>>, SocketAddr)>
        + Sink<(Packet<Frames<Bytes>>, SocketAddr), Error = CodecError>,
{
    type Item = (connected::Packet<Frames<BytesMut>>, Peer);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        loop {
            if let Some(sending) = this.sending {
                if let Err(err) = ready!(sending.poll_unpin(cx)) {
                    error!("send error: {err}");
                }
            }
            // there is no sending or sending is finished, reset the sending
            *this.sending = None;
            let Some((packet, addr)) = ready!(this.frame.as_mut().poll_next(cx)) else {
                return Poll::Ready(None);
            };
            let pack = match packet {
                Packet::Unconnected(pack) => pack,
                Packet::Connected(pack) => {
                    if let Some(peer) = this.connected.get(&addr) {
                        return Poll::Ready(Some((pack, peer.clone())));
                    }
                    debug!("ignore connected packet from unconnected client {addr}");
                    // TODO: Send DETECT_LOST_CONNECTION ?
                    let send = this
                        .frame
                        .send((Self::make_connection_request_failed(this.config), addr));
                    // Breaking limit from borrow checker
                    // Safety:
                    // Self-reference struct has the 'static lifetime reference to itself
                    // We never allow &mut T access unpinned field in `OfflineHandler` and
                    // `OfflineHandler` itself.
                    *this.sending = unsafe { std::mem::transmute(Some(send)) };
                    continue;
                }
            };
            let resp = match pack {
                unconnected::Packet::UnconnectedPing { send_timestamp, .. } => {
                    unconnected::Packet::UnconnectedPong {
                        send_timestamp,
                        server_guid: this.config.sever_guid,
                        magic: (),
                        data: this.config.advertisement.clone(),
                    }
                }
                unconnected::Packet::OpenConnectionRequest1 {
                    protocol_version,
                    mtu,
                    ..
                } => {
                    if this
                        .config
                        .support_version
                        .binary_search(&protocol_version)
                        .is_err()
                    {
                        let send = this
                            .frame
                            .send((Self::make_incompatible_version(this.config), addr));
                        *this.sending = unsafe { std::mem::transmute(Some(send)) };
                        continue;
                    }
                    if this.pending.put(addr, protocol_version).is_some() {
                        debug!("received duplicate open connection request 1 from {addr}");
                    }
                    // max_mtu >= final_mtu >= min_mtu
                    let final_mtu = this.config.max_mtu.min(this.config.min_mtu.max(mtu));
                    unconnected::Packet::OpenConnectionReply1 {
                        magic: (),
                        server_guid: this.config.sever_guid,
                        use_encryption: false, // must set to false first
                        mtu: final_mtu,
                    }
                }
                unconnected::Packet::OpenConnectionRequest2 { mtu, .. } => {
                    if this.pending.pop(&addr).is_none() {
                        debug!("received open connection request 2 from {addr} without open connection request 1");
                        let send = this
                            .frame
                            .send((Self::make_incompatible_version(this.config), addr));
                        *this.sending = unsafe { std::mem::transmute(Some(send)) };
                        continue;
                    };
                    // client should adjust the mtu
                    if mtu < this.config.min_mtu
                        || mtu > this.config.max_mtu
                        || this.connected.contains_key(&addr)
                    {
                        let send = this
                            .frame
                            .send((Self::make_already_connected(this.config), addr));
                        *this.sending = unsafe { std::mem::transmute(Some(send)) };
                        continue;
                    }
                    this.connected.insert(addr, Peer { addr, mtu });
                    unconnected::Packet::OpenConnectionReply2 {
                        magic: (),
                        server_guid: this.config.sever_guid,
                        client_address: addr,
                        mtu,
                        encryption_enabled: false, // must set to false
                    }
                }
                _ => {
                    warn!(
                        "received a package({:?}) that should not be received on the server.",
                        pack.pack_type()
                    );
                    continue;
                }
            };
            let send = this.frame.send((Packet::Unconnected(resp), addr));
            *this.sending = unsafe { std::mem::transmute(Some(send)) };
        }
    }
}

#[cfg(test)]
mod test {
    use std::collections::VecDeque;

    use futures::StreamExt;

    use super::*;

    struct TestCase {
        source: VecDeque<Packet<Frames<BytesMut>>>,
        dst: Vec<Packet<Frames<Bytes>>>,
    }

    impl Stream for TestCase {
        type Item = (Packet<Frames<BytesMut>>, SocketAddr);

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
            if let Some(pack) = self.source.pop_front() {
                return Poll::Ready(Some((pack, addr)));
            }
            Poll::Ready(None)
        }
    }

    impl Sink<(Packet<Frames<Bytes>>, SocketAddr)> for TestCase {
        type Error = CodecError;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(
            mut self: Pin<&mut Self>,
            item: (Packet<Frames<Bytes>>, SocketAddr),
        ) -> Result<(), Self::Error> {
            self.dst.push(item.0);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn test_offline_handshake_works() {
        let test_case = TestCase {
            source: vec![
                unconnected::Packet::UnconnectedPing {
                    send_timestamp: 0,
                    magic: (),
                    client_guid: 114514,
                },
                unconnected::Packet::OpenConnectionRequest1 {
                    magic: (),
                    protocol_version: 11,
                    mtu: 1000,
                },
                unconnected::Packet::OpenConnectionRequest2 {
                    magic: (),
                    server_address: "0.0.0.0:1".parse().unwrap(),
                    mtu: 1000,
                    client_guid: 114514,
                },
            ]
            .into_iter()
            .map(Packet::Unconnected)
            .collect(),
            dst: vec![],
        };
        let handler = test_case.handle_offline(Config {
            sever_guid: 1919810,
            advertisement: Bytes::from_static(b"hello"),
            min_mtu: 800,
            max_mtu: 1400,
            support_version: vec![8, 11, 12],
            max_pending: 10,
        });
        tokio::pin!(handler);
        assert!(handler.next().await.is_none());
        assert_eq!(
            handler.project().frame.dst,
            vec![
                unconnected::Packet::UnconnectedPong {
                    send_timestamp: 0,
                    server_guid: 1919810,
                    magic: (),
                    data: Bytes::from_static(b"hello")
                },
                unconnected::Packet::OpenConnectionReply1 {
                    magic: (),
                    server_guid: 1919810,
                    use_encryption: false,
                    mtu: 1000
                },
                unconnected::Packet::OpenConnectionReply2 {
                    magic: (),
                    server_guid: 1919810,
                    client_address: "0.0.0.0:0".parse().unwrap(),
                    mtu: 1000,
                    encryption_enabled: false
                },
            ]
            .into_iter()
            .map(Packet::Unconnected)
            .collect::<Vec<_>>()
        );
    }

    // TODO: add more test
}
