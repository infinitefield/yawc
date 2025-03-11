use std::{sync::Arc, time::Duration};

use clap::Args;
use futures::{SinkExt, StreamExt};
use rustls::pki_types::TrustAnchor;
use rustyline::ExternalPrinter;
use tokio::{
    runtime,
    sync::mpsc::{unbounded_channel, UnboundedReceiver},
    time::timeout,
};
use tokio_rustls::TlsConnector;
use url::Url;
use yawc::{
    frame::{FrameView, OpCode},
    WebSocket,
};

/// Command to connect and interact with a WebSocket server.
///
/// This command establishes a WebSocket client connection to a server and allows sending
/// messages and receiving responses interactively. It supports both plaintext WebSocket (ws://)
/// and secure WebSocket (wss://) connections.
#[derive(Args)]
#[command(alias = "c")]
pub struct Cmd {
    /// Maximum duration to wait when establishing the connection.
    /// Accepts human-readable formats like "5s", "1m", "500ms".
    #[arg(short, long, value_parser = humantime::parse_duration, default_value = "5s")]
    timeout: Duration,

    /// When enabled, validates and pretty-prints received messages as JSON.
    /// Invalid JSON messages will result in an error.
    #[arg(long)]
    input_as_json: bool,

    /// The WebSocket URL to connect to (ws:// or wss://)
    url: Url,
}

pub fn run(cmd: Cmd) -> anyhow::Result<()> {
    let history_path = home::home_dir()
        .ok_or(anyhow::anyhow!("unable to determine home path"))?
        .join(".yawc_history");

    // Handle user input with history
    let mut rl = rustyline::DefaultEditor::with_config(
        rustyline::Config::builder()
            .auto_add_history(true)
            .completion_type(rustyline::CompletionType::List)
            .max_history_size(1000)
            .unwrap()
            .build(),
    )?;
    // ignore the error
    let _ = rl.load_history(&history_path);
    // external printer
    let printer = rl.create_external_printer().unwrap();

    let runtime = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = runtime.enter();

    let (tx, rx) = unbounded_channel();

    let url = cmd.url.clone();
    let ws = runtime.block_on(timeout(
        cmd.timeout,
        WebSocket::connect(url, Some(tls_connector())),
    ))??;

    println!("> Connected to {}", cmd.url);

    // Spawn reading task
    let opts = Opts {
        input_as_json: cmd.input_as_json,
    };

    runtime.spawn_blocking(move || loop {
        let readline = rl.readline("> ");
        match readline {
            Ok(mut line) => {
                let _ = rl.add_history_entry(line.as_str());
                // commented line
                if let Some(pos) = line.rfind("//") {
                    let _ = line.split_off(pos);
                }

                if tx.send(line).is_err() {
                    break;
                }
            }
            Err(_) => {
                rl.save_history(&history_path).expect("save history");
                break;
            }
        }
    });
    runtime.block_on(handle_websocket(ws, rx, printer, opts));

    runtime.shutdown_background();

    Ok(())
}

struct Opts {
    input_as_json: bool,
}

async fn handle_websocket(
    mut ws: WebSocket,
    mut rx: UnboundedReceiver<String>,
    mut printer: impl ExternalPrinter,
    opts: Opts,
) {
    loop {
        tokio::select! {
            msg = rx.recv() => {
                if msg.is_none() {
                    break;
                }

                let msg = msg.unwrap();
                if let Err(err) = ws.send(FrameView::text(msg)).await {
                    let _ = printer.print(format!("unable to write: {}", err));
                }
            }
            frame = ws.next() => {
                if frame.is_none() {
                    let _ = printer.print(format!("<Disconnected>"));
                    break;
                }

                let frame = frame.unwrap();
                match frame.opcode {
                    OpCode::Text => {
                        let msg = std::str::from_utf8(&frame.payload).expect("utf8");
                        if opts.input_as_json {
                            match serde_json::from_str::<serde_json::Value>(msg) {
                                Ok(ok) => {
                                    let _ = printer.print(format!("{:#}", ok));
                                }
                                Err(err) => {
                                    let _ = printer.print(format!("parsing json: {}", err));
                                }
                            }
                        } else {
                            let _ = printer.print(format!("{msg}"));
                        }
                    }
                    _ => {
                        let _ = printer.print(format!("<{:?}>", frame.opcode));
                    }
                }
            }
        }
    }

    let _ = ws.close().await;
}

fn tls_connector() -> TlsConnector {
    let mut root_cert_store = rustls::RootCertStore::empty();
    root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| TrustAnchor {
        subject: ta.subject.clone(),
        subject_public_key_info: ta.subject_public_key_info.clone(),
        name_constraints: ta.name_constraints.clone(),
    }));
    // config.dangerous()... to ignore the cert verification

    TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(root_cert_store)
            .with_no_client_auth(),
    ))
}
