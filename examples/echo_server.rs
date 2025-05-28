//! A WebSocket echo server implementation using yawc and hyper.
//! This server accepts WebSocket connections and echoes back any text or binary messages it receives.

use futures::{SinkExt, StreamExt};
use http_body_util::Empty;
use hyper::{
    body::{Bytes, Incoming},
    server::conn::http1,
    service::service_fn,
    Request, Response,
};
use tokio::net::TcpListener;
use yawc::{frame::OpCode, CompressionLevel, FrameView, Options, WebSocket, WebSocketError};

pub async fn dummy(mut websocket: WebSocket) {
    loop {
        let data = "abc,def,gh,ad,fe,ga,sg,h,as,h\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n1,2,3,4,5,6,7,8,9,,,,,7\n";
        let data = data.to_string();
        let data_to_send = FrameView::text(data);

        websocket.send(data_to_send).await.ok();
    }
}

/// Handles an individual WebSocket client connection by echoing back any received messages.
///
/// # Arguments
/// * `fut` - Future that resolves to the WebSocket connection
///
/// # Returns
/// * `yawc::Result<()>` - Result indicating success or failure of the WebSocket connection handling
async fn handle_client(fut: yawc::UpgradeFut) -> yawc::Result<()> {
    let mut ws = fut.await?;

    dummy(ws).await;

    // loop {
    //     let frame = ws.next().await.ok_or(WebSocketError::ConnectionClosed)?;
    //     match frame.opcode {
    //         OpCode::Close => break,
    //         OpCode::Text | OpCode::Binary => {
    //             ws.send(frame).await?;
    //         }
    //         _ => {}
    //     }
    // }
    //

    log::debug!("Client disconnected");

    Ok(())
}

/// Upgrades an HTTP connection to a WebSocket connection with specific options.
///
/// # Arguments
/// * `req` - The HTTP request to upgrade
///
/// # Returns
/// * `yawc::Result<Response<Empty<Bytes>>>` - The HTTP response for the upgrade
async fn server_upgrade(mut req: Request<Incoming>) -> yawc::Result<Response<Empty<Bytes>>> {
    let (response, fut) = WebSocket::upgrade_with_options(
        &mut req,
        Options::default()
            .with_utf8()
            .with_max_payload_read(100 * 1024 * 1024)
            .with_max_read_buffer(200 * 1024 * 1024)
            .with_compression_level(CompressionLevel::none()),
    )?;

    tokio::task::spawn(async move {
        if let Err(e) = handle_client(fut).await {
            log::error!("Error in websocket connection: {}", e);
        }
    });

    Ok(response)
}

/// Main entry point for the WebSocket server.
///
/// Initializes logging and starts listening for WebSocket connections on port 8080.
/// Each client connection is handled in a separate task.
#[tokio::main]
async fn main() -> yawc::Result<()> {
    // Initialize logging
    simple_logger::init_with_level(log::Level::Debug).expect("log");

    let listener = TcpListener::bind("0.0.0.0:8080").await?;

    log::debug!("Listening on {}", listener.local_addr().unwrap());

    loop {
        let (stream, _) = listener.accept().await?;
        log::info!("Client connected");

        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let conn_fut = http1::Builder::new()
                .serve_connection(io, service_fn(server_upgrade))
                .with_upgrades();
            if let Err(e) = conn_fut.await {
                log::error!("An error occurred: {:?}", e);
            }
        });
    }
}
