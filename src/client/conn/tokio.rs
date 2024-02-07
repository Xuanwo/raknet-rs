use std::io;
use std::net::ToSocketAddrs;
use std::sync::Arc;

use log::debug;
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio_util::udp::UdpFramed;

use super::ConnectTo;
use crate::client::handler::offline::HandleOffline;
use crate::client::handler::online::HandleOnline;
use crate::codec::{Codec, Decoded, Encoded};
use crate::common::ack::{HandleIncomingAck, HandleOutgoingAck};
use crate::errors::{CodecError, Error};
use crate::utils::{IOImpl, Logged, WithAddress};
use crate::IO;

impl ConnectTo for TokioUdpSocket {
    async fn connect_to(
        self,
        addrs: impl ToSocketAddrs,
        config: super::Config,
    ) -> Result<impl IO, Error> {
        fn err_f(err: CodecError) {
            debug!("[frame] got codec error: {err} when decode frames");
        }
        let socket = Arc::new(self);

        let (incoming_ack_tx, incoming_ack_rx) = flume::unbounded();
        let (incoming_nack_tx, incoming_nack_rx) = flume::unbounded();

        let (outgoing_ack_tx, outgoing_ack_rx) = flume::unbounded();
        let (outgoing_nack_tx, outgoing_nack_rx) = flume::unbounded();

        let mut lookups = addrs.to_socket_addrs()?;

        let addr = loop {
            if let Some(addr) = lookups.next() {
                if socket.connect(addr).await.is_ok() {
                    break addr;
                }
                continue;
            }
            return Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "invalid address").into());
        };

        let write = UdpFramed::new(Arc::clone(&socket), Codec)
            .with_addr(addr)
            .handle_outgoing_ack(
                incoming_ack_rx,
                incoming_nack_rx,
                outgoing_ack_rx,
                outgoing_nack_rx,
                config.send_buf_cap,
                config.offline.mtu,
            )
            .frame_encoded(config.offline.mtu, config.codec);

        let io = UdpFramed::new(socket, Codec)
            .logged_err(err_f)
            .handle_offline(addr, config.offline)
            .await?
            .handle_incoming_ack(incoming_ack_tx, incoming_nack_tx)
            .decoded(config.codec, outgoing_ack_tx, outgoing_nack_tx)
            .handle_online(write, addr, config.offline.client_guid)
            .await?;

        Ok(IOImpl::new(io))
    }
}
