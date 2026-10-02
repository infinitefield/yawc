use std::{env, net::SocketAddr};

use anyhow::{bail, ensure, Result};
use bytes::Bytes;
use fastwebsockets::{Frame as FastFrame, Payload as FastPayload};
use futures::{FutureExt, SinkExt, StreamExt};
use http_body_util::Empty;
use hyper::{body::Incoming, server::conn::http1, service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, UnixListener},
};
use tokio_tungstenite::tungstenite::Message;
use yawc::{Frame, OpCode, Options, WebSocket};

#[path = "server/telemetry.rs"]
mod telemetry;
use telemetry::TelemetryState;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Workload {
    Echo,
    Telemetry,
}

#[derive(Clone, Copy)]
enum Library {
    Yawc,
    YawcBatched,
    YawcBuffered(usize),
    Fastwebsockets,
    Tungstenite,
    TungsteniteBatched,
}

fn yawc_reply(frame: Frame, workload: Workload, state: &mut TelemetryState) -> Result<Frame> {
    if workload == Workload::Echo {
        return Ok(frame);
    }
    ensure!(
        frame.opcode() == OpCode::Binary,
        "telemetry requires binary frames"
    );
    Ok(Frame::binary(state.acknowledge(frame.payload())?.to_vec()))
}

async fn upgrade(
    mut req: Request<Incoming>,
    library: Library,
    workload: Workload,
) -> Result<Response<Empty<Bytes>>> {
    match library {
        Library::Yawc | Library::YawcBatched | Library::YawcBuffered(_) => {
            let mut options = Options::default().with_utf8().without_compression();
            if let Library::YawcBuffered(capacity) = library {
                options = options
                    .with_read_buffer_capacity(capacity)
                    .with_backpressure_boundary(64 * 1024);
            }
            let (response, future) = WebSocket::upgrade_with_options(&mut req, options)?;
            tokio::spawn(async move {
                let mut ws = future.await?;
                let mut state = TelemetryState::default();
                loop {
                    let mut frame = ws.next_frame().await?;
                    if matches!(library, Library::YawcBatched | Library::YawcBuffered(_)) {
                        for index in 0..32 {
                            match frame.opcode() {
                                OpCode::Text | OpCode::Binary => {
                                    ws.feed(yawc_reply(frame, workload, &mut state)?).await?
                                }
                                OpCode::Close => {
                                    ws.close().await?;
                                    return Ok::<_, anyhow::Error>(());
                                }
                                _ => {}
                            }
                            if index == 31 {
                                break;
                            }
                            match ws.next_frame().now_or_never() {
                                Some(next) => frame = next?,
                                None => break,
                            }
                        }
                        ws.flush().await?;
                        continue;
                    }
                    match frame.opcode() {
                        OpCode::Text | OpCode::Binary => {
                            ws.send(yawc_reply(frame, workload, &mut state)?).await?
                        }
                        OpCode::Close => break,
                        _ => {}
                    }
                }
                Ok::<_, anyhow::Error>(())
            });
            Ok(response)
        }
        Library::Fastwebsockets => {
            let (response, future) = fastwebsockets::upgrade::upgrade(&mut req)?;
            tokio::spawn(async move {
                let mut ws = fastwebsockets::FragmentCollector::new(future.await?);
                let mut state = TelemetryState::default();
                loop {
                    let frame = ws.read_frame().await?;
                    match frame.opcode {
                        fastwebsockets::OpCode::Text | fastwebsockets::OpCode::Binary => {
                            if workload == Workload::Telemetry {
                                ensure!(
                                    frame.opcode == fastwebsockets::OpCode::Binary,
                                    "telemetry requires binary frames"
                                );
                                let reply = state.acknowledge(&frame.payload)?;
                                ws.write_frame(FastFrame::binary(FastPayload::Owned(
                                    reply.to_vec(),
                                )))
                                .await?;
                            } else {
                                ws.write_frame(frame).await?;
                            }
                        }
                        fastwebsockets::OpCode::Close => break,
                        _ => {}
                    }
                }
                Ok::<_, anyhow::Error>(())
            });
            Ok(response)
        }
        Library::Tungstenite | Library::TungsteniteBatched => {
            unreachable!("tungstenite handles its own upgrade")
        }
    }
}

async fn connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    library: Library,
    workload: Workload,
) -> Result<()> {
    if matches!(library, Library::Tungstenite | Library::TungsteniteBatched) {
        let mut ws = tokio_tungstenite::accept_async(stream).await?;
        let mut state = TelemetryState::default();
        while let Some(message) = ws.next().await {
            let mut message = message?;
            if matches!(library, Library::TungsteniteBatched) {
                for index in 0..32 {
                    if message.is_close() {
                        ws.flush().await?;
                        return Ok(());
                    }
                    if message.is_text() || message.is_binary() {
                        let reply = if workload == Workload::Telemetry {
                            ensure!(message.is_binary(), "telemetry requires binary messages");
                            Message::binary(state.acknowledge(&message.into_data())?.to_vec())
                        } else {
                            message
                        };
                        ws.feed(reply).await?;
                    }
                    if index == 31 {
                        break;
                    }
                    match ws.next().now_or_never() {
                        Some(Some(next)) => message = next?,
                        Some(None) => return Ok(()),
                        None => break,
                    }
                }
                ws.flush().await?;
                continue;
            }
            if message.is_close() {
                break;
            }
            if message.is_text() || message.is_binary() {
                let reply = if workload == Workload::Telemetry {
                    ensure!(message.is_binary(), "telemetry requires binary messages");
                    Message::binary(state.acknowledge(&message.into_data())?.to_vec())
                } else {
                    message
                };
                ws.send(reply).await?;
            }
        }
    } else {
        http1::Builder::new()
            .serve_connection(
                TokioIo::new(stream),
                service_fn(move |req| upgrade(req, library, workload)),
            )
            .with_upgrades()
            .await?;
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let library = match args.next().as_deref() {
        Some("yawc") => Library::Yawc,
        Some("yawc-batched") => Library::YawcBatched,
        Some("yawc-buffered") => Library::YawcBuffered(64 * 1024),
        Some("yawc-buffered-128k") => Library::YawcBuffered(128 * 1024),
        Some("yawc-buffered-512k") => Library::YawcBuffered(512 * 1024),
        Some("fastwebsockets") => Library::Fastwebsockets,
        Some("tokio-tungstenite") => Library::Tungstenite,
        Some("tokio-tungstenite-batched") => Library::TungsteniteBatched,
        _ => bail!("expected a library name"),
    };
    let address = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("expected bind address"))?;
    let workload = match args.next().as_deref() {
        None | Some("echo") => Workload::Echo,
        Some("telemetry") => Workload::Telemetry,
        _ => bail!("expected echo or telemetry workload"),
    };
    ensure!(args.next().is_none(), "unexpected extra argument");
    if let Some(path) = address.strip_prefix("unix:") {
        let listener = UnixListener::bind(path)?;
        println!("READY unix");
        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(connection(stream, library, workload));
        }
    }
    let address: SocketAddr = address.parse()?;
    anyhow::ensure!(
        !address.ip().is_unspecified(),
        "an explicit bind address is required"
    );
    let listener = TcpListener::bind(address).await?;
    println!("READY {}", listener.local_addr()?.port());
    loop {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true)?;
        tokio::spawn(connection(stream, library, workload));
    }
}
