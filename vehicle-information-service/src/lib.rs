// SPDX-License-Identifier: MIT

#![recursion_limit = "256"]

mod api_error;
mod api_type;
mod filter;
mod signal_server;
mod websocket_server;

use futures::{channel::mpsc::Sender, select};
use futures_util::future::FutureExt;
use signal_server::{ClientAction, SignalServer};
use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use websocket_server::WebsocketServer;

pub use api_error::ActionErrorResponse;
pub use api_type::{Action, ActionPath, ActionSuccessResponse};
pub use serde_json::{json, Value};

pub(crate) fn unix_timestamp() -> Option<Duration> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok()
}

pub fn unix_timestamp_ms() -> u128 {
    unix_timestamp().map(|t| t.as_millis()).unwrap_or_default()
}

pub struct VisServer {
    socket_addr: SocketAddr,
    signal_server: SignalServer,
}

impl VisServer {
    pub fn new(socket_addr: SocketAddr) -> Self {
        Self {
            signal_server: SignalServer::new(),
            socket_addr,
        }
    }

    pub fn signal_tx(&self) -> Sender<(ActionPath, Value)> {
        self.signal_server.signals_tx.clone()
    }

    pub async fn run(&mut self) {
        let tx_client = self.signal_server.clients_tx.clone();

        select! {
            _ = self.signal_server.run().fuse() => panic!("Signal server stopped"),
            _ = WebsocketServer::run(&self.socket_addr, tx_client).fuse() => panic!("Websocket server stopped"),
        }
    }
}
