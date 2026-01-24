//! Example demonstrating how to send large data as fragmented WebSocket messages.
//!
//! Message fragmentation allows you to:
//! - Send large messages in smaller chunks without loading everything into memory
//! - Start sending data before knowing the total size
//! - Implement backpressure and flow control
//!
//! According to RFC 6455, a fragmented message consists of:
//! 1. An initial frame with FIN=0 and an opcode (Text or Binary)
//! 2. Zero or more continuation frames with FIN=0 and opcode=Continuation
//! 3. A final continuation frame with FIN=1 and opcode=Continuation

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use yawc::{Frame, OpCode, Options, WebSocket};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    simple_logger::init_with_level(log::Level::Info).expect("Failed to initialize logger");

    // Start echo server
    let server_handle = tokio::spawn(run_server());

    // Wait for server to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Connect to server
    let client = WebSocket::connect("ws://localhost:9003".parse()?)
        .with_options(
            Options::default()
                .with_high_compression()
                .with_max_payload_write_size(1024),
        )
        .await?;

    log::info!("Streaming large text in fragments");

    // Generate large text (e.g., a novel or large document)
    let large_text = generate_large_text();
    let total_size = large_text.len();
    let fragment_size = 4096; // 4KB chunks

    log::info!("Total data size: {} bytes", total_size);
    log::info!("Fragment size: {} bytes", fragment_size);
    log::info!(
        "Number of fragments: {}",
        total_size.div_ceil(fragment_size)
    );

    // Split the WebSocket for concurrent read/write
    let (mut write, mut read) = client.split();

    // Spawn a task to receive the echo
    let read_task = tokio::spawn(async move {
        if let Some(frame) = read.next().await {
            log::info!(
                "Received reassembled message: {} bytes",
                frame.payload().len()
            );
            log::info!(
                "First 100 chars: {}",
                std::str::from_utf8(&frame.payload()[..100]).unwrap()
            );
        }
    });

    // Send data in fragments from the main task
    let chunks: Vec<&str> = large_text
        .as_bytes()
        .chunks(fragment_size)
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let is_first = i == 0;
        let is_last = i == chunks.len() - 1;

        let frame = if is_first {
            // First fragment: Text opcode, FIN=0
            Frame::text(chunk.to_string()).with_fin(is_last)
        } else if is_last {
            // Last fragment: Continuation opcode, FIN=1 (default)
            Frame::continuation(chunk.to_string())
        } else {
            // Middle fragments: Continuation opcode, FIN=0
            Frame::continuation(chunk.to_string()).with_fin(false)
        };

        log::info!(
            "Sending fragment {}/{}: {} bytes (FIN={})",
            i + 1,
            chunks.len(),
            chunk.len(),
            frame.is_fin()
        );

        write.send(frame).await.context("client")?;
    }

    log::info!("Sent all fragments");

    // Wait for the echo to be received
    read_task.await.unwrap();

    // Close connection
    write
        .send(Frame::close(yawc::close::CloseCode::Normal, b"Done"))
        .await?;

    server_handle.abort();
    Ok(())
}

/// Generate a large text document for demonstration.
fn generate_large_text() -> String {
    let paragraph = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod \
        tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis \
        nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis \
        aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat \
        nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui \
        officia deserunt mollit anim id est laborum. ";

    paragraph.repeat(200)
}

/// Simple echo server that automatically reassembles fragmented messages.
async fn run_server() -> yawc::Result<()> {
    use http_body_util::Empty;
    use hyper::{
        body::{Bytes, Incoming},
        server::conn::http1,
        service::service_fn,
        Request, Response,
    };
    use tokio::net::TcpListener;

    async fn handle_client(fut: yawc::UpgradeFut) -> yawc::Result<()> {
        let mut ws = fut.await?;

        // Echo server: WebSocket automatically reassembles fragments
        // We receive complete messages, not individual fragments
        while let Some(frame) = ws.next().await {
            match frame.opcode() {
                OpCode::Text | OpCode::Binary => {
                    log::info!(
                        "[Server] Received complete message: {} bytes",
                        frame.payload().len()
                    );
                    ws.send(frame).await?;
                }
                _ => {}
            }
        }

        log::warn!("User closed connection");

        Ok(())
    }

    async fn server_upgrade(mut req: Request<Incoming>) -> yawc::Result<Response<Empty<Bytes>>> {
        let (response, fut) =
            WebSocket::upgrade_with_options(&mut req, Options::default().with_high_compression())?;

        tokio::task::spawn(async move {
            if let Err(e) = handle_client(fut).await {
                log::error!("Error in websocket connection: {}", e);
            }
        });

        Ok(response)
    }

    let listener = TcpListener::bind("0.0.0.0:9003").await?;

    loop {
        let (stream, _) = listener.accept().await?;

        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let conn_fut = http1::Builder::new()
                .serve_connection(io, service_fn(server_upgrade))
                .with_upgrades();
            if let Err(e) = conn_fut.await {
                log::error!("Error serving connection: {:?}", e);
            }
        });
    }
}
