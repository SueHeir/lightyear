use alloc::{boxed::Box, vec::Vec};
use core::net::SocketAddr;
use no_std_io2::io;

use super::{
    ClientId, MAX_PACKET_SIZE, MAX_PKT_BUF_SIZE, PACKET_SEND_RATE_SEC,
    bytes::Bytes,
    error::{Error, Result},
    packet::{
        DisconnectPacket, KeepAlivePacket, Packet, PayloadPacket, RequestPacket, ResponsePacket,
    },
    replay::ReplayProtection,
    token::{ChallengeToken, ConnectToken},
    utils,
};
use lightyear_link::{LinkReceiver, LinkSender, RecvPayload, SendPayload};
use lightyear_serde::writer::Writer;
use tracing::{debug, error, info, trace};

type Callback<Ctx> = Box<dyn FnMut(ClientState, ClientState, &mut Ctx) + Send + Sync + 'static>;

/// Configuration for a client.
///
/// * `num_disconnect_packets` - The number of redundant disconnect packets that will be sent to a server when the clients wants to disconnect.
/// * `packet_send_rate` - The rate at which periodic packets will be sent to the server.
/// * `on_state_change` - A callback that will be called when the client changes states.
///
/// # Example
/// ```
/// # struct MyContext;
/// #
/// # use lightyear_netcode::{generate_key, client::{ClientConfig, ClientState, Client}, Server};
/// # let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 40007));
/// # let private_key = generate_key();
/// # let token = Server::new(0x11223344, private_key).unwrap().token(123u64, addr).generate().unwrap();
/// # let token_bytes = token.try_into_bytes().unwrap();
///
/// let cfg = ClientConfig::with_context(MyContext {})
///     .num_disconnect_packets(10)
///     .packet_send_rate(0.1)
///     .on_state_change(|from, to, _ctx| {
///     if let (ClientState::SendingChallengeResponse, ClientState::Connected) = (from, to) {
///        println!("client connected to server");
///     }
/// });
/// let mut client = Client::with_config(&token_bytes, cfg).unwrap();
/// client.connect();
/// ```
pub struct ClientConfig<Ctx> {
    num_disconnect_packets: usize,
    packet_send_rate: f64,
    context: Ctx,
    on_state_change: Option<Callback<Ctx>>,
}

impl Default for ClientConfig<()> {
    fn default() -> Self {
        Self {
            num_disconnect_packets: 10,
            packet_send_rate: PACKET_SEND_RATE_SEC,
            context: (),
            on_state_change: None,
        }
    }
}

impl<Ctx> ClientConfig<Ctx> {
    /// Create a new, default client configuration with no context.
    pub fn new() -> ClientConfig<()> {
        ClientConfig::<()>::default()
    }
    /// Create a new client configuration with context that will be passed to the callbacks.
    pub fn with_context(ctx: Ctx) -> Self {
        Self {
            num_disconnect_packets: 10,
            packet_send_rate: PACKET_SEND_RATE_SEC,
            context: ctx,
            on_state_change: None,
        }
    }
    /// Set the number of redundant disconnect packets that will be sent to a server when the clients wants to disconnect.
    /// The default is 10 packets.
    pub fn num_disconnect_packets(mut self, num_disconnect_packets: usize) -> Self {
        self.num_disconnect_packets = num_disconnect_packets;
        self
    }
    /// Set the rate at which periodic packets will be sent to the server.
    /// The default is 10 packets per second. (`0.1` seconds)
    pub fn packet_send_rate(mut self, rate_seconds: f64) -> Self {
        self.packet_send_rate = rate_seconds;
        self
    }
    /// Set a callback that will be called when the client changes states.
    pub fn on_state_change<F>(mut self, cb: F) -> Self
    where
        F: FnMut(ClientState, ClientState, &mut Ctx) + Send + Sync + 'static,
    {
        self.on_state_change = Some(Box::new(cb));
        self
    }
}

/// The states in the client state machine.
///
/// The initial state is `Disconnected`.
/// When a client wants to connect to a server, it requests a connect token from the web backend.
/// To begin this process, it transitions to `SendingConnectionRequest` with the first server address in the connect token.
/// After that the client can either transition to `SendingChallengeResponse` or one of the error states.
/// While in `SendingChallengeResponse`, when the client receives a connection keep-alive packet from the server,
/// it stores the client index and max clients in the packet, and transitions to `Connected`.
///
/// Any payload packets received prior to `Connected` are discarded.
///
/// `Connected` is the final stage in the connection process and represents a successful connection to the server.
///
/// While in this state:
///
///  - The client application may send payload packets to the server.
///  - In the absence of payload packets sent by the client application, the client generates and sends connection keep-alive packets
///    to the server at some rate (default is 10HZ, can be overridden in [`ClientConfig`]).
///  - If no payload or keep-alive packets are received from the server within the timeout period specified in the connect token,
///    the client transitions to `ConnectionTimedOut`.
///  - While `Connected`, if the client receives a disconnect packet from the server, it transitions to `Disconnected`.
///    If the client wishes to disconnect from the server,
///    it sends a number of redundant connection disconnect packets (default is 10, can be overridden in [`ClientConfig`])
///    before transitioning to `Disconnected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClientState {
    /// The connect token has expired.
    ConnectTokenExpired,
    /// The client has timed out while trying to connect to the server,
    /// or while connected to the server due to a lack of packets received/sent.
    ConnectionTimedOut,
    /// The client has timed out while waiting for a response from the server after sending a connection request packet.
    ConnectionRequestTimedOut,
    /// The client has timed out while waiting for a response from the server after sending a challenge response packet.
    ChallengeResponseTimedOut,
    /// The server has denied the client's connection request, most likely due to the server being full.
    ConnectionDenied,
    /// The client is disconnected from the server.
    Disconnected,
    /// The client is waiting for a response from the server after sending a connection request packet.
    SendingConnectionRequest,
    /// The client is waiting for a response from the server after sending a challenge response packet.
    SendingChallengeResponse,
    /// The client is connected to the server.
    Connected,
}

/// The `netcode` client.
///
/// To create a client one should obtain a connection token from a web backend (by REST API or other means). <br>
/// The client will use this token to connect to the dedicated `netcode` server.
///
/// While the client is connected, it can send and receive packets to and from the server. <br>
/// Similarly to the server, the client should be updated at a fixed rate (e.g., 60Hz) to process incoming packets and send outgoing packets. <br>
///
/// # Example
/// ```
/// # use core::net::{Ipv4Addr, SocketAddr};
/// # use std::time::{Instant, Duration};
/// # use std::thread;
/// # use lightyear_link::Link;
/// # use lightyear_netcode::{client::Client, Server};
/// # let addr =  SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
/// # let mut link = Link::default();
/// # let mut server = Server::new(0, [0; 32]).unwrap();
/// # let token_bytes = server.token(0, addr).generate().unwrap().try_into_bytes().unwrap();
/// let mut client = Client::new(&token_bytes).unwrap();
/// client.connect();
/// ```
pub struct Client<Ctx = ()> {
    id: ClientId,
    state: ClientState,
    time: f64,
    start_time: f64,
    last_send_time: f64,
    last_receive_time: f64,
    server_addr_idx: usize,
    sequence: u64,
    challenge_token_sequence: u64,
    challenge_token_data: [u8; ChallengeToken::SIZE],
    token: ConnectToken,
    replay_protection: ReplayProtection,
    should_disconnect: bool,
    should_disconnect_state: ClientState,
    send_queue: Vec<SendPayload>,
    packet_queue: Vec<RecvPayload>,
    // We use a Writer (wrapper around BytesMut) here because we will keep re-using the
    // same allocation for the bytes we send.
    // 1. We create an array on the stack of size MAX_PACKET_SIZE
    // 2. We copy the serialized array in the writer via `extend_from_size`
    // 3. We split the bytes off, to recover the allocation
    writer: Writer,
    cfg: ClientConfig<Ctx>,
}

impl<Ctx> Client<Ctx> {
    fn from_token(token_bytes: &[u8], cfg: ClientConfig<Ctx>) -> Result<Self> {
        if token_bytes.len() != ConnectToken::SIZE {
            return Err(Error::SizeMismatch(ConnectToken::SIZE, token_bytes.len()));
        }
        let mut buf = [0u8; ConnectToken::SIZE];
        buf.copy_from_slice(token_bytes);
        let mut cursor = io::Cursor::new(&mut buf[..]);
        let token = match ConnectToken::read_from(&mut cursor) {
            Ok(token) => token,
            Err(err) => {
                error!("invalid connect token: {err}");
                return Err(Error::InvalidToken(err));
            }
        };
        Ok(Self {
            id: 0,
            state: ClientState::Disconnected,
            time: 0.0,
            start_time: 0.0,
            last_send_time: f64::NEG_INFINITY,
            last_receive_time: f64::NEG_INFINITY,
            server_addr_idx: 0,
            sequence: 0,
            challenge_token_sequence: 0,
            challenge_token_data: [0u8; ChallengeToken::SIZE],
            token,
            replay_protection: ReplayProtection::new(),
            should_disconnect: false,
            should_disconnect_state: ClientState::Disconnected,
            send_queue: Vec::new(),
            packet_queue: Vec::new(),
            writer: Writer::with_capacity(MAX_PKT_BUF_SIZE),
            cfg,
        })
    }
}

impl Client {
    /// Create a new client with a default configuration.
    ///
    /// # Example
    /// ```
    /// # use lightyear_netcode::{generate_key, ConnectToken, client::Client};
    /// // Generate a connection token for the client
    /// let private_key = generate_key();
    /// let token_bytes = ConnectToken::build("127.0.0.1:0", 0, 0, private_key)
    ///     .generate()
    ///     .unwrap()
    ///     .try_into_bytes()
    ///     .unwrap();
    ///
    /// let mut client = Client::new(&token_bytes).unwrap();
    /// ```
    pub fn new(token_bytes: &[u8]) -> Result<Self> {
        let client = Client::from_token(token_bytes, ClientConfig::default())?;
        // info!("client started on {}", client.io.local_addr());
        Ok(client)
    }
}

impl<Ctx> Client<Ctx> {
    /// Create a new client with a custom configuration. <br>
    /// Callbacks with context can be registered with the client to be notified when the client changes states. <br>
    /// See [`ClientConfig`] for more details.
    ///
    /// # Example
    /// ```ignore
    /// # use lightyear_netcode::{generate_key, client::{ClientConfig, ClientState}, ConnectToken, NetcodeClient};
    /// # let private_key = generate_key();
    /// # let token_bytes = ConnectToken::build("127.0.0.1:0", 0, 0, private_key)
    /// #    .generate()
    /// #    .unwrap()
    /// #    .try_into_bytes()
    /// #    .unwrap();
    /// struct MyContext {}
    /// let cfg = ClientConfig::with_context(MyContext {}).on_state_change(|from, to, _ctx| {
    ///    assert_eq!(from, ClientState::Disconnected);
    ///    assert_eq!(to, ClientState::SendingConnectionRequest);
    /// });
    ///
    /// let mut client = NetcodeClient::with_config(&token_bytes, cfg).unwrap();
    /// ```
    pub fn with_config(token_bytes: &[u8], cfg: ClientConfig<Ctx>) -> Result<Self> {
        let client = Client::from_token(token_bytes, cfg)?;
        Ok(client)
    }
}

impl<Ctx> Client<Ctx> {
    const ALLOWED_PACKETS: u8 = 1 << Packet::DENIED
        | 1 << Packet::CHALLENGE
        | 1 << Packet::KEEP_ALIVE
        | 1 << Packet::PAYLOAD
        | 1 << Packet::DISCONNECT;
    fn set_state(&mut self, state: ClientState) {
        debug!("client state changing from {:?} to {:?}", self.state, state);
        if let Some(ref mut cb) = self.cfg.on_state_change {
            cb(self.state, state, &mut self.cfg.context)
        }
        self.state = state;
    }
    fn reset_connection(&mut self) {
        self.start_time = self.time;
        self.last_send_time = self.time - 1.0; // force a packet to be sent immediately
        self.last_receive_time = self.time;
        self.should_disconnect = false;
        self.should_disconnect_state = ClientState::Disconnected;
        self.challenge_token_sequence = 0;
        self.replay_protection = ReplayProtection::new();
    }
    fn reset(&mut self, new_state: ClientState) {
        self.sequence = 0;
        self.start_time = 0.0;
        self.server_addr_idx = 0;
        self.set_state(new_state);
        self.reset_connection();
        debug!("client disconnected");
    }
    fn send_packets(&mut self) -> Result<()> {
        if self.last_send_time + self.cfg.packet_send_rate >= self.time {
            return Ok(());
        }
        let packet = match self.state {
            ClientState::SendingConnectionRequest => {
                debug!("client sending connection request packet to server");
                RequestPacket::create(
                    self.token.protocol_id,
                    self.token.expire_timestamp,
                    self.token.nonce,
                    self.token.private_data,
                )
            }
            ClientState::SendingChallengeResponse => {
                debug!("client sending connection response packet to server");
                ResponsePacket::create(self.challenge_token_sequence, self.challenge_token_data)
            }
            ClientState::Connected => {
                trace!("client sending connection keep-alive packet to server");
                KeepAlivePacket::create(0)
            }
            _ => return Ok(()),
        };
        self.send_netcode_packet(packet)
    }
    fn connect_to_next_server(&mut self) -> core::result::Result<(), ()> {
        if self.server_addr_idx + 1 >= self.token.server_addresses.len() {
            debug!("no more servers to connect to");
            return Err(());
        }
        self.server_addr_idx += 1;
        self.connect();
        Ok(())
    }
    fn send_packet(&mut self, packet: Packet, sender: &mut LinkSender) -> Result<()> {
        let mut buf = [0u8; MAX_PKT_BUF_SIZE];
        let size = packet.write(
            &mut buf,
            self.sequence,
            &self.token.client_to_server_key,
            self.token.protocol_id,
        )?;
        self.writer.extend_from_slice(&buf[..size]);
        sender.push(self.writer.split());
        self.last_send_time = self.time;
        self.sequence += 1;
        Ok(())
    }

    /// We buffer netcode packets (non-user-payload packets) instead of storing them in the link
    fn send_netcode_packet(&mut self, packet: Packet) -> Result<()> {
        let mut buf = [0u8; MAX_PKT_BUF_SIZE];
        let size = packet.write(
            &mut buf,
            self.sequence,
            &self.token.client_to_server_key,
            self.token.protocol_id,
        )?;
        self.writer.extend_from_slice(&buf[..size]);
        self.send_queue.push(self.writer.split());
        self.last_send_time = self.time;
        self.sequence += 1;
        Ok(())
    }

    pub fn server_addr(&self) -> SocketAddr {
        self.token.server_addresses[self.server_addr_idx]
    }
    fn process_packet(&mut self, packet: Packet) -> Result<Option<RecvPayload>> {
        // if addr != self.server_addr() {
        //     debug!(?addr, server_addr = ?self.server_addr(), "wrong addr");
        //     return Ok(());
        // }
        let recv = match (packet, self.state) {
            (
                Packet::Denied(pkt),
                ClientState::SendingConnectionRequest | ClientState::SendingChallengeResponse,
            ) => {
                error!(
                    "client connection denied by server. Reason: {:?}",
                    pkt.reason
                );
                self.should_disconnect = true;
                self.should_disconnect_state = ClientState::ConnectionDenied;
                None
            }
            (Packet::Challenge(pkt), ClientState::SendingConnectionRequest) => {
                debug!("client received connection challenge packet from server");
                self.challenge_token_sequence = pkt.sequence;
                self.challenge_token_data = pkt.token;
                self.set_state(ClientState::SendingChallengeResponse);
                None
            }
            (Packet::KeepAlive(_), ClientState::Connected) => {
                trace!("client received connection keep-alive packet from server");
                None
            }
            (Packet::KeepAlive(pkt), ClientState::SendingChallengeResponse) => {
                debug!("client received connection keep-alive packet from server");
                self.set_state(ClientState::Connected);
                self.id = pkt.client_id;
                debug!("client connected to server");
                None
            }
            (Packet::Payload(pkt), ClientState::Connected) => {
                // trace!(?pkt.buf, "client received payload packet from server");
                // TODO: control the size of the packet queue?
                Some(pkt.buf)
            }
            (Packet::Disconnect(_), ClientState::Connected) => {
                debug!("client received disconnect packet from server");
                self.should_disconnect = true;
                self.should_disconnect_state = ClientState::Disconnected;
                None
            }
            _ => return Ok(None),
        };
        self.last_receive_time = self.time;
        Ok(recv)
    }
    fn update_state(&mut self) {
        let is_token_expired = self.time - self.start_time
            >= self.token.expire_timestamp as f64 - self.token.create_timestamp as f64;
        let is_connection_timed_out = self.token.timeout_seconds.is_positive()
            && (self.last_receive_time + (self.token.timeout_seconds as f64) < self.time);
        let new_state = match self.state {
            ClientState::SendingConnectionRequest | ClientState::SendingChallengeResponse
                if is_token_expired =>
            {
                info!("client connect failed. connect token expired");
                ClientState::ConnectTokenExpired
            }
            _ if self.should_disconnect => {
                debug!(
                    "client should disconnect -> {:?}",
                    self.should_disconnect_state
                );
                if self.connect_to_next_server().is_ok() {
                    return;
                };
                self.should_disconnect_state
            }
            ClientState::SendingConnectionRequest if is_connection_timed_out => {
                info!("client connect failed. connection request timed out");
                if self.connect_to_next_server().is_ok() {
                    return;
                };
                ClientState::ConnectionRequestTimedOut
            }
            ClientState::SendingChallengeResponse if is_connection_timed_out => {
                info!("client connect failed. connection response timed out");
                if self.connect_to_next_server().is_ok() {
                    return;
                };
                ClientState::ChallengeResponseTimedOut
            }
            ClientState::Connected if is_connection_timed_out => {
                info!("client connection timed out");
                ClientState::ConnectionTimedOut
            }
            _ => return,
        };
        self.reset(new_state);
    }

    /// Read a packet received from the network, process it, and return the internal
    /// payload if it was a payload packet.
    fn recv_packet(&mut self, buf: RecvPayload, now: u64) -> Result<Option<RecvPayload>> {
        if buf.len() <= 1 {
            // Too small to be a packet
            return Ok(None);
        }
        let packet = match Packet::read(
            buf,
            self.token.protocol_id,
            now,
            self.token.server_to_client_key,
            Some(&mut self.replay_protection),
            Self::ALLOWED_PACKETS,
        ) {
            Ok(packet) => packet,
            Err(Error::Crypto(_)) => {
                debug!("client ignored packet because it failed to decrypt");
                return Ok(None);
            }
            Err(e) => {
                error!("client ignored packet: {e}");
                return Ok(None);
            }
        };
        self.process_packet(packet)
    }

    fn recv_packets(&mut self, receiver: &mut LinkReceiver) -> Result<()> {
        // number of seconds since unix epoch
        let now = utils::now()?;

        // we pop every packet that is currently in the receiver, then we process them
        // Processing them might mean that we're re-adding them to the receiver so that
        // the Transport can read them later
        for _ in 0..receiver.len() {
            if let Some(recv_packet) = receiver.pop() {
                if let Some(payload) = self.recv_packet(recv_packet, now)? {
                    receiver.push_raw(payload);
                }
            }
        }
        Ok(())
    }

    /// Returns the netcode client id of the client once it is connected, or returns 0 if not connected.
    pub fn id(&self) -> ClientId {
        self.id
    }

    /// Prepares the client to connect to the server.
    ///
    /// This function does not perform any IO, it only readies the client to send/receive packets on the next call to [`update`](Client::update).
    pub fn connect(&mut self) {
        self.reset_connection();
        self.set_state(ClientState::SendingConnectionRequest);
        info!(
            "client connecting to server {} [{}/{}]",
            self.token.server_addresses[self.server_addr_idx],
            self.server_addr_idx + 1,
            self.token.server_addresses.len()
        );
    }
    /// Updates the client.
    ///
    /// * Updates the client's elapsed time.
    /// * Receives packets from the server, any received payload packets will be queued.
    /// * Sends keep-alive or request/response packets to the server to establish/maintain a connection.
    /// * Updates the client's state - checks for timeouts, errors and transitions to new states.
    ///
    /// This method should be called regularly, probably at a fixed rate (e.g., 60Hz).
    ///
    /// # Panics
    /// Panics if the client can't send or receive packets.
    /// For a non-panicking version, use [`try_update`](Client::try_update).
    pub fn update(&mut self, delta_ms: f64, receiver: &mut LinkReceiver) -> ClientState {
        self.try_update(delta_ms, receiver)
            .expect("send/recv error while updating client")
    }

    /// The fallible version of [`update`](Client::update).
    ///
    /// Returns an error if the client can't send or receive packets.
    pub fn try_update(
        &mut self,
        delta_ms: f64,
        receiver: &mut LinkReceiver,
    ) -> Result<ClientState> {
        self.time += delta_ms;
        self.recv_packets(receiver)?;
        self.send_packets()?;
        self.update_state();
        Ok(self.state())
    }

    pub(crate) fn drain_send_netcode_packets(&mut self, sender: &mut LinkSender) {
        for packet in self.send_queue.drain(..) {
            sender.push(packet);
        }
    }

    /// Sends a packet to the server.
    ///
    /// The provided buffer must be smaller than [`MAX_PACKET_SIZE`].
    pub fn send(&mut self, buf: SendPayload, sender: &mut LinkSender) -> Result<()> {
        if self.state != ClientState::Connected {
            trace!("tried to send but not connected. We only send payload packets once connected");
            return Ok(());
        }
        if buf.len() > MAX_PACKET_SIZE {
            return Err(Error::SizeMismatch(MAX_PACKET_SIZE, buf.len()));
        }
        self.send_packet(PayloadPacket::create(buf), sender)?;
        Ok(())
    }
    /// Disconnects the client from the server.
    ///
    /// The client will send a number of redundant disconnect packets to the server before transitioning to `Disconnected`.
    pub fn disconnect(&mut self) -> Result<()> {
        debug!(
            "client sending {} disconnect packets to server",
            self.cfg.num_disconnect_packets
        );
        for _ in 0..self.cfg.num_disconnect_packets {
            self.send_netcode_packet(DisconnectPacket::create())?;
        }
        self.reset(ClientState::Disconnected);
        Ok(())
    }

    /// Gets the current state of the client.
    pub fn state(&self) -> ClientState {
        self.state
    }
    /// Returns true if the client is in an error state.
    pub fn is_error(&self) -> bool {
        self.state < ClientState::Disconnected
    }
    /// Returns true if the client is in a pending state.
    pub fn is_pending(&self) -> bool {
        self.state == ClientState::SendingConnectionRequest
            || self.state == ClientState::SendingChallengeResponse
    }
    /// Returns true if the client is connected to a server.
    pub fn is_connected(&self) -> bool {
        self.state == ClientState::Connected
    }
    /// Returns true if the client is disconnected from the server.
    pub fn is_disconnected(&self) -> bool {
        self.state == ClientState::Disconnected
    }
}

// TODO: put this test somewhere else

// #[cfg(test)]
// mod tests {
//     #[cfg(not(feature = "std"))]
//     use super::*;
//     use crate::client::networking::ClientCommandsExt;
//     use crate::prelude::client;
//     use crate::prelude::server::{NetServer, ServerCommandsExt};
//     use crate::tests::stepper::BevyStepper;
//     use bevy::prelude::State;
//     use lightyear_connection::server::ServerConnections;
//     use tracing::trace;
//
//     // TODO: investigate why this test is not working!
//     /// Check that if the client disconnects during the handshake, the server
//     /// gets rid of the client connection properly
//     #[test]
//     #[ignore]
//     fn test_client_disconnect_when_failing_handshake() {
//         let mut stepper = BevyStepper::default_no_init();
//
//         stepper.server_app.world_mut().start_server();
//         stepper.client_app().world_mut().connect_client();
//
//         // Wait until the server sees a single client
//         for _ in 0..100 {
//             if !stepper
//                 .server_app
//                 .world_mut()
//                 .resource_mut::<ServerConnections>()
//                 .servers[0]
//                 .connected_client_ids()
//                 .is_empty()
//             {
//                 break;
//             }
//             stepper.frame_step();
//         }
//
//         // TODO: how come this is necessary for the client to be able to enter the Disconnecting state?
//         //  if this is commented then the client does not enter Disconnecting state!
//         for _ in 0..30 {
//             stepper.frame_step();
//         }
//
//         trace!(
//             "Client NetworkingState: {:?}",
//             stepper
//                 .client_app
//                 .world()
//                 .resource::<State<client::NetworkingState>>()
//                 .get()
//         );
//         // Immediately disconnect the client
//         stepper.client_app().world_mut().disconnect_client();
//
//         // Wait for the client to time out
//         for _ in 0..10000 {
//             stepper.frame_step();
//         }
//
//         // The server should have successfully timed out the client
//         assert_eq!(
//             stepper
//                 .server_app
//                 .world_mut()
//                 .resource_mut::<ServerConnections>()
//                 .servers[0]
//                 .connected_client_ids(),
//             vec![]
//         );
//     }
// }
