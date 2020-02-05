// SPDX-License-Identifier: MIT

use crate::{
    api_error::{ActionErrorResponse, *},
    api_type::{Action, ActionPath, ActionSuccessResponse, Filters, ReqID, SubscriptionID},
    filter, unix_timestamp_ms,
};
use futures::{
    channel::mpsc::{channel, Receiver, Sender},
    select,
};
use futures_util::{future::FutureExt, sink::SinkExt, stream::StreamExt};
use http::status::StatusCode;
use log::*;
use serde_json::Value;
use std::{collections::HashMap, fmt, net::SocketAddr, time::SystemTime};
use uuid::Uuid;

const MAX_CLIENT_BUFFER: usize = 1024;

/// A client subscription.
#[derive(Debug)]
pub struct Subscription {
    /// A random subscriptionId that is generated when creating a subscription.
    /// This is also passed when sending SubscriptionNotifications.
    subscription_id: SubscriptionID,

    /// Signal path
    path: ActionPath,

    /// Filters e.g. minChange requested by client when subscribing
    filters: Option<Filters>,

    /// Latest known signal value, this may not have been sent to the client yet
    /// if the filter did not match or if this an interval based subscription.
    pub latest_signal_value: Option<Value>,

    /// Last value send to client via SubscriptionNotification, contains
    /// timestamp when last value was sent
    pub last_signal_value_client: Option<(SystemTime, Value)>,

    /// Send a response to the client
    to_client_tx: Sender<Result<ActionSuccessResponse, ActionErrorResponse>>,
}

impl fmt::Display for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Subscription path: {}", self.path,)
    }
}

#[derive(Debug)]
pub struct Client {
    pub socket_addr: SocketAddr,
    pub to_client_tx: Sender<Result<ActionSuccessResponse, ActionErrorResponse>>,
    pub subscriptions: HashMap<SubscriptionID, ActionPath>,
}

impl fmt::Display for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.socket_addr)
    }
}

#[derive(Debug)]
pub enum ClientAction {
    Connected {
        socket_addr: SocketAddr,
        client: Client,
    },
    Disconnected {
        socket_addr: SocketAddr,
    },
    Message {
        socket_addr: SocketAddr,
        action: Action,
    },
}

pub struct SignalServer {
    /// Report a client action e.g. disconnect, connect or new message
    pub clients_tx: Sender<ClientAction>,
    clients_rx: Receiver<ClientAction>,

    /// Update a path with a new value
    pub signals_tx: Sender<(ActionPath, Value)>,
    signals_rx: Receiver<(ActionPath, Value)>,

    /// Currently connected clients
    clients: HashMap<SocketAddr, Client>,

    /// Current signal state e.g. Vehicle Speed
    signal_cache: HashMap<ActionPath, Value>,

    /// List of subscribers to update once a signal is updated
    subscriptions: HashMap<ActionPath, Vec<Subscription>>,
}

impl SignalServer {
    pub fn new() -> Self {
        let (clients_tx, clients_rx) = channel(MAX_CLIENT_BUFFER);
        let (signals_tx, signals_rx) = channel(MAX_CLIENT_BUFFER);

        Self {
            clients_tx,
            clients_rx,
            signals_tx,
            signals_rx,
            clients: HashMap::new(),
            signal_cache: HashMap::new(),
            subscriptions: HashMap::new(),
        }
    }

    ///
    /// Remove a client subscription from the server subscription list.
    /// Client will no longer be notified afterwards.
    fn remove_server_subscription(
        subscriptions: &mut HashMap<ActionPath, Vec<Subscription>>,
        subscription_id: &SubscriptionID,
        path: &ActionPath,
    ) {
        if let Some(subs) = subscriptions.get_mut(&path) {
            subs.retain(|x| x.subscription_id != *subscription_id);
            debug!(
                "Removed subscription {} for path: {}, from SignalServer subscriptions",
                subscription_id, path
            );
        } else {
            error!(
                "Subscription {} not in SignalServer subscription list",
                subscription_id
            );
        }
    }

    ///
    /// Responde to a client's `Get` request retrieving the current signal state
    /// from the signal cache.
    fn handle_get(
        &self,
        path: ActionPath,
        request_id: ReqID,
    ) -> Result<ActionSuccessResponse, ActionErrorResponse> {
        if let Some(signal) = self.signal_cache.get(&path) {
            Ok(ActionSuccessResponse::Get {
                request_id: request_id,
                value: signal.clone(),
                timestamp: unix_timestamp_ms(),
            })
        } else {
            Err(ActionErrorResponse::Get {
                request_id: request_id,
                timestamp: unix_timestamp_ms(),
                error: NOT_FOUND_INVALID_PATH.into(),
            })
        }
    }

    ///
    /// Respond to a client's `Subscribe` request.
    /// The client will be notified of updates for the specified path
    /// afterwards.
    fn handle_subscribe(
        client: &mut Client,
        subscriptions: &mut HashMap<ActionPath, Vec<Subscription>>,
        path: ActionPath,
        request_id: ReqID,
        filters: Option<Filters>,
    ) -> Result<ActionSuccessResponse, ActionErrorResponse> {
        let subscription_id = SubscriptionID::SubscriptionIDUUID(Uuid::new_v4());
        debug!(
            "Adding subscriber with id {} to path: {}",
            subscription_id, &path
        );

        let subscription = Subscription {
            subscription_id,
            path: path.clone(),
            filters,
            latest_signal_value: None,
            last_signal_value_client: None,
            to_client_tx: client.to_client_tx.clone(),
            // interval_handle: None,
        };

        if let Some(path_subscriptions) = subscriptions.get_mut(&path) {
            path_subscriptions.push(subscription);
        } else {
            subscriptions.insert(path.clone(), vec![subscription]);
        }

        client.subscriptions.insert(subscription_id, path);

        let response = ActionSuccessResponse::Subscribe {
            request_id: request_id,
            subscription_id,
            timestamp: unix_timestamp_ms(),
        };

        Ok(response)
    }

    ///
    /// Remove all client subscriptions.
    /// The client will no longer be notified for any of his current
    /// subscriptions.
    fn handle_unsubscribe_all(
        client: &mut Client,
        subscriptions: &mut HashMap<ActionPath, Vec<Subscription>>,
        request_id: ReqID,
    ) -> Result<ActionSuccessResponse, ActionErrorResponse> {
        let len = client.subscriptions.len();

        for (sub_id, path) in client.subscriptions.iter() {
            Self::remove_server_subscription(subscriptions, sub_id, path);
        }

        client.subscriptions.clear();
        debug!("Cleared client {}, {} subscriptions", client, len);

        Ok(ActionSuccessResponse::UnsubscribeAll {
            request_id: request_id,
            timestamp: unix_timestamp_ms(),
        })
    }

    ///
    /// Remove a specified subscription.
    /// The client will no longer be notified for this subscription.
    fn handle_unsubscribe(
        client: &mut Client,
        subscriptions: &mut HashMap<ActionPath, Vec<Subscription>>,
        request_id: ReqID,
        subscription_id: SubscriptionID,
    ) -> Result<ActionSuccessResponse, ActionErrorResponse> {
        if let Some(path) = client.subscriptions.remove(&subscription_id) {
            Self::remove_server_subscription(subscriptions, &subscription_id, &path);
            debug!(
                "Removed client {} subscription: {}",
                client, subscription_id
            );
            Ok(ActionSuccessResponse::Unsubscribe {
                request_id: request_id,
                subscription_id,
                timestamp: unix_timestamp_ms(),
            })
        } else {
            warn!(
                "Client attempted to remove subscription {} not belonging to client",
                subscription_id
            );
            Err(ActionErrorResponse::Unsubscribe {
                request_id: request_id,
                subscription_id: subscription_id,
                timestamp: unix_timestamp_ms(),
                error: NOT_FOUND_INVALID_SUBSCRIPTION_ID.into(),
            })
        }
    }

    ///
    /// Respond to a request sent from a client.
    async fn dispatch_client_message(&mut self, socket_addr: SocketAddr, action: Action) {
        let subscriptions = &mut self.subscriptions;
        if let Some(mut client) = self.clients.get_mut(&socket_addr) {
            let mut to_client_tx = client.to_client_tx.clone();

            let response = match action {
                Action::Get { request_id, path } => self.handle_get(path, request_id),
                Action::Subscribe {
                    path,
                    request_id,
                    filters,
                } => Self::handle_subscribe(&mut client, subscriptions, path, request_id, filters),
                Action::UnsubscribeAll { request_id } => {
                    Self::handle_unsubscribe_all(&mut client, subscriptions, request_id)
                }
                Action::Unsubscribe {
                    request_id,
                    subscription_id,
                } => Self::handle_unsubscribe(
                    &mut client,
                    subscriptions,
                    request_id,
                    subscription_id,
                ),
                Action::Set {
                    path: _path,
                    value: _value,
                    request_id,
                } => {
                    warn!("SET action has not yet been implemented");
                    Err(new_set_error(
                        request_id,
                        StatusCode::INTERNAL_SERVER_ERROR.into(),
                    ))
                }
                Action::GetMetadata {
                    path: _path,
                    request_id,
                } => {
                    warn!("GET_METADATA action has not yet been implemented");
                    Err(new_get_metadata_error(
                        request_id,
                        StatusCode::INTERNAL_SERVER_ERROR.into(),
                    ))
                }
                Action::Authorize {
                    tokens: _tokens,
                    request_id,
                } => {
                    warn!("AUTHORIZE action has not yet been implemented");
                    Err(new_authorize_error(
                        request_id,
                        StatusCode::INTERNAL_SERVER_ERROR.into(),
                    ))
                }
            };

            to_client_tx.send(response).await.unwrap();
        } else {
            error!(
                "Unknown client can not respond, socket_addr: {}",
                socket_addr
            );
        }
    }

    ///
    /// Process a client action e.g. connect or disconnect and handle an
    /// incoming client message.
    async fn process_client_requests(&mut self, client_action: ClientAction) {
        info!("Starting client request processor");
        trace!("Processing client action: {:#?}", client_action);
        match client_action {
            ClientAction::Connected {
                socket_addr,
                client,
            } => {
                self.clients.insert(socket_addr, client);
                debug!(
                    "Inserted connected client: {}, # connected clients: {}",
                    socket_addr,
                    self.clients.len()
                );
            }
            ClientAction::Disconnected { socket_addr } => {
                self.clients.remove(&socket_addr);
                debug!(
                    "Removed disconnected client: {}, # connected clients: {}",
                    socket_addr,
                    self.clients.len()
                );
            }
            ClientAction::Message {
                socket_addr,
                action,
            } => {
                self.dispatch_client_message(socket_addr, action).await;
            }
        }
    }

    ///
    /// Process incoming signals, storing and distributing them to clients.
    /// Dispatch incoming client requests to be handled.
    pub async fn run(&mut self) {
        loop {
            select! {
                signal = self.signals_rx.next().fuse() => {
                    if let Some((path, value)) = signal {
                        self.signal_cache.insert(path.clone(), value.clone());

                        // Notify all path subscribers
                        if let Some(mut client_subscriptions) = self.subscriptions.get_mut(&path) {
                            for sub in client_subscriptions {

                                let filter_result = filter::matches(&value, &sub.last_signal_value_client, &sub.filters);
                                match filter_result {
                                    // Filter matches
                                    Ok(true) => {
                                        debug!(
                                            "Notifiying SubscriptionId {} of value change",
                                            sub.subscription_id
                                        );
                                        let response = ActionSuccessResponse::Subscription {
                                            subscription_id: sub.subscription_id,
                                            timestamp: unix_timestamp_ms(),
                                            value: value.clone(),
                                        };
                                        sub.last_signal_value_client = Some((SystemTime::now(), value.clone()));
                                        sub.to_client_tx.send(Ok(response)).await.unwrap();
                                    },
                                    // Filter criteria does not match
                                    Ok(false) => {
                                        debug!(
                                            "Update does not match filter for SubscriptionId {}",
                                            sub.subscription_id
                                        );
                                    },
                                    Err(filter::Error::ValueIsNotANumber) => {
                                        let response = new_subscription_notification_error(sub.subscription_id, BAD_REQUEST_FILTER_INVALID.into());
                                        sub.to_client_tx.send(Err(response)).await.unwrap();
                                    },
                                }
                            }
                        }
                    }
                },
                client_action = self.clients_rx.next().fuse() => {
                    if let Some(client_action) = client_action {
                        self.process_client_requests(client_action).await;
                    }
                },
            };
        }
    }
}
