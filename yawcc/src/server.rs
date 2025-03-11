use clap::Args;
use futures::{SinkExt, StreamExt};
use http_body_util::Empty;
use hyper::{
    body::{Bytes, Incoming},
    service::service_fn,
    Request, Response,
};
use hyper_util::{rt::TokioExecutor, server::conn::auto::Builder};
use tokio::{
    net::{TcpListener, TcpStream},
    runtime,
};
use yawc::{frame::OpCode, CompressionLevel, Options, WebSocket, WebSocketError};

/// Command line arguments for the WebSocket server.
///
/// This struct defines the configuration options for the WebSocket server,
/// including network binding parameters and endpoint path.
#[derive(Args)]
#[command(alias = "s")]
pub struct Cmd {
    /// Network address and port to listen on.
    ///
    /// Specifies the IP address and port where the WebSocket server will accept connections.
    /// Format: '<ip_address>:<port>' (e.g. '0.0.0.0:9090' for all interfaces, '127.0.0.1:9090' for localhost only)
    #[arg(short, long, default_value = "127.0.0.1:9090")]
    listen: String,

    /// URI path to serve the WebSocket endpoint.
    ///
    /// Defines the path component of the WebSocket URL where clients should connect.
    /// Example: if set to "/ws", clients would connect to "ws://<host>:<port>/ws"
    #[arg(short, long, default_value = "/")]
    path: String,
}

pub fn run(cmd: Cmd) -> anyhow::Result<()> {
    let runtime = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    runtime.block_on(run_async(cmd))
}

async fn run_async(cmd: Cmd) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&cmd.listen).await?;
    println!(
        "WebSocket server listening on: ws://{}{}",
        cmd.listen, cmd.path
    );

    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            res = listener.accept() => {
                let (stream, _) = res?;
                tokio::spawn(handle_connection(stream, cmd.path.clone()));
            }
            _ = &mut ctrl_c => {
                break Ok(());
            }
        }
    }
}

async fn handle_connection(stream: TcpStream, accepted_path: String) {
    let io = hyper_util::rt::TokioIo::new(stream);
    let builder = Builder::new(TokioExecutor::new());
    if let Err(error) = builder
        .serve_connection_with_upgrades(
            io,
            service_fn(|req| handle_upgrade(req, accepted_path.clone())),
        )
        .await
    {
        println!("error upgrading websocket connection: {error:?}");
    }
}

async fn handle_upgrade(
    mut req: Request<Incoming>,
    accepted_path: String,
) -> anyhow::Result<Response<Empty<Bytes>>> {
    let path = req.uri().path();
    if !path.starts_with(&accepted_path) {
        return Err(anyhow::anyhow!("invalid url {path}"));
    }

    let (response, fut) = WebSocket::upgrade_with_options(
        &mut req,
        Options::default()
            .with_utf8()
            .with_compression_level(CompressionLevel::best()),
    )?;

    tokio::task::spawn(async move {
        if let Err(e) = handle_client(fut).await {
            eprintln!("websocket connection: {}", e);
        }
    });

    Ok(response)
}

async fn handle_client(fut: yawc::UpgradeFut) -> yawc::Result<()> {
    let mut ws = fut.await?;

    loop {
        let frame = ws.next().await.ok_or(WebSocketError::ConnectionClosed)?;
        match frame.opcode {
            OpCode::Close => break,
            OpCode::Text | OpCode::Binary => {
                ws.send(frame).await?;
            }
            _ => {}
        }
    }

    Ok(())
}
