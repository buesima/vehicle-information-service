// SPDX-License-Identifier: MIT

use crate::{Action, ActionErrorResponse, ActionSuccessResponse, Client, ClientAction};
use futures::{
    channel::mpsc::{channel, Receiver, Sender},
    future, pin_mut,
    stream::{SplitSink, SplitStream},
    StreamExt,
};
use futures_util::sink::SinkExt;
use log::*;
use serde_json::from_str;
use std::{collections::HashMap, env, net::SocketAddr};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tungstenite::Message;

const MAX_CLIENT_SEND_BUFFER: usize = 1024;

pub struct WebsocketServer;

impl WebsocketServer {
    /// Handle an incoming TCP connection
    async fn handle_connection(
        mut tx_client_action: Sender<ClientAction>,
        raw_stream: TcpStream,
        socket_addr: SocketAddr,
    ) {
        info!("Incoming TCP connection from: {}", socket_addr);

        let ws_stream = tokio_tungstenite::accept_async(raw_stream)
            .await
            .expect("Error during the websocket handshake occurred");
        info!("WebSocket connection established: {}", socket_addr);

        let (to_client_tx, to_client_rx) = channel(MAX_CLIENT_SEND_BUFFER);
        let (outgoing, incoming) = ws_stream.split();

        let client = Client {
            socket_addr: socket_addr.clone(),
            to_client_tx: to_client_tx.clone(), // Must be cloned, the channel is closed otherwise
            subscriptions: HashMap::new(),
        };

        tx_client_action
            .send(ClientAction::Connected {
                socket_addr,
                client,
            })
            .await
            .unwrap();

        let forward_incoming =
            Self::handle_incoming_client_messages(&socket_addr, incoming, tx_client_action.clone());
        let forward_outgoing = Self::handle_outgoing_client_messages(to_client_rx, outgoing);

        pin_mut!(forward_incoming, forward_outgoing);
        future::select(forward_incoming, forward_outgoing).await;

        tx_client_action
            .send(ClientAction::Disconnected { socket_addr })
            .await
            .unwrap();
        info!("Client disconnected, socket_addr: {}", &socket_addr);
    }

    /// Serializes outgoing client messages.
    async fn handle_outgoing_client_messages(
        mut to_client_rx: Receiver<Result<ActionSuccessResponse, ActionErrorResponse>>,
        mut outgoing: SplitSink<WebSocketStream<tokio::net::TcpStream>, Message>,
    ) {
        while let Some(response) = to_client_rx.next().await {
            let txt = match response {
                Ok(success) => {
                    debug!("Respond to client with: {:#?}", success);
                    serde_json::to_string(&success).unwrap()
                }
                Err(error) => {
                    warn!("Responding with error to client: {}", error);
                    serde_json::to_string(&error).unwrap()
                }
            };
            let msg = Message::Text(txt);
            outgoing.send(msg).await.unwrap();
        }
    }

    /// Deserializes incoming client messages.
    async fn handle_incoming_client_messages(
        socket_addr: &SocketAddr,
        mut incoming: SplitStream<WebSocketStream<tokio::net::TcpStream>>,
        mut tx_client_action: Sender<ClientAction>,
    ) -> Result<(), tungstenite::Error> {
        while let Some(Ok(msg)) = incoming.next().await {
            debug!(
                "Received a message from {}: {}",
                socket_addr,
                msg.to_text().unwrap()
            );

            match msg {
                Message::Ping(msg) => {
                    debug!("Responding to `Ping` message: {:?} with Pong", msg);
                }
                Message::Pong(_) => {}
                Message::Binary(_bin) => {
                    warn!("Binary message payload. This message will be ignored.");
                }
                Message::Close(close_reason) => {
                    info!(
                        "Client {} sent Close message, reason: {:?}",
                        socket_addr, close_reason
                    );
                }
                Message::Text(ref txt) => {
                    if let Ok(action) = from_str::<Action>(txt) {
                        debug!("VIS Action: {}", action);
                        tx_client_action
                            .send(ClientAction::Message {
                                socket_addr: socket_addr.clone(),
                                action,
                            })
                            .await
                            .unwrap();
                    } else {
                        warn!("Failed to parse received message: {:#?}", txt);
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn run(tx_client_action: Sender<ClientAction>) {
        let addr = env::args()
            .nth(1)
            .unwrap_or_else(|| "127.0.0.1:8080".to_string());

        info!("Listening on: {}", addr);

        // Create the event loop and TCP listener we'll accept connections on.
        let try_socket = TcpListener::bind(&addr).await;
        let mut listener = try_socket.expect("Failed to bind");

        // Let's spawn the handling of each connection in a separate task.
        while let Ok((stream, addr)) = listener.accept().await {
            tokio::spawn(Self::handle_connection(
                tx_client_action.clone(),
                stream,
                addr,
            ));
        }
    }
}
