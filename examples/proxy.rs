//! Binance proxy sending compressed data to client and serves order book updates from Binance

//! This proxy acts as an intermediary server that:
//! - Connects to the Binance WebSocket API to receive BTC order book updates
//! - Maintains a pool of connected WebSocket clients
//! - Broadcasts updates to all connected clients using compression
//! - Handles client connections/disconnections and WebSocket handshakes
//! - Provides a convenient way to fan-out data to multiple consumerss

use std::{
    collections::BTreeMap,
    sync::{atomic::AtomicU64, Arc},
};

use anyhow::Result;
use futures::{
    lock::Mutex,
    stream::SplitSink,
    {SinkExt, StreamExt},
};
use http_body_util::Empty;
use hyper::{
    body::{Bytes, Incoming},
    server::conn::http1,
    service::service_fn,
    {Request, Response},
};
use tokio::net::TcpListener;
use yawc::{CompressionLevel, FrameView, OpCode, WebSocket};

// Type alias for storing connected clients
// Uses BTreeMap for ordered storage of client IDs -> WebSocket sinks
type Clients = Mutex<BTreeMap<u64, SplitSink<WebSocket, FrameView>>>;

// Atomic counter for generating unique client IDs
static CLIENT_ID: AtomicU64 = AtomicU64::new(0);

// =============== server functions ================

// Handles an individual WebSocket client connection
async fn handle_client(clients: Arc<Clients>, ws: WebSocket) -> yawc::Result<()> {
    // Split WebSocket into sink (for sending) and stream (for receiving)
    let (sink, mut stream) = ws.split();

    // Generate unique client ID and store sink in shared clients map
    let client_id = CLIENT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    clients.lock().await.insert(client_id, sink);

    // Listen for incoming frames until a Close frame is received
    while let Some(frame) = stream.next().await {
        if let OpCode::Close = frame.opcode {
            break;
        }
    }

    Ok(())
}

// Handles upgrading HTTP connection to WebSocket
async fn server_upgrade(
    clients: Arc<Clients>,
    mut req: Request<Incoming>,
) -> yawc::Result<Response<Empty<Bytes>>> {
    // Configure WebSocket upgrade options, enabling compression with the highest level
    let options = yawc::Options::default().with_compression_level(CompressionLevel::best());
    let (response, fut) = WebSocket::upgrade_with_options(&mut req, options)?;

    // Spawn a task to manage the WebSocket connection for the client
    tokio::task::spawn(async move {
        if let Ok(ws) = fut.await {
            // Call `handle_client` to process the client's WebSocket connection
            if let Err(e) = handle_client(clients, ws).await {
                log::error!("Error in WebSocket connection: {}", e);
            }
        }
    });

    Ok(response)
}

// Main server function that listens for incoming connections
async fn server(clients: Arc<Clients>) -> yawc::Result<()> {
    // Bind server to all interfaces on port 3001
    let addr = "0.0.0.0:3001";
    let listener = TcpListener::bind(addr).await?;
    log::info!("Server started, listening on {addr}");

    // Accept incoming connections indefinitely
    loop {
        let (stream, _) = listener.accept().await?;
        log::info!("Client connected");

        // Clone clients reference for the new connection
        let clients = Arc::clone(&clients);
        tokio::spawn(async move {
            // Create service function for handling HTTP upgrade
            let service_fn = service_fn(|req| {
                let clients = Arc::clone(&clients);
                server_upgrade(clients, req)
            });

            // Set up HTTP connection with upgrade support
            let io = hyper_util::rt::TokioIo::new(stream);
            let conn_fut = http1::Builder::new()
                .serve_connection(io, service_fn)
                .with_upgrades();
            if let Err(e) = conn_fut.await {
                log::error!("an error occurred: {:?}", e);
            }
        });
    }
}

// =============== client functions ================

// Client function that connects to external WebSocket and broadcasts messages
async fn client(clients: Arc<Clients>) -> Result<()> {
    loop {
        // Connect to Binance WebSocket API
        let mut ws = WebSocket::connect(
            "wss://stream.binance.com:9443/ws/btcusdt@depth"
                .parse()
                .unwrap(),
        )
        .await?;

        // Process incoming messages
        while let Some(view) = ws.next().await {
            match view.opcode {
                OpCode::Text => {
                    // Track disconnected clients
                    let mut disconnected = vec![];
                    let mut client_list = clients.lock().await;

                    // Broadcast message to all connected clients
                    for (key, client) in client_list.iter_mut() {
                        if let Err(err) = client.send(view.clone()).await {
                            log::error!("client: {}", err);
                            disconnected.push(*key);
                        }
                    }

                    // Remove disconnected clients
                    for key in disconnected {
                        client_list.remove(&key);
                    }
                }
                OpCode::Close => {
                    break;
                }
                _ => {}
            }
        }
    }
}

// ================ main ===================

#[tokio::main]
async fn main() -> yawc::Result<()> {
    // Initialize logging
    simple_logger::init_with_level(log::Level::Debug).expect("log");

    // Create shared clients state
    let clients = Arc::new(Mutex::new(BTreeMap::default()));

    // Spawn client task and run server
    tokio::spawn(client(Arc::clone(&clients)));
    let _ = server(clients).await;

    Ok(())
}
