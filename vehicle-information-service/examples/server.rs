// SPDX-License-Identifier: MIT

//!
//! This shows a simple server providing two retrievable signals (via `get` or
//! `subscribe`), as well as one settable signal.
//! For a better understanding of the VIS specification make sure you read the specification `https://w3c.github.io/automotive/vehicle_data/vehicle_information_service.html`.
use futures::{channel::mpsc::Sender, select};
use futures_util::{
    compat::Stream01CompatExt, future::FutureExt, sink::SinkExt, stream::StreamExt,
};
use log::*;
use serde_json::{json, Value};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use structopt::StructOpt;
use tokio::time;
use tokio_socketcan;
use vehicle_information_service::{ActionPath, VisServer};

const PATH_PRIVATE_EXAMPLE_PRINT_SET: &str = "Private.Example.Print.Set";
const PATH_CURRENT_UNIX_TIMESTAMP: &str = "Private.Example.Timestamp";
const PATH_PRIVATE_EXAMPLE_SOCKETCAN_LAST_FRAME_ID: &str =
    "Private.Example.SocketCan.Last.Frame.Id";

#[derive(StructOpt, Debug, Clone)]
#[structopt(name = "Vehicle Information Service Demo")]
struct Opt {
    #[structopt(
        short = "c",
        long = "can",
        default_value = "vcan0",
        help = "CAN Interface"
    )]
    can_interface: String,

    #[structopt(
        short = "p",
        long = "port",
        default_value = "14430",
        help = "Websocket Port"
    )]
    port: u16,
}

/// Build with `cargo run --example server --can vcan0 --port 14430`
/// Connect with websocket client using e.g. wscat
/// ```
/// wscat -c "localhost:14430"
/// { "action": "Subscribe", "path": "Private.Example.Timestamp", "requestId": 104 }
/// ```
#[tokio::main]
async fn main() -> Result<(), io::Error> {
    env_logger::init();

    let opt = Opt::from_args();

    let socket_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), opt.port);

    info!("Starting server");

    let mut vis_server = VisServer::new(socket_addr);
    let signal_tx = vis_server.signal_tx();

    select! {
        _ = vis_server.run().fuse() => panic!("Vis Server stopped"),
        _ = signal_current_time(signal_tx.clone()).fuse() => panic!("Interval signal stopped"),
        _ = signal_last_frame_id(&opt.can_interface, signal_tx.clone()).fuse() => panic!("CAN signal stopped"),
    }
}

async fn signal_last_frame_id(can_interface: &str, mut signal_tx: Sender<(ActionPath, Value)>) {
    let mut can_stream = tokio_socketcan::CANSocket::open(can_interface)
        .unwrap()
        .compat();

    while let Some(Ok(frame)) = can_stream.next().await {
        let signal = (
            PATH_PRIVATE_EXAMPLE_SOCKETCAN_LAST_FRAME_ID.into(),
            json!(frame.id()),
        );
        signal_tx.send(signal).await.unwrap();
    }
}

async fn signal_current_time(mut signal_tx: Sender<(ActionPath, Value)>) {
    let mut interval = time::interval(Duration::from_secs(1));
    while let _ = interval.tick().await {
        let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        signal_tx
            .send((PATH_CURRENT_UNIX_TIMESTAMP.into(), json!(time.as_secs())))
            .await
            .unwrap();
    }
}

// This `set` recipient will handle incoming SET requests
// and print the incoming result. Implement `Handler<Set>` in your actors
// to deal with incoming `set` requests.
//
// Register your `set` recipient to a specified path like this:
//
// ```
// use vehicle_information_service::{KnownError, Router, Set};
//
// let app = Router::start();
// app.state().add_set_recipient(
//     PATH_PRIVATE_EXAMPLE_PRINT_SET.into(),
//     example_set.recipient().clone(),
// );
//```
// #[derive(Default)]
// struct PrintSetRecipient {}

// impl Actor for PrintSetRecipient {
//     type Context = Context<Self>;

//     fn started(&mut self, _ctx: &mut Context<Self>) {
//         info!(
//             "Print `set`-recipient started, PATH: {}",
//             PATH_PRIVATE_EXAMPLE_PRINT_SET
//         );
//     }

//     fn stopped(&mut self, _ctx: &mut Context<Self>) {
//         info!(
//             "Print `set`-recipient stopped, PATH: {}",
//             PATH_PRIVATE_EXAMPLE_PRINT_SET
//         );
//     }
// }

// impl Handler<Set> for PrintSetRecipient {
//     type Result = Result<(), KnownError>;

//     fn handle(&mut self, msg: Set, _ctx: &mut Context<Self>) -> Result<(),
// KnownError> {         info!("Received SET for path `{}`, value: {}",
// msg.path, msg.value);         Ok(())
//     }
// }
