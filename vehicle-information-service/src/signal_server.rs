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
use std::{
    collections::HashMap,
    fmt,
    net::SocketAddr,
    time::{Duration, SystemTime},
};
use tokio::task::JoinHandle;
use uuid::Uuid;

const MAX_CLIENT_BUFFER: usize = 1024;

/// A client subscription.
#[derive(Debug, Clone)]
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
        write!(f, "Subscription path: {}", self.path)
    }
}

#[derive(Debug)]
pub struct Client {
    socket_addr: SocketAddr,
    to_client_tx: Sender<Result<ActionSuccessResponse, ActionErrorResponse>>,

    pub message_tx: Sender<Action>,
    message_rx: Receiver<Action>,

    subscriptions: HashMap<SubscriptionID, Subscription>,

    subscription_tx: Sender<(SubscriptionID, Value)>,
    subscription_rx: Receiver<(SubscriptionID, Value)>,

    /// Update a path with a new value
    pub signals_tx: Sender<(ActionPath, Value)>,
    signals_rx: Receiver<(ActionPath, Value)>,

    /// Current signal state e.g. Vehicle Speed
    signal_cache: HashMap<ActionPath, Value>,

    path_subscriptions: HashMap<ActionPath, Vec<SubscriptionID>>,

    periodic_subscription_tx: Sender<SubscriptionID>,
    periodic_subscription_rx: Receiver<SubscriptionID>,

    /// List of subscribers to update once a signal is updated
    // path_subscriptions: HashMap<ActionPath, Vec<Subscription>>,
    periodic_subscription_handles: HashMap<SubscriptionID, JoinHandle<()>>,
}

impl Client {
    fn new(
        socket_addr: SocketAddr,
        to_client_tx: Sender<Result<ActionSuccessResponse, ActionErrorResponse>>,
    ) -> Self {
        let (signals_tx, signals_rx) = channel(MAX_CLIENT_BUFFER);
        let (subscription_tx, subscription_rx) = channel(MAX_CLIENT_BUFFER);
        let (periodic_subscription_tx, periodic_subscription_rx) = channel(MAX_CLIENT_BUFFER);
        let (message_tx, message_rx) = channel(MAX_CLIENT_BUFFER);

        Self {
            socket_addr,
            to_client_tx,
            message_tx,
            message_rx,
            subscriptions: HashMap::new(),
            subscription_tx,
            subscription_rx,
            signals_tx,
            signals_rx,
            signal_cache: HashMap::new(),
            path_subscriptions: HashMap::new(),
            periodic_subscription_tx,
            periodic_subscription_rx,
            periodic_subscription_handles: HashMap::new(),
        }
    }

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

    async fn periodic_notify_stream(
        mut periodic_subscriptions_tx: Sender<SubscriptionID>,
        interval_secs: u64,
        subscription_id: SubscriptionID,
    ) {
        let mut notify_stream = tokio::time::interval(Duration::from_secs(interval_secs));
        while let _ = notify_stream.tick().await {
            periodic_subscriptions_tx
                .send(subscription_id)
                .await
                .unwrap();
        }
    }

    /// Respond to a client's `Subscribe` request.
    /// The client will be notified of updates for the specified path
    /// afterwards.
    fn handle_subscribe(
        &mut self,
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
            filters: filters.clone(),
            latest_signal_value: None,
            last_signal_value_client: None,
            to_client_tx: self.to_client_tx.clone(),
        };

        let mut is_periodic_subscription = false;
        if let Some(ref filters) = filters {
            if let Some(interval_secs) = filters.interval {
                // Start a stream that will periodically retrieve the signal from the
                // signal cache and send the value to the client
                let periodic_subscriptions_tx = self.periodic_subscription_tx.clone();
                let join_handle = tokio::spawn(Self::periodic_notify_stream(
                    periodic_subscriptions_tx,
                    interval_secs,
                    subscription_id.clone(),
                ));

                self.periodic_subscription_handles
                    .insert(subscription_id.clone(), join_handle);

                debug!(
                    "Spawned periodic subscription:{} to path: {}",
                    subscription_id, &path
                );
                is_periodic_subscription = true;
            }
        };

        if !is_periodic_subscription {
            if let Some(path_subscriptions) = self.path_subscriptions.get_mut(&path) {
                path_subscriptions.push(subscription.subscription_id.clone());
            } else {
                self.path_subscriptions
                    .insert(path.clone(), vec![subscription.subscription_id.clone()]);
            }
        }

        self.subscriptions.insert(subscription_id, subscription);

        let response = ActionSuccessResponse::Subscribe {
            request_id: request_id,
            subscription_id,
            timestamp: unix_timestamp_ms(),
        };

        Ok(response)
    }

    /// Remove all client subscriptions.
    /// The client will no longer be notified for any of his current
    /// subscriptions.
    fn handle_unsubscribe_all(
        &mut self,
        request_id: ReqID,
    ) -> Result<ActionSuccessResponse, ActionErrorResponse> {
        let len = self.subscriptions.len();

        self.path_subscriptions.clear();
        self.periodic_subscription_handles.clear();
        self.subscriptions.clear();
        debug!("Cleared client {}, {} subscriptions", self, len);

        Ok(ActionSuccessResponse::UnsubscribeAll {
            request_id: request_id,
            timestamp: unix_timestamp_ms(),
        })
    }

    /// Remove a specified subscription.
    /// The client will no longer be notified for this subscription.
    fn handle_unsubscribe(
        &mut self,
        request_id: ReqID,
        subscription_id: SubscriptionID,
    ) -> Result<ActionSuccessResponse, ActionErrorResponse> {
        if let Some(subscription) = self.subscriptions.remove(&subscription_id) {
            if let Some(subs) = self.path_subscriptions.get_mut(&subscription.path) {
                subs.retain(|x| *x != subscription_id);
                debug!(
                    "Removed subscription {} for path: {}, from SignalServer subscriptions",
                    subscription_id, subscription.path
                );
            }

            self.periodic_subscription_handles.remove(&subscription_id);
            debug!("Removed client {} subscription: {}", self, subscription_id);
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

    /// Respond to a request sent from a client.
    async fn dispatch_message(&mut self, action: Action) {
        let response = match action {
            Action::Get { request_id, path } => self.handle_get(path, request_id),
            Action::Subscribe {
                path,
                request_id,
                filters,
            } => self.handle_subscribe(path, request_id, filters),
            Action::UnsubscribeAll { request_id } => self.handle_unsubscribe_all(request_id),
            Action::Unsubscribe {
                request_id,
                subscription_id,
            } => self.handle_unsubscribe(request_id, subscription_id),
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

        self.to_client_tx.send(response).await.unwrap();
    }

    async fn notify_subscriber(sub: &mut Subscription, value: &Value) {
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
            }
            // Filter criteria does not match
            Ok(false) => {
                debug!(
                    "Update does not match filter for SubscriptionId {}",
                    sub.subscription_id
                );
            }
            Err(filter::Error::ValueIsNotANumber) => {
                let response = new_subscription_notification_error(
                    sub.subscription_id,
                    BAD_REQUEST_FILTER_INVALID.into(),
                );
                sub.to_client_tx.send(Err(response)).await.unwrap();
            }
        }
    }

    async fn run(&mut self) {
        loop {
            select! {
                client_message = self.message_rx.next().fuse() => {
                    if let Some(client_message) = client_message {
                        self.dispatch_message(client_message).await;
                    }
                }
                signal = self.signals_rx.next().fuse() => {
                    if let Some((path, value)) = signal {
                        self.signal_cache.insert(path.clone(), value.clone());
                        // Update subscriptions
                        for subscription_id in self.path_subscriptions.get(&path).unwrap_or(&Vec::new()).iter() {
                            self.subscription_tx.send((subscription_id.clone(), value.clone())).await.unwrap();
                        }
                    }
                },
                sub_notify = self.subscription_rx.next().fuse() => {
                    if let Some((subscription_id, signal_value)) = sub_notify {
                        if let Some(sub) = self.subscriptions.get_mut(&subscription_id) {
                            Self::notify_subscriber(sub, &signal_value).await;
                        }
                    }
                }
                periodic_sub_notify = self.periodic_subscription_rx.next().fuse() => {
                    if let Some(subscription_id) = periodic_sub_notify {
                        if let Some(sub) = self.subscriptions.get_mut(&subscription_id) {
                            if let Some(signal_value) = self.signal_cache.get(&sub.path) {
                                Self::notify_subscriber(sub, signal_value).await;
                            }
                        }
                    }
                }
            }
        }
    }
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
        to_client_tx: Sender<Result<ActionSuccessResponse, ActionErrorResponse>>,
    },
    Disconnected {
        socket_addr: SocketAddr,
    },
    Message {
        socket_addr: SocketAddr,
        action: Action,
    },
}

struct ClientRef {
    join_handle: JoinHandle<()>,
    signal_tx: Sender<(ActionPath, Value)>,
    message_tx: Sender<Action>,
}

pub struct SignalServer {
    /// Report a client action e.g. disconnect, connect or new message
    pub clients_tx: Sender<ClientAction>,
    clients_rx: Receiver<ClientAction>,

    /// Currently connected clients
    clients: HashMap<SocketAddr, ClientRef>,

    /// Update a path with a new value
    pub signals_tx: Sender<(ActionPath, Value)>,
    signals_rx: Receiver<(ActionPath, Value)>,
}

impl SignalServer {
    pub fn new() -> Self {
        let (clients_tx, clients_rx) = channel(MAX_CLIENT_BUFFER);
        let (signals_tx, signals_rx) = channel(MAX_CLIENT_BUFFER);

        Self {
            clients_tx,
            clients_rx,
            clients: HashMap::new(),
            signals_tx,
            signals_rx,
        }
    }

    /// Process a client action e.g. connect or disconnect and handle an
    /// incoming client message.
    async fn process_client_requests(&mut self, client_action: ClientAction) {
        info!("Starting client request processor");
        trace!("Processing client action: {:#?}", client_action);
        match client_action {
            ClientAction::Connected {
                socket_addr,
                to_client_tx,
            } => {
                let mut client = Client::new(socket_addr.clone(), to_client_tx);

                let client_ref = ClientRef {
                    signal_tx: client.signals_tx.clone(),
                    message_tx: client.message_tx.clone(),
                    join_handle: tokio::spawn(async move { client.run().await }),
                };
                debug!("Inserting connected client: {}", socket_addr);
                self.clients.insert(socket_addr, client_ref);
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
                if let Some(client_ref) = self.clients.get_mut(&socket_addr) {
                    debug!("Forwarding message to client");
                    client_ref.message_tx.send(action).await.unwrap();
                }
            }
        }
    }

    /// Process incoming signals, storing and distributing them to clients.
    /// Dispatch incoming client requests to be handled.
    pub async fn run(&mut self) {
        loop {
            select! {
                signal = self.signals_rx.next().fuse() => {
                    if let Some((path, value)) = signal {
                        for (_socket_addr, client) in self.clients.iter_mut() {
                            client.signal_tx.send((path.clone(), value.clone())).await.unwrap();
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
