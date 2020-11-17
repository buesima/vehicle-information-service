// SPDX-License-Identifier: MIT

use actix::prelude::*;
use actix_web::{web, HttpRequest};
use actix_web_actors::ws;

use futures::prelude::*;
use http::status::StatusCode;
use serde_json::{from_str, json, to_string};
use uuid::Uuid;

use crate::action;
use crate::api_error::*;
use crate::api_type::*;
use crate::serialize_result;
use crate::signal_manager::{SignalManager, UpdateSignal};

pub struct ClientSession {
    /// Each client is assigned a unique identifier after connecting.
    /// This identifier can be used to identify the client in the logs.
    client_connection_id: ClientConnectionId,

    signal_manager_addr: Addr<SignalManager>,
}

impl ClientSession {
    pub fn new(signal_manager_addr: Addr<SignalManager>) -> Self {
        Self {
            client_connection_id: Uuid::new_v4(),
            signal_manager_addr,
        }
    }
}

impl Actor for ClientSession {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, _ctx: &mut Self::Context) {
        info!("Client {} started", self.client_connection_id);
    }

    fn stopped(&mut self, ctx: &mut Self::Context) {
        // Cleanup client subscriptions
        self.signal_manager_addr.do_send(action::ClientMessage {
            client_connection_id: self.client_connection_id,
            client_addr: ctx.address(),
            message: action::UnsubscribeAll { request_id: None },
        });

        info!("Client {} stopped", self.client_connection_id);
    }
}

impl Message for ActionSuccessResponse {
    type Result = ();
}

impl Message for ActionErrorResponse {
    type Result = ();
}

impl Handler<ActionSuccessResponse> for ClientSession {
    type Result = ();
    fn handle(&mut self, msg: ActionSuccessResponse, ctx: &mut Self::Context) {
        // TODO replace subscribe error with subscription error
        let serialized = serialize_result(&Ok(msg), || {
            new_subscribe_error(ReqID::ReqIDInt(0), StatusCode::INTERNAL_SERVER_ERROR.into())
        });
        ctx.text(serialized)
    }
}

impl Handler<ActionErrorResponse> for ClientSession {
    type Result = ();
    fn handle(&mut self, msg: ActionErrorResponse, ctx: &mut Self::Context) {
        // TODO replace subscribe error with subscription error
        let serialized = serialize_result(&Err(msg), || {
            new_subscribe_error(ReqID::ReqIDInt(0), StatusCode::INTERNAL_SERVER_ERROR.into())
        });
        ctx.text(serialized)
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for ClientSession {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        debug!("WS: {:?}", msg);
        match msg {
            Ok(ws::Message::Ping(msg)) => {
                debug!("Responding to `Ping` message: {:?} with Pong", msg);
                ctx.pong(&msg);
            }
            Ok(ws::Message::Pong(_)) => {}
            Ok(ws::Message::Binary(_bin)) => {
                warn!("Binary message payload. This message will be ignored.");
            }
            Ok(ws::Message::Text(ref txt)) => {
                // deserialize and dispatch VIS action

                match from_str::<Action>(txt) {
                    Err(e) => {
                        warn!("Deserialization error {}", e);
                        let err = new_deserialization_error();
                        if let Ok(serialized) = to_string(&err) {
                            ctx.text(serialized);
                        }
                    }
                    Ok(action) => {
                        debug!(
                            "Received action {:?} for client connection_id {}",
                            action, self.client_connection_id
                        );
                        match action {
                            Action::Subscribe {
                                path,
                                request_id,
                                filters,
                            } => {
                                self.signal_manager_addr.do_send(action::ClientMessage {
                                    client_connection_id: self.client_connection_id,
                                    client_addr: ctx.address(),
                                    message: action::Subscribe {
                                        path,
                                        request_id,
                                        filters,
                                    },
                                });
                            }
                            Action::Unsubscribe {
                                request_id,
                                subscription_id,
                            } => {
                                self.signal_manager_addr.do_send(action::ClientMessage {
                                    client_connection_id: self.client_connection_id,
                                    client_addr: ctx.address(),
                                    message: action::Unsubscribe {
                                        request_id,
                                        subscription_id,
                                    },
                                });
                            }
                            Action::Get { path, request_id } => {
                                self.signal_manager_addr.do_send(action::ClientMessage {
                                    client_connection_id: self.client_connection_id,
                                    client_addr: ctx.address(),
                                    message: action::Get { request_id, path },
                                });
                            }
                            Action::UnsubscribeAll { request_id } => {
                                self.signal_manager_addr.do_send(action::ClientMessage {
                                    client_connection_id: self.client_connection_id,
                                    client_addr: ctx.address(),
                                    message: action::UnsubscribeAll {
                                        request_id: Some(request_id),
                                    },
                                });
                            }
                            Action::Set {
                                request_id,
                                path,
                                value,
                            } => {
                                self.signal_manager_addr.do_send(action::ClientMessage {
                                    client_connection_id: self.client_connection_id,
                                    client_addr: ctx.address(),
                                    message: action::Set {
                                        request_id,
                                        path,
                                        value,
                                    },
                                });
                            }
                            // TODO implement
                            Action::Authorize { request_id, .. } => {
                                if let Ok(serialized) = to_string(&new_authorize_error(
                                    request_id,
                                    StatusCode::NOT_IMPLEMENTED.into(),
                                )) {
                                    ctx.text(serialized)
                                }
                            }
                            // TODO implement
                            Action::GetMetadata { request_id, .. } => {
                                if let Ok(serialized) = to_string(&new_get_metadata_error(
                                    request_id,
                                    StatusCode::NOT_IMPLEMENTED.into(),
                                )) {
                                    ctx.text(serialized)
                                }
                            }
                        }
                    }
                };
            }
            Ok(ws::Message::Close(close_reason)) => {
                info!(
                    "Client {} sent Close message, reason: {:?}",
                    self.client_connection_id, close_reason
                );
                ctx.stop();
            }
            Ok(ws::Message::Nop) => ctx.stop(),
            Ok(ws::Message::Continuation(_)) => ctx.stop(),
            Err(err) => {
                error!("Message error: {}", err);
                ctx.stop();
            }
        }
    }
}

pub struct AppState {
    signal_manager_addr: Addr<SignalManager>,
}

impl AppState {
    pub fn signal_manager_addr(&self) -> Addr<SignalManager> {
        self.signal_manager_addr.clone()
    }

    /// Set the path to the given value.
    pub fn set_signal<T>(&self, path: ActionPath, value: T)
    where
        T: serde::ser::Serialize,
    {
        self.signal_manager_addr.do_send(UpdateSignal {
            path,
            value: json!(value),
        });
    }

    /// Register a `set` action recipient. This recipient will receive all `set` action requests for all clients.
    pub fn add_set_recipient(&self, path: ActionPath, recipient: Recipient<action::Set>) {
        self.signal_manager_addr
            .do_send(action::AddSetRecipient { path, recipient });
    }

    /// Spawn a new signal stream source. A signal stream will provide signal updates for the given path.
    pub fn spawn_stream_signal_source<St>(&self, path: ActionPath, s: St)
    where
        St: TryStream + Unpin,
        St: 'static,
        St::Ok: serde::Serialize,
        St::Error: std::fmt::Debug,
    {
        let signal_manager_addr = self.signal_manager_addr.clone();

        let stream_signal_source = s
            .map_err(|e| warn!("Signal source stream error: {:?}", e))
            .for_each(move |item| {
                let update = UpdateSignal {
                    path: ActionPath(path.to_string()),
                    value: json!(item),
                };
                signal_manager_addr.do_send(update);

                futures::future::ready(())
            });

        actix::spawn(stream_signal_source);
    }

    /// Spawn a new signal stream source. A signal stream will provide signal updates for the given path.
    pub fn spawn_stream_signal_source2<T, St>(&self, s: St)
    where
        T: serde::Serialize,
        St: TryStream + Unpin,
        St: 'static,
        St::Ok: ActionSourceable,
        St::Error: std::fmt::Debug,
    {
        let signal_manager_addr = self.signal_manager_addr.clone();

        let stream_signal_source = s
            .map_err(|e| warn!("Signal source stream error: {:?}", e))
            .for_each(move |items| {
                if let Ok(items) = items {
                    for item in items.source() {
                        let (path, value) = item;
                        let update = UpdateSignal { path, value };
                        signal_manager_addr.do_send(update);
                    }
                }
                futures::future::ready(())
            });

        actix::spawn(stream_signal_source);
    }
}

pub trait ActionSourceable {
    fn source(self) -> Vec<(ActionPath, serde_json::Value)>;
}

impl ActionSourceable for (ActionPath, serde_json::Value) {
    fn source(self) -> Vec<(ActionPath, serde_json::Value)> {
        vec![self]
    }
}

impl ActionSourceable for Vec<(ActionPath, serde_json::Value)> {
    fn source(self) -> Vec<(ActionPath, serde_json::Value)> {
        self
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            signal_manager_addr: SignalManager::start_default(),
        }
    }
}

pub struct Router {}

async fn ws_index(
    state: web::Data<AppState>,
    r: HttpRequest,
    stream: web::Payload,
) -> Result<actix_web::HttpResponse, actix_web::Error> {
    let addr = state.signal_manager_addr.clone();
    ws::start(ClientSession::new(addr), &r, stream)
}

impl Router {
    /// Create a new instance of a Router
    pub fn configure_routes(cfg: &mut web::ServiceConfig) {
        cfg.service(web::resource("/").route(web::get().to(ws_index)));
    }
}
