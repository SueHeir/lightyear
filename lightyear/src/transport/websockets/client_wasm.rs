use std::{
    future::Future,
    io::BufReader,
    net::{SocketAddr, SocketAddrV4},
    sync::Arc,
};

use anyhow::Result;
use bevy::utils::hashbrown::HashMap;

use tokio::sync::mpsc::unbounded_channel;
use tracing::{debug, info, trace};
use tracing_log::log::error;

use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{
    js_sys::{ArrayBuffer, Uint8Array},
    BinaryType, CloseEvent, ErrorEvent, MessageEvent, WebSocket,
};

use crate::transport::{PacketReceiver, PacketSender, Transport, LOCAL_SOCKET};

use super::MTU;

pub struct WebSocketClientSocket {
    server_addr: SocketAddr,
}

impl WebSocketClientSocket {
    pub(crate) fn new(server_addr: SocketAddr) -> Self {
        Self { server_addr }
    }
}

impl Transport for WebSocketClientSocket {
    fn local_addr(&self) -> SocketAddr {
        LOCAL_SOCKET
    }

    fn listen(self) -> (Box<dyn PacketSender>, Box<dyn PacketReceiver>) {
        let (serverbound_tx, mut serverbound_rx) = unbounded_channel::<Vec<u8>>();
        let (clientbound_tx, clientbound_rx) = unbounded_channel::<Vec<u8>>();

        let packet_sender = WebSocketClientSocketSender { serverbound_tx };

        let packet_receiver = WebSocketClientSocketReceiver {
            buffer: [0; MTU],
            server_addr: self.server_addr,
            clientbound_rx,
        };

        info!("Starting client websocket task");

        let ws = WebSocket::new(&format!("ws://{}/", self.server_addr))
            .expect("Unable to connect to websocket server");

        ws.set_binary_type(BinaryType::Arraybuffer);

        let on_open_callback = Closure::<dyn FnMut(_)>::new(move || {
            info!("WebSocket handshake has been successfully completed");
        });

        let on_message_callback = Closure::<dyn FnMut(_)>::new(move |e: MessageEvent| {
            let msg = Uint8Array::new(&e.data()).to_vec();

            clientbound_tx
                .send(msg)
                .expect("Unable to propagate the read websocket message to the receiver");
        });

        let on_close_callback = Closure::<dyn FnMut(_)>::new(move |e: CloseEvent| {
            info!(
                "WebSocket connection closed with code {} and reason {}",
                e.code(),
                e.reason()
            );
        });

        let on_error_callback = Closure::<dyn FnMut(_)>::new(move |e: ErrorEvent| {
            info!("WebSocket connection error {}", e.message());
        });

        ws.set_onopen(Some(on_open_callback.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(on_message_callback.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close_callback.as_ref().unchecked_ref()));
        ws.set_onerror(Some(on_error_callback.as_ref().unchecked_ref()));

        on_open_callback.forget();
        on_message_callback.forget();
        on_close_callback.forget();
        on_error_callback.forget();

        tokio::spawn(async move {
            while let Some(msg) = serverbound_rx.recv().await {
                ws.send_with_u8_array(&msg).unwrap();
            }
        });

        (Box::new(packet_sender), Box::new(packet_receiver))
    }
}

struct WebSocketClientSocketSender {
    serverbound_tx: UnboundedSender<Vec<u8>>,
}

impl PacketSender for WebSocketClientSocketSender {
    fn send(&mut self, payload: &[u8], address: &SocketAddr) -> std::io::Result<()> {
        self.serverbound_tx
            .send(Message::Binary(payload.to_vec()))
            .map_err(|e| {
                std::io::Error::other(format!("unable to send message to server: {:?}", e))
            })
    }
}

struct WebSocketClientSocketReceiver {
    buffer: [u8; MTU],
    server_addr: SocketAddr,
    clientbound_rx: UnboundedReceiver<Vec<u8>>,
}

impl PacketReceiver for WebSocketClientSocketReceiver {
    fn recv(&mut self) -> std::io::Result<Option<(&mut [u8], SocketAddr)>> {
        match self.clientbound_rx.try_recv() {
            Ok(msg) => match msg {
                Message::Binary(buf) => {
                    self.buffer[..buf.len()].copy_from_slice(&buf);
                    Ok(Some((&mut self.buffer[..buf.len()], self.server_addr)))
                }
                Message::Close(frame) => {
                    info!("WebSocket connection closed (Frame: {:?})", frame);
                    Ok(None)
                }
                _ => Ok(None),
            },
            Err(e) => {
                if e == TryRecvError::Empty {
                    Ok(None)
                } else {
                    Err(std::io::Error::other(format!(
                        "unable to receive message from client: {}",
                        e
                    )))
                }
            }
        }
    }
}
