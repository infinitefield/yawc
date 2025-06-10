use std::time::Duration;

use clap::Args;
use futures::{SinkExt, StreamExt};
use rustyline::ExternalPrinter;
use tokio::{
    runtime,
    sync::mpsc::{unbounded_channel, UnboundedReceiver},
    time::timeout,
};
use url::Url;
use yawc::{
    frame::{FrameView, OpCode},
    CompressionLevel, HttpRequest, HttpRequestBuilder, Options, WebSocket,
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

    /// Includes the timestamp for each message.
    #[arg(long)]
    include_time: bool,

    /// Custom headers to send to the server in "Key: Value" format
    /// For example: --header "Authorization: Bearer token123"
    #[arg(short = 'H', long = "header", value_name = "Headers")]
    headers: Vec<String>,

    /// When enabled, validates and pretty-prints received messages as JSON.
    /// Invalid JSON messages will result in an error.
    #[arg(long)]
    input_as_json: bool,

    /// The WebSocket URL to connect to (ws:// or wss://)
    url: Url,
}

fn build_request(headers: &[String]) -> anyhow::Result<HttpRequestBuilder> {
    let mut builder = HttpRequest::builder();
    for header in headers.iter().map(|item| item.split_once(':')) {
        // TODO: handle split error
        let Some((key, value)) = header else { continue };
        let key = key.trim();
        let value = value.trim_start(); // maybe the user wants to add some space after, idk
        builder = builder.header(key, value);
    }

    Ok(builder)
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

    let request_builder = build_request(&cmd.headers)?;

    let runtime = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = runtime.enter();

    let (tx, rx) = unbounded_channel();

    let url = cmd.url.clone();
    let ws = runtime.block_on(timeout(
        cmd.timeout,
        WebSocket::connect(url)
            .with_request(request_builder)
            .with_options(Options::default().with_compression_level(CompressionLevel::best())),
    ))??;

    println!("> Connected to {}", cmd.url);

    // Spawn reading task
    let opts = Opts {
        input_as_json: cmd.input_as_json,
        include_time: cmd.include_time,
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
    include_time: bool,
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
                    let _ = printer.print("<Disconnected>".to_string());
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
                            if opts.include_time {
                                let time = chrono::Local::now().format("%H:%M:%S.%9f");
                                let _ = printer.print(format!("{} > {:}", time, msg));
                            } else {
                                let _ = printer.print(msg.to_string());
                            }
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
