use std::str::FromStr;
use std::{marker::PhantomData, time::SystemTime};

use crate::old::Context;
use crate::{engine::StreamNode, header_map};
use arroyo_formats::old::DataDeserializer;
use arroyo_formats::SchemaData;
use arroyo_macro::source_fn;
use arroyo_rpc::formats::BadData;
use arroyo_rpc::{
    grpc::{StopMode, TableDescriptor},
    var_str::VarStr,
    ControlMessage, OperatorConfig,
};
use arroyo_state::tables::global_keyed_map::GlobalKeyedState;
use arroyo_types::{Data, Message, UserError, Watermark};
use bincode::{Decode, Encode};
use futures::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::select;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::{connect_async, tungstenite};
use tracing::{debug, info};
use tungstenite::http::Request;
use typify::import_types;
use arroyo_operator::RateLimiter;

import_types!(
    schema = "../connector-schemas/websocket/table.json",
    convert = { {type = "string", format = "var-str"} = VarStr });

#[derive(Clone, Debug, Encode, Decode, PartialEq, PartialOrd, Default)]
pub struct WebsocketSourceState {}

#[derive(StreamNode)]
pub struct WebsocketSourceFunc<K, T>
where
    K: DeserializeOwned + Data,
    T: SchemaData,
{
    url: String,
    headers: Vec<(String, String)>,
    subscription_messages: Vec<String>,
    deserializer: DataDeserializer<T>,
    bad_data: Option<BadData>,
    rate_limiter: RateLimiter,
    state: WebsocketSourceState,
    _t: PhantomData<K>,
}

#[source_fn(out_k = (), out_t = T)]
impl<K, T> WebsocketSourceFunc<K, T>
where
    K: DeserializeOwned + Data,
    T: SchemaData,
{
    pub fn from_config(config: &str) -> Self {
        let config: OperatorConfig =
            serde_json::from_str(config).expect("Invalid config for WebsocketSource");
        let table: WebsocketTable =
            serde_json::from_value(config.table).expect("Invalid table config for WebsocketSource");

        // Include subscription_message for backwards compatibility
        let mut subscription_messages = vec![];
        if let Some(message) = table.subscription_message {
            subscription_messages.push(message.to_string());
        };
        subscription_messages.extend(
            table
                .subscription_messages
                .into_iter()
                .map(|m| m.to_string()),
        );

        let headers = header_map(table.headers)
            .into_iter()
            .map(|(k, v)| {
                (
                    (&k).try_into()
                        .expect(&format!("invalid header name {}", k)),
                    (&v).try_into()
                        .expect(&format!("invalid header value {}", v)),
                )
            })
            .collect();

        Self {
            url: table.endpoint,
            headers,
            subscription_messages,
            deserializer: DataDeserializer::new(
                config.format.expect("WebsocketSource requires a format"),
                config.framing,
            ),
            bad_data: config.bad_data,
            rate_limiter: RateLimiter::new(),
            state: WebsocketSourceState::default(),
            _t: PhantomData,
        }
    }

    fn name(&self) -> String {
        "WebsocketSource".to_string()
    }

    fn tables(&self) -> Vec<TableDescriptor> {
        vec![arroyo_state::global_table("e", "websocket source state")]
    }

    async fn on_start(&mut self, ctx: &mut Context<(), T>) {
        let s: GlobalKeyedState<(), WebsocketSourceState, _> =
            ctx.state.get_global_keyed_state('e').await;

        if let Some(state) = s.get(&()) {
            self.state = state.clone();
        }
    }

    async fn our_handle_control_message(
        &mut self,
        ctx: &mut Context<(), T>,
        msg: Option<ControlMessage>,
    ) -> Option<SourceFinishType> {
        match msg? {
            ControlMessage::Checkpoint(c) => {
                debug!("starting checkpointing {}", ctx.task_info.task_index);
                let mut s: GlobalKeyedState<(), WebsocketSourceState, _> =
                    ctx.state.get_global_keyed_state('e').await;
                s.insert((), self.state.clone()).await;

                if self.checkpoint(c, ctx).await {
                    return Some(SourceFinishType::Immediate);
                }
            }
            ControlMessage::Stop { mode } => {
                info!("Stopping websocket source: {:?}", mode);

                match mode {
                    StopMode::Graceful => {
                        return Some(SourceFinishType::Graceful);
                    }
                    StopMode::Immediate => {
                        return Some(SourceFinishType::Immediate);
                    }
                }
            }
            ControlMessage::Commit { .. } => {
                unreachable!("sources shouldn't receive commit messages");
            }
            ControlMessage::LoadCompacted { compacted } => {
                ctx.load_compacted(compacted).await;
            }
            ControlMessage::NoOp => {}
        }
        None
    }

    async fn handle_message(
        &mut self,
        msg: &[u8],
        ctx: &mut Context<(), T>,
    ) -> Result<(), UserError> {
        let iter = self.deserializer.deserialize_slice(msg).await;
        for value in iter {
            ctx.collect_source_record(
                SystemTime::now(),
                value,
                &self.bad_data,
                &mut self.rate_limiter,
            )
            .await?;
        }

        Ok(())
    }

    async fn run(&mut self, ctx: &mut Context<(), T>) -> SourceFinishType {
        match self.run_int(ctx).await {
            Ok(r) => r,
            Err(e) => {
                ctx.report_error(e.name.clone(), e.details.clone()).await;

                panic!("{}: {}", e.name, e.details);
            }
        }
    }

    async fn run_int(&mut self, ctx: &mut Context<(), T>) -> Result<SourceFinishType, UserError> {
        let uri = match Uri::from_str(&self.url.to_string()) {
            Ok(uri) => uri,
            Err(e) => {
                ctx.report_error("Failed to parse endpoint".to_string(), format!("{:?}", e))
                    .await;
                panic!("Failed to parse endpoint: {:?}", e);
            }
        };

        let host = match uri.host() {
            Some(host) => host,
            None => {
                ctx.report_error("Endpoint must have a host".to_string(), "".to_string())
                    .await;
                panic!("Endpoint must have a host");
            }
        };

        let mut request_builder = Request::builder().uri(&self.url);

        for (k, v) in &self.headers {
            request_builder = request_builder.header(k, v);
        }

        let request = match request_builder
            .header("Host", host)
            .header("Sec-WebSocket-Key", generate_key())
            .header("Sec-WebSocket-Version", "13")
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .body(())
        {
            Ok(request) => request,
            Err(e) => {
                ctx.report_error("Failed to build request".to_string(), format!("{:?}", e))
                    .await;
                panic!("Failed to build request: {:?}", e);
            }
        };

        let ws_stream = match connect_async(request).await {
            Ok((ws_stream, _)) => ws_stream,
            Err(e) => {
                ctx.report_error(
                    "Failed to connect to websocket server".to_string(),
                    e.to_string(),
                )
                .await;
                panic!("{}", e);
            }
        };

        let (mut tx, mut rx) = ws_stream.split();

        for msg in &self.subscription_messages {
            if let Err(e) = tx.send(tungstenite::Message::Text(msg.clone())).await {
                ctx.report_error(
                    "Failed to send subscription message to websocket server".to_string(),
                    e.to_string(),
                )
                .await;
                panic!(
                    "Failed to send subscription message to websocket server: {:?}",
                    e
                );
            }
        }

        // since there's no way to partition across a websocket source, only read on the first task
        if ctx.task_info.task_index == 0 {
            loop {
                select! {
                    message = rx.next()  => {
                        match message {
                            Some(Ok(msg)) => {
                                match msg {
                                    tungstenite::Message::Text(t) => {
                                        self.handle_message(&t.as_bytes(), ctx).await?
                                    },
                                    tungstenite::Message::Binary(bs) => {
                                        self.handle_message(&bs, ctx).await?
                                    },
                                    tungstenite::Message::Ping(d) => {
                                        tx.send(tungstenite::Message::Pong(d)).await
                                            .map(|_| ())
                                            .map_err(|e| UserError::new("Failed to send pong to websocket server", e.to_string()))?
                                    },
                                    tungstenite::Message::Pong(_) => {
                                        // ignore
                                    },
                                    tungstenite::Message::Close(_) => {
                                        ctx.report_error("Received close frame from server".to_string(), "".to_string()).await;
                                        panic!("Received close frame from server");
                                    },
                                    tungstenite::Message::Frame(_) => {
                                        // this should be captured by tungstenite
                                    },
                                };
                            }
                        Some(Err(e)) => {
                            ctx.report_error("Error while reading from websocket".to_string(), format!("{:?}", e)).await;
                            panic!("Error while reading from websocket: {:?}", e);
                        }
                        None => {
                            info!("Socket closed");
                            return Ok(SourceFinishType::Final);
                        }
                    }
                    }
                    control_message = ctx.control_rx.recv() => {
                        if let Some(r) = self.our_handle_control_message(ctx, control_message).await {
                            return Ok(r);
                        }
                    }
                }
            }
        } else {
            // otherwise set idle and just process control messages
            ctx.broadcast(Message::Watermark(Watermark::Idle)).await;
            loop {
                let msg = ctx.control_rx.recv().await;
                if let Some(r) = self.our_handle_control_message(ctx, msg).await {
                    return Ok(r);
                }
            }
        }
    }
}
