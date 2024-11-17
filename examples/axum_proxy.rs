//! This example demonstrates how to create a WebSocket broadcast server that:
//! 1. Accepts WebSocket connections from clients
//! 2. Allows clients to subscribe to topics
//! 3. Connects to an upstream WebSocket server
//! 4. Broadcasts messages from upstream to subscribed clients on specific topics
//!
//! # Usage
//!
//! ```bash
//! yawcc c ws://localhost:3001/ws --input-as-json
//! > {"method":"subscribe","symbol":"BTCUSDT","topic":"orderbook","levels":50} // proxy
//! ```

use std::{
    borrow::Cow,
    collections::HashMap,
    io,
    sync::{Arc, RwLock},
    time::Duration,
};

use axum::{extract::State, response::IntoResponse, routing::get, Router};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpListener,
    sync::{
        broadcast,
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    },
    time::{interval, timeout},
};
use tokio_stream::{wrappers::BroadcastStream, StreamMap};
use url::Url;
use yawc::{
    close::CloseCode,
    frame::{FrameView, OpCode},
    CompressionLevel, IncomingUpgrade, Options, UpgradeFut, WebSocket,
};

#[tokio::main]
async fn main() {
    // Initialize logging for debugging
    simple_logger::init_with_level(log::Level::Debug).expect("log");

    if let Err(err) = server().await {
        log::error!("{}", err);
    }
}

/// Subscription topic that clients can subscribe to.
/// Each topic is associated with a specific cryptocurrency coin.
#[derive(Debug, PartialEq, Eq, Clone, Hash, PartialOrd, Ord)]
struct Topic<'a> {
    symbol: Cow<'a, str>,
    name: Cow<'a, str>,
    levels: Option<u8>,
}

impl<'a> TryFrom<&'a str> for Topic<'a> {
    type Error = ();

    fn try_from(s: &'a str) -> Result<Self, Self::Error> {
        let mut parts = s.split('.');

        let name = parts.next().ok_or(())?;
        // second could be the levels or the symbol
        let second = parts.next().ok_or(())?;

        if name == "orderbook" {
            let levels = second.parse().map_err(|_| ())?;
            let symbol = parts.next().ok_or(())?;

            Ok(Topic {
                symbol: Cow::from(symbol),
                name: Cow::from(name),
                levels: Some(levels),
            })
        } else {
            Ok(Topic {
                symbol: Cow::from(second),
                name: Cow::from(name),
                levels: None,
            })
        }
    }
}

impl<'a> Serialize for Topic<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use std::fmt::Write;

        let mut buf = String::with_capacity(32);
        buf.push_str(&self.name);

        if let Some(levels) = self.levels {
            buf.push('.');
            // Safe because levels is u8
            write!(buf, "{}", levels).unwrap();
        }

        buf.push('.');
        buf.push_str(&self.symbol);

        serializer.serialize_str(&buf)
    }
}

/// Application state shared between all clients
struct AppState {
    // Map of topics to broadcast channels
    topics: RwLock<HashMap<Topic<'static>, broadcast::Sender<Bytes>>>,
    // Channel to send subscription updates to upstream connection
    upstream: UnboundedSender<Subscription>,
}

impl AppState {
    pub fn new(upstream: UnboundedSender<Subscription>) -> Self {
        Self {
            topics: Default::default(),
            upstream,
        }
    }
}

/// Initialize and start the WebSocket server
async fn server() -> io::Result<()> {
    // Channel for communicating with upstream connection
    let (tx, rx) = unbounded_channel();

    // Initialize shared application state
    let state = Arc::new(AppState::new(tx));
    let router = Router::new()
        .route("/ws", get(on_websocket))
        .with_state(Arc::clone(&state));

    // Spawn task to handle upstream connection
    tokio::spawn(async move { connect_upstream(rx, state).await });

    // Start the server
    let listener = TcpListener::bind("0.0.0.0:3001").await?;
    axum::serve(listener, router).await
}

/// Handle new WebSocket connection requests
async fn on_websocket(
    State(state): State<Arc<AppState>>,
    ws: IncomingUpgrade,
) -> impl IntoResponse {
    // Configure WebSocket options
    let options = Options::default()
        .with_compression_level(CompressionLevel::best())
        .with_max_payload_read(8192);

    let (response, fut) = ws.upgrade(options).unwrap();
    // Spawn task to handle the client connection
    tokio::task::spawn(async move {
        if let Err(e) = on_websocket_client(state, fut).await {
            log::error!("websocket: {}", e);
        }
    });

    response
}

/// Message format for client subscription requests
#[derive(Deserialize)]
struct UserSubscribe {
    method: String,
    symbol: String,
    topic: String,
    levels: Option<u8>,
}

/// Handle individual WebSocket client connections
async fn on_websocket_client(state: Arc<AppState>, fut: UpgradeFut) -> yawc::Result<()> {
    let mut ws = fut.await?;

    // Track all broadcast streams this client is subscribed to
    let mut streams = StreamMap::new();

    loop {
        tokio::select! {
            // Handle incoming broadcast messages
            Some((_, res)) = streams.next() => {
                match res {
                    Ok(input) => {
                        let _ = ws.send(FrameView::text(input)).await;
                    }
                    Err(_) => {
                        // just give up on the stream
                    }
                }
            }
            // Handle client messages
            maybe_frame = ws.next() => {
                let Some(frame) = maybe_frame else {
                    log::debug!("WebSocket connection closed");

                    let keys: Vec<_> = streams.keys().cloned().collect();
                    for topic in keys {
                        streams.remove(&topic);
                        unsubscribe_user(&state, topic);
                    }

                    return Ok(());
                };

                match serde_json::from_slice(&frame.payload) {
                    Ok(ok) => {
                       if let Err(err) = on_subscription(&state, &mut streams, ok) {
                           let _ = ws.send(FrameView::close(CloseCode::Abnormal, err)).await;
                       }
                    }
                    Err(err) => {
                        log::warn!("user: {}", err);
                        let _ = ws
                            .send(FrameView::text(format!("unable to parse input: {err}")))
                            .await;
                    }
                }
            }
        }
    }
}

/// Process client subscription requests
fn on_subscription(
    state: &AppState,
    streams: &mut StreamMap<Topic<'static>, BroadcastStream<Bytes>>,
    sub: UserSubscribe,
) -> Result<(), String> {
    if !["orderbook", "publicTrade", "ticker"].contains(&sub.topic.as_str()) {
        return Err(format!("unknown topic: {}", sub.topic));
    }

    let topic = Topic {
        symbol: Cow::from(sub.symbol),
        name: Cow::from(sub.topic),
        levels: sub.levels,
    };

    match sub.method.as_str() {
        "subscribe" => {
            let mut topics = state.topics.write().unwrap();
            if let Some(tx) = topics.get(&topic) {
                // Topic exists, just add this client as a subscriber
                let rx = tx.subscribe();
                streams.insert(topic, BroadcastStream::new(rx));
            } else {
                // Create new topic and broadcast channel
                let tx = broadcast::Sender::new(1024);
                topics.insert(topic.clone(), tx.clone());
                drop(topics);

                log::debug!("Subscribing to {:?}", topic);

                let rx = tx.subscribe();
                streams.insert(topic.clone(), BroadcastStream::new(rx));

                // Notify upstream about new subscription
                let _ = state.upstream.send(Subscription::Sub(topic));
            }
        }
        "unsubscribe" => {
            streams.remove(&topic);
            unsubscribe_user(&state, topic);
        }
        _ => {}
    }

    Ok(())
}

fn unsubscribe_user(state: &AppState, topic: Topic<'static>) {
    let mut topics = state.topics.write().unwrap();
    if let Some(tx) = topics.get(&topic) {
        if tx.receiver_count() == 0 {
            // No more subscribers, remove the topic
            log::debug!("Removing {:?} topic", topic);
            topics.remove(&topic);

            // Notify upstream about unsubscription
            let _ = state.upstream.send(Subscription::Unsub(topic));
        }
    }
}

/// Internal subscription commands sent to upstream connection
enum Subscription {
    Sub(Topic<'static>),
    Unsub(Topic<'static>),
}

/// Maintain connection to upstream WebSocket server
async fn connect_upstream(mut rx: UnboundedReceiver<Subscription>, state: Arc<AppState>) {
    let base_url: Url = "wss://stream.bybit.com/v5/public/linear".parse().unwrap();
    let client = reqwest::Client::new();

    loop {
        log::info!("Connecting upstream {base_url}");

        // Connect to upstream with timeout
        let mut ws = match timeout(
            Duration::from_secs(5),
            WebSocket::reqwest(
                base_url.clone(),
                client.clone(),
                Options::default().with_compression_level(CompressionLevel::best()),
            ),
        )
        .await
        {
            Ok(res) => res.expect("WebSocket upgrade"),
            Err(err) => {
                log::error!("Unable to connect upstream ({base_url}): {err}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let mut next_id = 0;

        // Resubscribe to all active topics
        let subscriptions: Vec<Topic<'static>> = {
            let topics = state.topics.read().unwrap();
            topics.keys().cloned().collect()
        };
        if !subscriptions.is_empty() {
            next_id += 1;
            let id = next_id;

            let _ = ws
                .send_json(&BybitSubscribe {
                    req_id: id.to_string(),
                    op: "subscribe",
                    args: subscriptions,
                })
                .await;
        }

        let mut ping_ticker = interval(Duration::from_secs(5));

        loop {
            tokio::select! {
                // Handle subscription changes
                maybe_msg = rx.recv() => {
                    let Some(msg) = maybe_msg else {
                        return;
                    };

                    next_id += 1;
                    let id = next_id;

                    let (op, topic) = match msg {
                        Subscription::Sub(topic) => {
                            ("subscribe", topic)
                        }
                        Subscription::Unsub(topic) => {
                            ("unsubscribe", topic)
                        }
                    };

                    let _ = ws.send_json(&BybitSubscribe {
                        req_id: id.to_string(),
                        op,
                        args: vec![topic],
                    }).await;
                }
                // Handle upstream messages
                maybe_msg = ws.next() => {
                    let Some(msg) = maybe_msg else {
                        // reconnect
                        break;
                    };

                    if msg.opcode == OpCode::Text {
                        on_upstream_message(&state, msg);
                    }
                }
                // Send periodic pings
                _ = ping_ticker.tick() => {
                    let _ = ws.send(FrameView::ping("ping")).await;
                }
            }
        }
    }
}

/// Subscription message format for upstream server
#[derive(Serialize)]
struct BybitSubscribe<'a> {
    req_id: String,
    op: &'a str,
    args: Vec<Topic<'a>>,
}

/// Message format received from upstream server
#[derive(Deserialize)]
struct BybitMsg<'a> {
    topic: &'a str,
    r#type: &'a str,
}

#[derive(Deserialize)]
struct BybitSub<'a> {
    op: &'a str,
}

/// Process messages received from upstream and broadcast to subscribers
fn on_upstream_message(state: &AppState, frame: FrameView) {
    // log::debug!("<< {}", std::str::from_utf8(&frame.payload).unwrap());
    match serde_json::from_slice::<BybitMsg>(&frame.payload) {
        Ok(ok) => {
            // Convert upstream message format to internal Topic
            let topic = Topic::try_from(ok.topic).expect("topic");

            // Broadcast message to all subscribers of this topic
            let topics = state.topics.read().unwrap();
            if let Some(tx) = topics.get(&topic).cloned() {
                let _ = tx.send(frame.payload);
            }
        }
        Err(err) => match serde_json::from_slice::<BybitSub>(&frame.payload) {
            Ok(ok) => {
                log::debug!("{} completed", ok.op);
            }
            Err(err) => {
                let text = std::str::from_utf8(&frame.payload).unwrap();
                log::warn!("{}: {}", err, text);
            }
        },
    }
}
