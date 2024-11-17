# WebSocket Broadcasting Server Example

This server implements a pub/sub system that:

1. Accepts WebSocket connections from downstream clients
2. Manages topic-based subscriptions (orderbook, trades, tickers)
3. Maintains a persistent connection to an upstream WebSocket server (Bybit)
4. Efficiently broadcasts messages to subscribed clients using tokio broadcast channels

## Detailed Implementation Analysis

### Basic Usage

```bash
# Start the server
cargo run

# Connect using WebSocket client utility
yawcc c ws://localhost:3001/ws --input-as-json
> {"method":"subscribe","symbol":"BTCUSDT","topic":"orderbook","levels":50}
```

### Core Data Structures

#### Topic Implementation

```rust
struct Topic<'a> {
    symbol: Cow<'a, str>,
    name: Cow<'a, str>,
    levels: Option<u8>,
}
```

The `Topic` struct uses generic lifetime parameters and `Cow<str>` for several reasons:

1. `Cow` (Clone-on-Write) enables zero-copy operations when strings are static (like hardcoded topics) while allowing owned `String` when needed (like dynamic user input)
2. The lifetime parameter `'a` makes the struct flexible for both static and dynamic lifetimes
3. `levels` is `Option<u8>` because:
   - It's only relevant for orderbook subscriptions
   - u8 is sufficient for depth levels (max 255)
   - Keeps memory footprint minimal

#### Topic Parsing

```rust
impl<'a> TryFrom<&'a str> for Topic<'a> {
    type Error = ();

    fn try_from(s: &'a str) -> Result<Self, Self::Error> {
        let mut parts = s.split('.');
        let name = parts.next().ok_or(())?;
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
```

This implementation:

1. Uses `TryFrom` trait for robust string parsing
2. Handles two format types:
   - `orderbook.50.BTCUSDT` for orderbook subscriptions
   - `publicTrade.BTCUSDT` for other subscriptions
3. Returns `Result` with unit error type since detailed error handling is handled at a higher level
4. Preserves zero-copy string handling through `Cow`

### Connection Management

#### Server Initialization

```rust
async fn server() -> io::Result<()> {
    let (tx, rx) = unbounded_channel();
    let state = Arc::new(AppState::new(tx));
    let router = Router::new()
        .route("/ws", get(on_websocket))
        .with_state(Arc::clone(&state));

    tokio::spawn(async move { connect_upstream(rx, state).await });

    let listener = TcpListener::bind("0.0.0.0:3001").await?;
    axum::serve(listener, router).await
}
```

Design decisions:

1. Uses `unbounded_channel` for upstream communication because:
   - Subscription messages are infrequent
   - Back-pressure isn't critical for subscription handling
   - Simplifies error handling
2. `Arc<AppState>` enables safe shared state across tasks
3. Spawns upstream connection in separate task for isolation

#### WebSocket Upgrade Handler

```rust
async fn on_websocket(
    State(state): State<Arc<AppState>>,
    ws: IncomingUpgrade,
) -> impl IntoResponse {
    let options = Options::default()
        .with_compression_level(CompressionLevel::best())
        .with_max_payload_read(8192);

    let (response, fut) = ws.upgrade(options).unwrap();
    tokio::task::spawn(async move {
        if let Err(e) = on_websocket_client(state, fut).await {
            log::error!("websocket: {}", e);
        }
    });

    response
}
```

Key features:

1. WebSocket compression enabled for bandwidth efficiency
2. 8KB payload limit prevents memory exhaustion attacks
3. Spawns each client in separate task for:
   - Connection isolation
   - Independent error handling
   - Parallel processing

### Client Message Handling

#### Client Connection Handler

```rust
async fn on_websocket_client(state: Arc<AppState>, fut: UpgradeFut) -> yawc::Result<()> {
    let mut ws = fut.await?;
    let mut streams = StreamMap::new();

    loop {
        tokio::select! {
            Some((_, res)) = streams.next() => {
                match res {
                    Ok(input) => {
                        let _ = ws.send(FrameView::text(input)).await;
                    }
                    Err(_) => {
                        // Stream error handling
                    }
                }
            }
            maybe_frame = ws.next() => {
                // Handle client messages
            }
        }
    }
}
```

Implementation highlights:

1. Uses `StreamMap` to:
   - Efficiently multiplex multiple broadcast streams
   - Associate messages with their topics
   - Handle stream errors independently
2. `tokio::select!` enables concurrent handling of:
   - Incoming client messages
   - Broadcast messages from subscribed topics

#### Subscription Processing

```rust
fn on_subscription(
    state: &AppState,
    streams: &mut StreamMap<Topic<'static>, BroadcastStream<Bytes>>,
    sub: UserSubscribe,
) -> Result<(), String> {
    // Topic validation
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
                let rx = tx.subscribe();
                streams.insert(topic, BroadcastStream::new(rx));
            } else {
                let tx = broadcast::Sender::new(1024);
                topics.insert(topic.clone(), tx.clone());
                drop(topics); // Release lock before upstream communication

                let rx = tx.subscribe();
                streams.insert(topic.clone(), BroadcastStream::new(rx));
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
```

Subscription handling details:

1. Early topic validation prevents invalid subscriptions
2. Write lock scope is minimized for better concurrency
3. Channel capacity of 1024 messages balances:
   - Memory usage
   - Message buffering needs
   - Slow consumer handling
4. Explicit lock dropping before upstream communication prevents deadlocks

### Upstream Connection Management

#### Upstream Connection Handler

```rust
async fn connect_upstream(mut rx: UnboundedReceiver<Subscription>, state: Arc<AppState>) {
    let base_url: Url = "wss://stream.bybit.com/v5/public/linear".parse().unwrap();
    let client = reqwest::Client::new();

    loop {
        // Connection logic
        let mut ws = match timeout(
            Duration::from_secs(5),
            WebSocket::reqwest(base_url.clone(), client.clone(), Options::default())
        ).await {
            Ok(res) => res.expect("WebSocket upgrade"),
            Err(err) => {
                log::error!("Unable to connect upstream ({base_url}): {err}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let mut next_id = 0;
        // Resubscription logic
        let subscriptions: Vec<Topic<'static>> = {
            let topics = state.topics.read().unwrap();
            topics.keys().cloned().collect()
        };

        // Message handling loop
        let mut ping_ticker = interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                // Subscription handling
                // Message processing
                // Connection health monitoring
            }
        }
    }
}
```

Important features:

1. Connection management:
   - 5-second connection timeout
   - 2-second reconnection delay
   - Automatic resubscription
2. Message ID management:
   - Monotonically increasing IDs
   - Per-connection ID space
3. Connection health:
   - 5-second ping interval
   - Automatic reconnection on failure
4. Efficient state handling:
   - Minimal lock holding
   - Batch resubscription
   - Stateless reconnection

### Message Processing

#### Upstream Message Handler

```rust
fn on_upstream_message(state: &AppState, frame: FrameView) {
    match serde_json::from_slice::<BybitMsg>(&frame.payload) {
        Ok(ok) => {
            let topic = Topic::try_from(ok.topic).expect("topic");
            let topics = state.topics.read().unwrap();
            if let Some(tx) = topics.get(&topic).cloned() {
                let _ = tx.send(frame.payload);
            }
        }
        Err(err) => {
            // Handle control messages
            match serde_json::from_slice::<BybitSub>(&frame.payload) {
                Ok(ok) => {
                    log::debug!("{} completed", ok.op);
                }
                Err(err) => {
                    let text = std::str::from_utf8(&frame.payload).unwrap();
                    log::warn!("{}: {}", err, text);
                }
            }
        },
    }
}
```

Processing details:

1. Two-phase message parsing:
   - First attempt: data messages
   - Second attempt: control messages
2. Zero-copy message forwarding:
   - Uses `frame.payload` directly
   - Avoids unnecessary copying
3. Minimal lock holding:
   - Read lock only for channel lookup
   - Channel cloned before message send

# Channel Selection Strategy and Snapshot Management

## Channel Architecture Deep Dive

### Understanding tokio's Channel Types

The implementation uses `broadcast::Sender/Receiver` for message distribution, but let's analyze why this was chosen and what other options were available:

```rust
struct AppState {
    // Current implementation
    topics: RwLock<HashMap<Topic<'static>, broadcast::Sender<Bytes>>>,

    // Alternative approach with mpsc
    // topics: RwLock<HashMap<Topic<'static>, Vec<UnboundedSender<Bytes>>>>,
}
```

#### Why broadcast::Sender?

1. **Memory Efficiency**

```rust
// With broadcast
let tx = broadcast::Sender::new(1024);
let rx1 = tx.subscribe();
let rx2 = tx.subscribe();

// vs mpsc approach
let mut senders = Vec::new();
for _ in 0..n_subscribers {
    let (tx, rx) = mpsc::unbounded_channel();
    senders.push(tx);
}
```

The broadcast channel:

- Maintains a single circular buffer for all receivers
- Uses atomic operations for message distribution
- Has O(1) message sending regardless of subscriber count

2. **Message Guarantees**

```rust
// Broadcast ensures all active subscribers get the message
let _ = tx.send(msg);  // Single send operation

// vs mpsc where you'd need
for tx in &senders {
    let _ = tx.send(msg.clone());  // N send operations, N clones
}
```

3. **Resource Management**

```rust
// Broadcast automatically handles dropped receivers
let rx = tx.subscribe();
drop(rx);  // Sender automatically updates receiver count

// vs mpsc where you need manual cleanup
senders.retain(|tx| !tx.is_closed());
```

### Limitations of Current Implementation

The current design has one significant limitation: it doesn't handle market data snapshots properly. Let's explore why this matters and how to fix it.

#### The Snapshot Problem

In market data systems:

1. Initial state (snapshot) represents current market state
2. Delta updates modify this state
3. New subscribers need both:
   - Current snapshot
   - Subsequent updates

Current implementation only handles updates:

```rust
fn on_subscription(
    state: &AppState,
    streams: &mut StreamMap<Topic<'static>, BroadcastStream<Bytes>>,
    sub: UserSubscribe,
) -> Result<(), String> {
    // Current implementation only handles update stream
    let tx = broadcast::Sender::new(1024);
    topics.insert(topic.clone(), tx.clone());
    // New subscribers might miss initial state
}
```

### Improved Design with Snapshot Support

Here's how we can enhance the implementation to handle snapshots properly:

```rust
/// Enhanced application state with snapshot support
struct ImprovedAppState {
    // For real-time updates
    topics: RwLock<HashMap<Topic<'static>, broadcast::Sender<Bytes>>>,
    // For current market state
    snapshots: RwLock<HashMap<Topic<'static>, watch::Sender<Option<Bytes>>>>,
}

/// Enhanced subscription data
struct TopicSubscription {
    updates: broadcast::Receiver<Bytes>,
    snapshot: watch::Receiver<Option<Bytes>>,
}
```

#### Using watch::Sender for Snapshots

The `watch` channel is ideal for snapshots because:

1. Always maintains latest value
2. New subscribers automatically get current state
3. Memory efficient (only stores latest value)

Implementation example:

```rust
impl ImprovedAppState {
    fn subscribe(&self, topic: &Topic<'static>) -> Result<TopicSubscription, Error> {
        let topics = self.topics.read().unwrap();
        let snapshots = self.snapshots.read().unwrap();

        let updates = topics.get(topic)
            .ok_or(Error::TopicNotFound)?
            .subscribe();

        let snapshot = snapshots.get(topic)
            .ok_or(Error::TopicNotFound)?
            .subscribe();

        Ok(TopicSubscription { updates, snapshot })
    }

    fn update_snapshot(&self, topic: &Topic<'static>, data: Bytes) -> Result<(), Error> {
        let snapshots = self.snapshots.write().unwrap();
        if let Some(sender) = snapshots.get(topic) {
            sender.send(Some(data))?;
        }
        Ok(())
    }
}

/// Enhanced client handler
async fn handle_client_subscription(
    state: &ImprovedAppState,
    topic: Topic<'static>,
) -> Result<(), Error> {
    let TopicSubscription { mut updates, mut snapshot } = state.subscribe(&topic)?;

    // First, get current state
    if let Some(current_state) = *snapshot.borrow() {
        // Send snapshot to client
        send_to_client(current_state).await?;
    }

    // Then listen for updates
    while let Ok(update) = updates.recv().await {
        send_to_client(update).await?;
    }

    Ok(())
}
```

#### Snapshot Synchronization

To ensure data consistency, we need to handle the gap between snapshot and updates:

```rust
struct MarketDataSequence {
    sequence: u64,
    data: Bytes,
}

async fn synchronized_subscription(
    state: &ImprovedAppState,
    topic: Topic<'static>,
) -> Result<(), Error> {
    let TopicSubscription { mut updates, mut snapshot } = state.subscribe(&topic)?;

    // Get initial sequence from snapshot
    let initial_seq = snapshot.borrow().as_ref()
        .and_then(|data| parse_sequence(data))
        .ok_or(Error::NoSnapshot)?;

    // Buffer updates while processing snapshot
    let mut buffer = Vec::new();

    loop {
        match updates.recv().await? {
            update if parse_sequence(&update) <= initial_seq => {
                // Discard updates older than snapshot
                continue;
            }
            update => {
                buffer.push(update);
                break;
            }
        }
    }

    // Process buffered updates in sequence
    for update in buffer {
        send_to_client(update).await?;
    }

    // Continue with real-time updates
    while let Ok(update) = updates.recv().await {
        send_to_client(update).await?;
    }

    Ok(())
}
```

### Performance Implications

The enhanced design with snapshot support has these characteristics:

1. Memory Usage:

   - broadcast channels: O(N) where N is buffer size
   - watch channels: O(1) per topic
   - Total: O(T \* (N + 1)) where T is topic count

2. CPU Usage:

   - Snapshot updates: O(1)
   - Real-time updates: O(S) where S is subscriber count
   - Snapshot retrieval: O(1)

3. Latency:
   - First message: Higher (snapshot + sync)
   - Subsequent messages: Same as before

### Performance Optimizations

1. Memory Management:

   - Zero-copy message handling with `Bytes`
   - `Cow<str>` for string data
   - Bounded broadcast channels
   - Explicit drop of unused resources

2. Lock Optimization:

   - Minimal lock scope
   - Read locks preferred over write locks
   - Lock-free message broadcasting

3. Task Management:

   - Independent client tasks
   - Dedicated upstream task
   - Efficient task cancellation

4. Resource Cleanup:
   - Automatic topic cleanup
   - Connection resource cleanup
   - Memory leak prevention
