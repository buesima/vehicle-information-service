// SPDX-License-Identifier: MIT

use async_tungstenite::WebSocketStream;
use async_tungstenite::{
    tokio::{connect_async, TokioAdapter},
    tungstenite::error::Error as WsError,
    tungstenite::protocol::Message,
};
use futures::prelude::*;
use futures::stream::SplitSink;
use futures::stream::SplitStream;
use futures_util::{future, StreamExt};
use log::{debug, error, warn};
use serde::de::DeserializeOwned;
use serde_json;
use std::convert::Into;
use std::io;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::net::TcpStream;

pub use vehicle_information_service::{
    Action, ActionErrorResponse, ActionPath, ActionSuccessResponse, Filters, ReqID, SubscriptionID,
};

#[derive(Error, Debug)]
pub enum VISClientError {
    #[error("WebsocketError")]
    WebSocketError(#[from] WsError),
    #[error("SerdeError")]
    SerdeError(#[from] serde_json::Error),
    #[error("IoError")]
    IoError(#[from] io::Error),
    #[error("Url parsing error")]
    UrlParseError(#[from] url::ParseError),
    #[error("VIS error")]
    VisError(ActionErrorResponse),
    #[error("Websocket connection lost while waiting for result")]
    LostConnectionWithoutResponse,
}

impl From<ActionErrorResponse> for VISClientError {
    fn from(action_error: ActionErrorResponse) -> Self {
        VISClientError::VisError(action_error)
    }
}

type Result<T> = core::result::Result<T, VISClientError>;

pub struct VISClient {
    #[allow(dead_code)]
    server_address: String,
    // client: websocket::client::r#async::Client<TcpStream>,w
    ws_tx: SplitSink<WebSocketStream<TokioAdapter<TcpStream>>, Message>,
    ws_rx: SplitStream<WebSocketStream<TokioAdapter<TcpStream>>>,
}

impl VISClient {
    #[allow(clippy::needless_lifetimes)] // Clippy false positive
    pub async fn connect(server_address: &str) -> Result<Self> {
        let (ws_stream, _) = connect_async(server_address).await?;
        let (ws_tx, ws_rx) = ws_stream.split();

        debug!("Connected to: {}", server_address);
        Ok(Self {
            server_address: server_address.to_string(),
            ws_tx,
            ws_rx,
        })
    }

    /// Retrieve vehicle signals.
    pub async fn get<T>(mut self, path: ActionPath) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let request_id = ReqID::default();
        let get = Action::Get { path, request_id };

        let get_msg = serde_json::to_string(&get)?;

        self.ws_tx.send(Message::Text(get_msg)).await?;

        let get_stream = self
            .ws_rx
            .map_err(Into::<VISClientError>::into)
            // Filter Websocket text messages
            .try_filter_map(|msg| {
                if let Message::Text(txt) = msg {
                    future::ok(Some(txt))
                } else {
                    future::ok(None)
                }
            })
            // Deserialize
            .and_then(|txt| {
                let txt_err = txt.clone();
                if let Ok(value) = serde_json::from_str::<ActionSuccessResponse>(&txt) {
                    return future::ok(value);
                }

                // Attempt to deserialize a VIS error
                let vis_error: std::result::Result<serde_json::Value, _> =
                    serde_json::from_str(&txt_err);
                // Workaround for https://github.com/serde-rs/json/issues/505
                // once this is fixed it should not be necessary to deserialize to Value first and then
                // to the actual type
                match vis_error {
                    Err(serde_error) => {
                        error!("{}", serde_error);
                        future::err(serde_error.into())
                    }
                    Ok(vis_error) => {
                        let vis_error = serde_json::from_value::<ActionErrorResponse>(vis_error);
                        match vis_error {
                            Err(serde_error) => {
                                error!("{}", serde_error);
                                future::err(serde_error.into())
                            }
                            Ok(vis_error) => future::err(VISClientError::VisError(vis_error)),
                        }
                    }
                }
            })
            // Filter get responses
            .try_filter_map(|response| {
                match response {
                    ActionSuccessResponse::Get {
                        request_id: resp_request_id,
                        value,
                        ..
                    } => future::ok(Some((resp_request_id, value))),
                    // No get response
                    _ => future::ok(None),
                }
            })
            // Filter get responses that have correct request_id
            .try_filter_map(|(resp_request_id, value)| {
                if request_id != resp_request_id {
                    return future::ok(None);
                }

                future::ok(Some(value))
            })
            // Deserialize value of get response
            .and_then(|value| future::ready(serde_json::from_value(value).map_err(Into::into)))
            .into_future();

        let (get_response, _stream) = get_stream.await;
        get_response.unwrap().map_err(Into::into)
    }

    /// Subscribe to the given path's vehicle signals.
    /// This will return aws_rx containing all incoming values
    pub async fn subscribe_raw(
        mut self,
        path: ActionPath,
        filters: Option<Filters>,
    ) -> Result<impl TryStream<Ok = ActionSuccessResponse, Error = VISClientError>> {
        let request_id = ReqID::default();
        let subscribe = Action::Subscribe {
            path,
            filters,
            request_id,
        };

        let subscribe_msg = serde_json::to_string(&subscribe)?;

        self.ws_tx.send(Message::Text(subscribe_msg)).await?;

        Ok(self.ws_rx.map_err(Into::into).try_filter_map(|msg| {
            debug!("VIS Message {:#?}", msg);
            if let Message::Text(txt) = msg {
                match serde_json::from_str::<ActionSuccessResponse>(&txt) {
                    Ok(success_response) => future::ok(Some(success_response)),
                    // propagate deserialize error to stream
                    Err(serde_error) => future::err(serde_error.into()),
                }
            } else {
                future::ok(None)
            }
        }))
    }

    /// Subscribe to the given path's vehicle signals.
    pub async fn subscribe<T>(
        mut self,
        path: ActionPath,
        filters: Option<Filters>,
    ) -> Result<impl TryStream<Ok = (SubscriptionID, T), Error = VISClientError>>
    where
        T: DeserializeOwned,
    {
        let request_id = ReqID::default();
        let subscribe = Action::Subscribe {
            path,
            filters,
            request_id,
        };

        let subscribe_msg = serde_json::to_string(&subscribe)?;

        // Send subscribe request to server
        self.ws_tx.send(Message::Text(subscribe_msg)).await?;

        let subscription_id: Arc<Mutex<Option<SubscriptionID>>> = Default::default();

        Ok(self
            .ws_rx
            .map_err::<VISClientError, _>(Into::into)
            .try_filter_map(move |msg| {
                debug!("VIS Message {:#?}", msg);

                if let Message::Text(txt) = msg {
                    match serde_json::from_str::<ActionSuccessResponse>(&txt) {
                        Ok(ActionSuccessResponse::Subscribe {
                            subscription_id: resp_subscription_id,
                            request_id: resp_request_id,
                            ..
                        }) => {
                            // Make sure this is actually the response to our subscription request
                            if resp_request_id != request_id {
                                return future::ok(None);
                            }
                            // Store subscription_id to make sure the stream only returns values based on this subscription
                            *subscription_id.lock().unwrap() = Some(resp_subscription_id);
                            future::ok(None)
                        }
                        Ok(ActionSuccessResponse::Subscription {
                            subscription_id: resp_subscription_id,
                            value,
                            ..
                        }) => {
                            if *subscription_id.lock().unwrap() != Some(resp_subscription_id) {
                                return future::ok(None);
                            }

                            match serde_json::from_value::<T>(value) {
                                Ok(stream_value) => {
                                    future::ok(Some((resp_subscription_id, stream_value)))
                                }
                                // propagate deserialize error to stream
                                Err(serde_error) => future::err(serde_error.into()),
                            }
                        }
                        Ok(_) => future::ok(None),
                        // propagate deserialize error to stream
                        Err(serde_error) => future::err(serde_error.into()),
                    }
                } else {
                    future::ok(None)
                }
            })
            .map_err(Into::into))
    }

    /// Subscribe to the given path's vehicle signals.
    pub async fn unsubscribe_all<T>(mut self) -> Result<()>
    where
        T: DeserializeOwned,
    {
        let request_id = ReqID::default();
        let unsubscribe_all = Action::UnsubscribeAll { request_id };

        let unsubscribe_all_msg = serde_json::to_string(&unsubscribe_all)?;

        self.ws_tx.send(Message::Text(unsubscribe_all_msg)).await?;

        while let Some(Ok(msg)) = self.ws_rx.next().await {
            debug!("VIS Message {:#?}", msg);

            if let Message::Text(txt) = msg {
                let action_success = serde_json::from_str::<ActionSuccessResponse>(&txt);

                match action_success {
                    Ok(ActionSuccessResponse::UnsubscribeAll {
                        request_id: resp_request_id,
                        ..
                    }) => {
                        // Request id mismatch
                        if resp_request_id != request_id {
                            continue;
                        }

                        return Ok(());
                    }
                    Ok(_) => continue,
                    Err(serde_error) => {
                        warn!(
                            "Failed to deserialize stream response, error: {}",
                            serde_error
                        );
                        return Err(serde_error.into());
                    }
                }
            }
        }

        return Err(VISClientError::LostConnectionWithoutResponse);
    }
}
