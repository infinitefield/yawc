use std::net::SocketAddr;

use axum::{response::IntoResponse, routing::get, Router};
use tokio::net::TcpListener;
use yawc::{frame::OpCode, CompressionLevel, HttpStream, IncomingUpgrade, Options, WebSocket};

async fn ws_handler(ws: IncomingUpgrade) -> impl IntoResponse {
    // Configure options based on what Autobahn tests need
    let options = get_server_options();

    let (response, fut) = ws.upgrade(options).unwrap();

    tokio::spawn(async move {
        if let Ok(websocket) = fut.await {
            handle_socket(websocket).await;
        }
    });

    response
}

fn get_server_options() -> Options {
    Options::default()
        .with_utf8()
        .with_max_payload_read(100 * 1024 * 1024)
        .with_max_read_buffer(200 * 1024 * 1024)
        .with_low_latency_compression()
        .with_compression_level(CompressionLevel::none())
}

async fn handle_socket(mut ws: WebSocket<HttpStream>) {
    use futures::{SinkExt, StreamExt};

    loop {
        let msg = match ws.next().await {
            Some(msg) => msg,
            None => break,
        };

        let (opcode, _is_fin, body) = msg.into_parts();

        // Echo back the message
        match opcode {
            OpCode::Text | OpCode::Binary => {
                if let Err(e) = ws.send(yawc::Frame::from((opcode, body))).await {
                    log::error!("Error sending message: {}", e);
                    break;
                }
            }
            OpCode::Close => {
                log::debug!("Received close frame");
                break;
            }
            _ => {}
        }
    }

    log::debug!("WebSocket connection closed");
}

#[tokio::main]
async fn main() {
    // Initialize logging
    simple_logger::init_with_level(log::Level::Debug).expect("log");

    let app = Router::new().route("/", get(ws_handler));

    let addr = SocketAddr::from(([127, 0, 0, 1], 9002));
    log::info!("Autobahn test server listening on {}", addr);

    let listener = TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
