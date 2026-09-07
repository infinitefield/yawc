use std::{
    convert::Infallible,
    io::IsTerminal,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{ensure, Context};
use clap::Args;
use futures::SinkExt;
use http_body_util::Empty;
use hyper::{
    body::{Bytes, Incoming},
    service::service_fn,
    Request, Response, StatusCode,
};
use hyper_util::{rt::TokioExecutor, rt::TokioIo, server::conn::auto::Builder};
use tokio::{
    net::TcpListener,
    sync::{broadcast, mpsc},
    task::JoinSet,
    time::timeout,
};
use yawc::{
    frame::{Frame, OpCode},
    Options, WebSocket,
};

use crate::{
    session::{self, MessageArgs},
    terminal::{self, Input, Output},
};

/// Serve an echo endpoint, or manually send messages with --interactive.
#[derive(Args)]
#[command(alias = "s")]
pub struct Cmd {
    /// Address to listen on. Port 0 chooses an available port.
    #[arg(short, long, default_value = "127.0.0.1:9090")]
    listen: String,
    /// Exact endpoint path (query strings are allowed).
    #[arg(short, long, default_value = "/")]
    path: String,
    /// Display incoming messages and broadcast stdin to connected clients, instead of echoing.
    #[arg(short, long)]
    interactive: bool,
    /// Accepted subprotocol; repeat in server preference order.
    #[arg(short = 's', long = "subprotocol")]
    subprotocols: Vec<String>,
    #[command(flatten)]
    messages: MessageArgs,
}

pub fn run(cmd: Cmd) -> anyhow::Result<()> {
    ensure!(
        cmd.path.starts_with('/') && !cmd.path.contains(['?', '#']),
        "--path must be an absolute path without query or fragment"
    );
    for protocol in &cmd.subprotocols {
        // HeaderName applies HTTP token validation, which also defines subprotocol names.
        ensure!(
            !protocol.is_empty()
                && hyper::header::HeaderName::from_bytes(protocol.as_bytes()).is_ok(),
            "invalid subprotocol"
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_async(cmd))
}

async fn run_async(cmd: Cmd) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&cmd.listen).await?;
    eprintln!(
        "WebSocket server listening on: ws://{}{}",
        listener.local_addr()?,
        cmd.path
    );
    let (mut input, output) = if cmd.interactive {
        terminal::start(
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            true,
        )?
    } else {
        let (_, rx) = mpsc::channel(1);
        (rx, Output::plain())
    };
    let output = Arc::new(Mutex::new(output));
    let (outgoing, _) = broadcast::channel::<Frame>(64);
    let (upgrades, mut upgrade_rx) = mpsc::channel(64);
    let mut connections = JoinSet::new();
    let mut sessions = JoinSet::new();
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let result = loop {
        tokio::select! {
            signal = &mut ctrl_c => break signal.context("listen for Ctrl+C"),
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                let path = cmd.path.clone();
                let protocols = cmd.subprotocols.clone();
                let upgrades = upgrades.clone();
                connections.spawn(async move {
                    let builder = Builder::new(TokioExecutor::new());
                    if let Err(error) = builder.serve_connection_with_upgrades(TokioIo::new(stream), service_fn(move |req| upgrade(req, path.clone(), protocols.clone(), upgrades.clone(), peer))).await {
                        eprintln!("HTTP connection {peer}: {error}");
                    }
                });
            }
            Some((fut, peer)) = upgrade_rx.recv() => {
                let args = cmd.messages.clone();
                let output = output.clone();
                let outgoing = outgoing.subscribe();
                let interactive = cmd.interactive;
                sessions.spawn(async move {
                    if interactive { eprintln!("Client connected: {peer}"); }
                    if let Err(error) = serve(fut, outgoing, output, args, interactive).await {
                        eprintln!("Client {peer}: {error:#}");
                    }
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {},
            event = input.recv(), if cmd.interactive => {
                let frame = match event {
                    Some(Input::Line(line)) => match cmd.messages.parse(&line) { Ok(frame) => frame, Err(error) => { eprintln!("error: {error:#}"); continue; } },
                    Some(Input::Frame(frame)) => Some(frame),
                    Some(Input::Error(error)) => break Err(error.into()),
                    Some(Input::Interrupted) | None => break Ok(()),
                };
                if let Some(frame) = frame {
                    if outgoing.send(frame).is_err() { eprintln!("No connected clients"); }
                }
            }
        }
    };
    // Stop accepting and upgrading before closing the established sessions.
    drop(listener);
    connections.abort_all();
    let _ = outgoing.send(Frame::close(
        yawc::close::CloseCode::Normal,
        "server shutting down",
    ));
    let _ = timeout(Duration::from_secs(3), async {
        while sessions.join_next().await.is_some() {}
    })
    .await;
    sessions.abort_all();
    result
}

type UpgradeSender = mpsc::Sender<(yawc::UpgradeFut, std::net::SocketAddr)>;
async fn upgrade(
    mut request: Request<Incoming>,
    path: String,
    protocols: Vec<String>,
    upgrades: UpgradeSender,
    peer: std::net::SocketAddr,
) -> Result<Response<Empty<Bytes>>, Infallible> {
    let reject = |status| {
        let mut response = Response::new(Empty::new());
        *response.status_mut() = status;
        response
    };
    if request.uri().path() != path {
        return Ok(reject(StatusCode::NOT_FOUND));
    }
    let offered = request
        .headers()
        .get_all(hyper::header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect::<Vec<_>>();
    let selected = protocols.iter().find(|p| offered.contains(&p.as_str()));
    if !protocols.is_empty() && selected.is_none() {
        return Ok(reject(StatusCode::BAD_REQUEST));
    }
    match WebSocket::upgrade_with_options(
        &mut request,
        Options::default().with_utf8().with_balanced_compression(),
    ) {
        Ok((mut response, future)) => {
            if let Some(protocol) = selected {
                response.headers_mut().insert(
                    hyper::header::SEC_WEBSOCKET_PROTOCOL,
                    protocol.parse().expect("validated subprotocol"),
                );
            }
            if upgrades.send((future, peer)).await.is_err() {
                return Ok(reject(StatusCode::SERVICE_UNAVAILABLE));
            }
            Ok(response)
        }
        Err(_) => Ok(reject(StatusCode::BAD_REQUEST)),
    }
}

async fn serve(
    fut: yawc::UpgradeFut,
    mut outgoing: broadcast::Receiver<Frame>,
    output: Arc<Mutex<Output>>,
    args: MessageArgs,
    interactive: bool,
) -> anyhow::Result<()> {
    let mut ws = timeout(Duration::from_secs(5), fut)
        .await
        .context("upgrade timed out")??;
    loop {
        tokio::select! {
            received = ws.next_frame() => {
                let frame = received?;
                if interactive || frame.opcode().is_control() {
                    args.display(&frame, &mut *output.lock().map_err(|_| anyhow::anyhow!("terminal output lock failed"))?)?;
                }
                if frame.opcode() == OpCode::Close { return session::finish_peer_close(&mut ws).await; }
                if !interactive && matches!(frame.opcode(), OpCode::Text | OpCode::Binary) {
                    timeout(Duration::from_secs(5), ws.send(frame)).await.context("send timed out")??;
                }
            }
            sent = outgoing.recv() => {
                let frame = sent.context("server output queue overflowed or closed")?;
                let closing = frame.opcode() == OpCode::Close;
                timeout(Duration::from_secs(5), ws.send(frame)).await.context("send timed out")??;
                if closing {
                    return timeout(Duration::from_secs(2), async {
                        loop { if ws.next_frame().await?.opcode() == OpCode::Close { return Ok(()); } }
                    }).await.context("peer did not close")?;
                }
            }
        }
    }
}
