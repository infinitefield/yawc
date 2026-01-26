//! Example demonstrating streaming WebSocket data directly to disk using `split_stream`.
//!
//! This example shows how to use the low-level `split_stream` API to process individual
//! fragments as they arrive, without waiting for the complete message to be reassembled.
//!
//! This is useful for:
//! - Streaming large files to disk without loading them entirely in memory
//! - Processing data incrementally as it arrives
//! - Implementing custom backpressure and flow control
//! - Real-time data processing pipelines
//!
//! **Warning**: `split_stream()` is an unsafe, low-level API. For most use cases,
//! prefer the high-level `futures::StreamExt::split()` which handles protocol details
//! automatically.

use futures::{SinkExt, StreamExt};
use std::fs::File;
use std::io::Write;
use yawc::{Frame, OpCode, Options, WebSocket};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    simple_logger::init_with_level(log::Level::Info).expect("Failed to initialize logger");

    // Start server
    let server_handle = tokio::spawn(run_server());

    // Wait for server to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Connect to server
    let ws = WebSocket::connect("ws://localhost:9004".parse()?)
        .with_options(Options::default().with_high_compression())
        .await?;
    let mut stream = ws.into_streaming();

    log::info!("Connected to streaming server");

    // Open file for writing
    let mut file = File::create("received_stream.txt")?;
    log::info!("Created output file: received_stream.txt");

    let mut total_bytes = 0;
    let mut fragment_count = 0;

    log::info!("Waiting for fragmented message...");

    // Process fragments as they arrive
    loop {
        // Use the convenience method instead of manual polling
        let frame = match stream.next().await {
            Some(frame) => frame,
            None => break,
        };

        let (opcode, is_fin, payload) = frame.into_parts();

        fragment_count += 1;

        match opcode {
            OpCode::Text | OpCode::Binary => {
                log::info!(
                    "Received fragment {}: {} bytes (FIN={})",
                    fragment_count,
                    payload.len(),
                    is_fin
                );

                // Write directly to disk without accumulating in memory
                file.write_all(&payload[..])?;
                total_bytes += payload.len();

                if is_fin {
                    log::info!("Message complete");
                    break;
                }
            }
            OpCode::Continuation => {
                log::info!(
                    "Received fragment {}: {} bytes (FIN={})",
                    fragment_count,
                    payload.len(),
                    is_fin,
                );

                // Write directly to disk
                file.write_all(&payload[..])?;
                total_bytes += payload.len();

                if is_fin {
                    log::info!("Message complete");
                    break;
                }
            }
            _ => {}
        }
    }

    file.flush()?;
    log::info!(
        "Streaming complete: {} fragments, {} total bytes written to disk",
        fragment_count,
        total_bytes
    );

    server_handle.abort();

    Ok(())
}

/// Server that sends a large message as fragments
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
        let ws = fut.await?;
        let mut stream = ws.into_streaming();

        log::info!("[Server] Ssending fragmented message");

        // Generate large text to send in fragments
        let large_text = generate_large_text();
        let fragment_size = 4096;
        let total_chunks = large_text.len().div_ceil(fragment_size);

        log::info!("[Server] Sending {} fragments", total_chunks);

        // Send fragments one by one using the high-level API
        let mut offset = 0;
        let mut chunk_index = 0;

        while offset < large_text.len() {
            let end = std::cmp::min(offset + fragment_size, large_text.len());
            let chunk = large_text.as_bytes()[offset..end].to_vec();

            let is_first = chunk_index == 0;
            let is_last = end >= large_text.len();

            let frame = if is_first {
                Frame::text(chunk).with_fin(is_last)
            } else if is_last {
                Frame::continuation(chunk)
            } else {
                Frame::continuation(chunk).with_fin(false)
            };

            stream.send(frame).await?;

            // Simulate network delay
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

            offset = end;
            chunk_index += 1;
        }

        log::info!("[Server] All fragments sent");

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

    let listener = TcpListener::bind("0.0.0.0:9004").await?;

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

/// Generate a large text document
fn generate_large_text() -> String {
    let paragraph = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod \
        tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis \
        nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis \
        aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat \
        nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui \
        officia deserunt mollit anim id est laborum. ";

    paragraph.repeat(200)
}
