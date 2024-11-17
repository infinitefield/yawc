// Required imports for axum web framework, futures, and websocket operations
use axum::{response::IntoResponse, routing::get, Router};
use futures::SinkExt;
use yawc::frame::OpCode;

// Main entry point using tokio async runtime
#[tokio::main]
async fn main() {
    // Create a new router that handles websocket connections at the root path
    let app = Router::new().route("/", get(ws_handler));

    // Bind TCP listener to all interfaces on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// Handler function for processing individual websocket client connections
async fn handle_client(fut: yawc::UpgradeFut) -> yawc::Result<()> {
    // Wait for the websocket connection to be established
    let mut ws = fut.await?;

    println!("Client accepted");

    // Continuously process incoming websocket frames
    loop {
        let frame = ws.next_frame().await?;
        match frame.opcode {
            // For text or binary frames, echo them back to the client
            OpCode::Text | OpCode::Binary => {
                ws.send(frame).await?;
            }
            // Ignore other types of frames
            _ => {}
        }
    }
}

// Handler for upgrading HTTP connections to websocket connections
async fn ws_handler(ws: yawc::IncomingUpgrade) -> impl IntoResponse {
    // Configure websocket options with best compression
    let options = yawc::Options::default().with_compression_level(yawc::CompressionLevel::best());
    // Upgrade the connection to websocket protocol
    let (response, fut) = ws.upgrade(options).unwrap();
    // Spawn a new task to handle the websocket connection
    tokio::task::spawn(async move {
        if let Err(e) = handle_client(fut).await {
            eprintln!("Error in websocket connection: {}", e);
        }
    });

    response
}
