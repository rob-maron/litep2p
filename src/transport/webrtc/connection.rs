// Copyright 2023 litep2p developers
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use crate::{
    codec::unsigned_varint::UnsignedVarint,
    crypto::{
        ed25519::Keypair,
        noise::{NoiseContext, STATIC_KEY_DOMAIN},
        PublicKey,
    },
    error::{Error, NegotiationError},
    multistream_select::{listener_negotiate, Message as MultiStreamMessage},
    peer_id::PeerId,
    protocol::{Direction, ProtocolEvent, ProtocolSet},
    substream::{channel::SubstreamBackend, SubstreamType},
    transport::{
        webrtc::{schema, util::WebRtcMessage, WebRtcEvent},
        TransportContext,
    },
    types::{ConnectionId, SubstreamId},
};

use bytes::BytesMut;
use multiaddr::{multihash::Multihash, Multiaddr, Protocol};
use prost::Message;
use str0m::{
    change::Fingerprint,
    channel::{ChannelData, ChannelId},
    net::Receive,
    Event, IceConnectionState, Input, Output, Rtc,
};
use tokio::{
    net::UdpSocket,
    sync::mpsc::{Receiver, Sender},
};
use tokio_util::codec::{Decoder, Encoder};

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

/// Logging target for the file.
const LOG_TARGET: &str = "webrtc::connection";

/// Substream context.
struct SubstreamContext {
    /// `str0m` channel id.
    channel_id: ChannelId,

    /// TX channel for sending messages to the protocol.
    tx: Sender<Vec<u8>>,
}

impl SubstreamContext {
    /// Create new [`SubstreamContext`].
    pub fn new(channel_id: ChannelId, tx: Sender<Vec<u8>>) -> Self {
        Self { channel_id, tx }
    }
}

/// WebRTC connection.
pub struct WebRtcConnection {
    /// `str0m` WebRTC object.
    rtc: Rtc,

    /// Transport context.
    context: TransportContext,

    /// WebRTC data channels.
    channels: HashMap<SubstreamId, SubstreamContext>,

    /// Remote address.
    remote_address: SocketAddr,

    /// Remote peer ID.
    remote_peer_id: PeerId,

    /// Local address.
    local_address: SocketAddr,

    /// Transport socket.
    socket: Arc<UdpSocket>,

    /// RX channel for receiving datagrams from the transport.
    dgram_rx: Receiver<Vec<u8>>,

    /// Substream backend.
    backend: SubstreamBackend,

    /// Next substream ID.
    substream_id: SubstreamId,

    /// ID mappings.
    id_mapping: HashMap<ChannelId, SubstreamId>,

    /// Noise context.
    noise_context: NoiseContext,

    /// Protocol set.
    protocol_set: ProtocolSet,
}

impl WebRtcConnection {
    pub(super) fn new(
        rtc: Rtc,
        remote_peer_id: PeerId,
        remote_address: SocketAddr,
        local_address: SocketAddr,
        context: TransportContext,
        socket: Arc<UdpSocket>,
        dgram_rx: Receiver<Vec<u8>>,
        noise_context: NoiseContext,
        protocol_set: ProtocolSet,
    ) -> WebRtcConnection {
        WebRtcConnection {
            rtc,
            socket,
            dgram_rx,
            context,
            protocol_set,
            noise_context,
            local_address,
            remote_address,
            remote_peer_id,
            channels: HashMap::new(),
            id_mapping: HashMap::new(),
            backend: SubstreamBackend::new(),
            substream_id: SubstreamId::new(),
        }
    }

    /// Poll output from the `Rtc` object.
    async fn poll_output(&mut self) -> crate::Result<WebRtcEvent> {
        match self.rtc.poll_output() {
            Ok(output) => self.handle_output(output).await,
            Err(error) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?error,
                    "`WebRtConnection::poll_output()` failed",
                );
                return Err(Error::WebRtc(error));
            }
        }
    }

    /// Handle data received from peer.
    pub(super) async fn on_input(&mut self, buffer: Vec<u8>) -> crate::Result<()> {
        let message = Input::Receive(
            Instant::now(),
            Receive {
                source: self.remote_address,
                destination: self.local_address,
                contents: buffer
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::InvalidData)?,
            },
        );

        match self.rtc.accepts(&message) {
            true => self.rtc.handle_input(message).map_err(|error| {
                tracing::debug!(target: LOG_TARGET, source = ?self.remote_address, ?error, "failed to handle data");
                Error::InputRejected
            }),
            false => return Err(Error::InputRejected),
        }
    }

    async fn handle_output(&mut self, output: Output) -> crate::Result<WebRtcEvent> {
        match output {
            Output::Transmit(transmit) => {
                self.socket
                    .send_to(&transmit.contents, transmit.destination)
                    .await
                    .expect("send to succeed");
                Ok(WebRtcEvent::Noop)
            }
            Output::Timeout(t) => Ok(WebRtcEvent::Timeout(t)),
            Output::Event(e) => match e {
                Event::IceConnectionStateChange(v) => {
                    if v == IceConnectionState::Disconnected {
                        tracing::debug!(target: LOG_TARGET, "ice connection closed");
                        return Err(Error::Disconnected);
                    }
                    Ok(WebRtcEvent::Noop)
                }
                Event::ChannelOpen(cid, name) => Ok(WebRtcEvent::Noop),
                Event::ChannelData(data) => self.on_channel_data(data).await,
                Event::ChannelClose(channel_id) => {
                    // TODO: notify the protocol
                    tracing::debug!(target: LOG_TARGET, ?channel_id, "channel closed");
                    Ok(WebRtcEvent::Noop)
                }
                Event::Connected => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        "client connected but is already connected"
                    );
                    return Err(Error::InvalidState);
                }
                event => {
                    tracing::warn!(target: LOG_TARGET, ?event, "unhandled event");
                    Ok(WebRtcEvent::Noop)
                }
            },
        }
    }

    /// Negotiate protocol for the channel
    async fn negotiate_protocol(&mut self, d: ChannelData) -> crate::Result<WebRtcEvent> {
        tracing::trace!(target: LOG_TARGET, channel_id = ?d.id, "negotiate protocol for the channel");

        let payload = WebRtcMessage::decode(&d.data)?
            .payload
            .ok_or(Error::InvalidData)?;

        let (protocol, response) =
            listener_negotiate(&mut self.context.protocols.keys(), payload.into())?;

        let message = WebRtcMessage::encode(response, None);

        self.rtc
            .channel(d.id)
            .ok_or(Error::ChannelDoesntExist)?
            .write(true, message.as_ref())
            .map_err(|error| Error::WebRtc(error))?;

        let substream_id = self.substream_id.next();
        let (substream, tx) = self.backend.substream(substream_id);
        self.id_mapping.insert(d.id, substream_id);
        self.channels
            .insert(substream_id, SubstreamContext::new(d.id, tx));

        let _ = self
            .protocol_set
            .report_substream_open(
                self.remote_peer_id,
                protocol.clone(),
                Direction::Inbound,
                // TODO: this is wrong
                SubstreamType::<tokio::net::TcpStream>::ChannelBackend(substream),
            )
            .await;

        Ok(WebRtcEvent::Noop)
    }

    /// Send received data to the protocol.
    async fn process_protocol_event(&mut self, d: ChannelData) -> crate::Result<WebRtcEvent> {
        tracing::debug!(
            target: LOG_TARGET,
            channel_id = ?d.id,
            "process protocol event",
        );

        // TODO: might be empty message with flags
        let message = WebRtcMessage::decode(&d.data)?
            .payload
            .ok_or(Error::InvalidData)?;

        match self.id_mapping.get(&d.id) {
            Some(id) => match self.channels.get_mut(&id) {
                Some(context) => {
                    let _ = context.tx.send(message).await;
                    Ok(WebRtcEvent::Noop)
                }
                None => {
                    tracing::error!(target: LOG_TARGET, "channel doesn't exist 1");
                    return Err(Error::ChannelDoesntExist);
                }
            },
            None => {
                tracing::error!(target: LOG_TARGET, "channel doesn't exist 2");
                return Err(Error::ChannelDoesntExist);
            }
        }
    }

    /// Handle channel data.
    async fn on_channel_data(&mut self, d: ChannelData) -> crate::Result<WebRtcEvent> {
        match self.id_mapping.get(&d.id) {
            Some(_) => self.process_protocol_event(d).await,
            None => self.negotiate_protocol(d).await,
        }
    }

    /// Run the event loop of a negotiated WebRTC connection.
    pub(super) async fn run(mut self) -> crate::Result<()> {
        loop {
            if !self.rtc.is_alive() {
                tracing::debug!(
                    target: LOG_TARGET,
                    "`Rtc` is not alive, closing `WebRtcHandshake`"
                );
                return Ok(());
            }

            let duration = match self.poll_output().await {
                Ok(WebRtcEvent::Timeout(timeout)) => {
                    let timeout =
                        std::cmp::min(timeout, Instant::now() + Duration::from_millis(100));
                    (timeout - Instant::now()).max(Duration::from_millis(1))
                }
                Ok(WebRtcEvent::Noop) => continue,
                Err(error) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?error,
                        "error occurred, closing connection"
                    );
                    self.rtc.disconnect();
                    return Ok(());
                }
            };

            tokio::select! {
                message = self.dgram_rx.recv() => match message {
                    Some(message) => match self.on_input(message).await {
                        Ok(_) | Err(Error::InputRejected) => {},
                        Err(error) => {
                            tracing::debug!(target: LOG_TARGET, ?error, "failed to handle input");
                            return Err(error)
                        }
                    }
                    None => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            source = ?self.remote_address,
                            "transport shut down, shutting down connection",
                        );
                        return Ok(());
                    }
                },
                event = self.backend.next_event() => {
                    let (id, message) = event.ok_or(Error::EssentialTaskClosed)?;
                    let channel_id = self.channels.get_mut(&id).ok_or(Error::ChannelDoesntExist)?.channel_id;

                    tracing::trace!(target: LOG_TARGET, ?id, ?message, "send message to remote peer");

                    self.rtc
                        .channel(channel_id)
                        .ok_or(Error::ChannelDoesntExist)?
                        .write(true, message.as_ref())
                        .map_err(|error| Error::WebRtc(error))?;
                }
                command = self.protocol_set.next_event() => match command {
                    Some(ProtocolEvent::OpenSubstream { .. }) => {
                        tracing::info!(target: LOG_TARGET, "handle open substream command from protocol");
                    }
                    None => return Err(Error::EssentialTaskClosed),
                },
                _ = tokio::time::sleep(duration) => {}
            }

            // drive time forward in the client
            if let Err(error) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?error,
                    "failed to handle timeout for `Rtc`"
                );

                self.rtc.disconnect();
                return Err(Error::Disconnected);
            }
        }
    }
}
