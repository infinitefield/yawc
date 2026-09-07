use std::time::Duration;

use anyhow::{bail, ensure, Context};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Args, ValueEnum};
use futures::SinkExt;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::{timeout, Instant},
};
use yawc::{
    close::CloseCode,
    frame::{Frame, OpCode},
    WebSocket,
};

use crate::terminal::{Input, Output, Receiver};

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum BinaryFormat {
    #[default]
    Hex,
    Base64,
    Raw,
}

#[derive(Args, Clone, Default)]
pub struct MessageArgs {
    /// Pretty-print received JSON. Invalid JSON is an error.
    #[arg(long, alias = "json")]
    pub input_as_json: bool,
    /// Prefix received messages with the local time (also works with JSON).
    #[arg(long)]
    pub include_time: bool,
    /// Strip whitespace-prefixed // comments outside double-quoted strings.
    #[arg(long)]
    pub comments: bool,
    /// Enable /ping [data], /pong [data], and /close [code[, reason]].
    #[arg(long)]
    pub slash: bool,
    /// Report incoming ping and pong payloads on stderr.
    #[arg(short = 'P', long)]
    pub show_ping_pong: bool,
    /// Format received binary messages. Raw writes exact bytes without a newline.
    #[arg(long, value_enum, default_value = "hex")]
    pub binary: BinaryFormat,
}

impl MessageArgs {
    pub fn parse(&self, line: &str) -> anyhow::Result<Option<Frame>> {
        let line = if self.comments {
            strip_comment(line)
        } else {
            line
        };
        if self.comments && line.trim().is_empty() {
            return Ok(None);
        }
        if self.slash && line.starts_with('/') {
            let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let rest = rest.trim_start();
            match command {
                "/ping" | "/pong" => {
                    ensure!(rest.len() <= 125, "control frame payload exceeds 125 bytes");
                    return Ok(Some(if command == "/ping" {
                        Frame::ping(rest.to_owned())
                    } else {
                        Frame::pong(rest.to_owned())
                    }));
                }
                "/close" => {
                    let (code, reason) = rest.split_once(',').unwrap_or((rest, ""));
                    let code = if code.trim().is_empty() {
                        1000
                    } else {
                        code.trim().parse::<u16>().context("invalid close code")?
                    };
                    ensure!(
                        CloseCode::from(code).is_allowed(),
                        "close code {code} cannot be sent"
                    );
                    let reason = reason.trim_start();
                    ensure!(reason.len() <= 123, "close reason exceeds 123 bytes");
                    return Ok(Some(Frame::close(code.into(), reason)));
                }
                _ => {} // Unknown slash-prefixed text remains a literal message.
            }
        }
        Ok(Some(Frame::text(line.to_owned())))
    }

    pub fn display(&self, frame: &Frame, output: &mut Output) -> anyhow::Result<()> {
        let message = match frame.opcode() {
            OpCode::Text => {
                let text =
                    std::str::from_utf8(frame.payload()).context("invalid UTF-8 text frame")?;
                if self.input_as_json {
                    serde_json::to_string_pretty(
                        &serde_json::from_str::<serde_json::Value>(text)
                            .context("received invalid JSON")?,
                    )?
                } else {
                    text.to_owned()
                }
            }
            OpCode::Binary => match self.binary {
                BinaryFormat::Raw => return output.bytes(frame.payload()),
                BinaryFormat::Base64 => STANDARD.encode(frame.payload()),
                BinaryFormat::Hex => frame
                    .payload()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
            },
            OpCode::Ping | OpCode::Pong => {
                if self.show_ping_pong {
                    eprintln!("{:?}: {}", frame.opcode(), STANDARD.encode(frame.payload()));
                }
                return Ok(());
            }
            OpCode::Close => {
                eprintln!(
                    "Disconnected: code={}, reason={}",
                    frame
                        .close_code()
                        .map(u16::from)
                        .map_or_else(|| "none".into(), |c| c.to_string()),
                    frame.close_reason()?.unwrap_or("")
                );
                return Ok(());
            }
            _ => return Ok(()),
        };
        if self.include_time {
            output.message(&format!(
                "{} {message}",
                chrono::Local::now().format("%H:%M:%S%.9f")
            ))
        } else {
            output.message(&message)
        }
    }
}

fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && c == '\\' {
            escaped = true;
            continue;
        }
        if c == '"' {
            quoted = !quoted;
        }
        if !quoted
            && line[i..].starts_with("//")
            && (i == 0 || line[..i].ends_with(char::is_whitespace))
        {
            return line[..i].trim_end();
        }
    }
    line
}

enum Event {
    Input(Option<Input>),
    Frame(yawc::Result<Frame>),
    Stop(std::io::Result<()>),
    Deadline,
}

pub async fn run<S>(
    mut ws: WebSocket<S>,
    mut input: Receiver,
    mut output: Output,
    args: &MessageArgs,
    wait: Option<Duration>,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut input_done = false;
    let mut deadline = None;
    let mut closing_code = None;
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    loop {
        let event = tokio::select! {
            signal_result = &mut signal, if closing_code.is_none() => Event::Stop(signal_result),
            _ = async { match deadline { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending().await } } => Event::Deadline,
            received = ws.next_frame() => Event::Frame(received),
            event = input.recv(), if !input_done => Event::Input(event),
        };
        let frame = match event {
            Event::Stop(result) => {
                result?;
                Some(Frame::close(CloseCode::Normal, ""))
            }
            Event::Deadline => {
                ensure!(
                    closing_code.is_none(),
                    "timed out waiting for peer close frame"
                );
                Some(Frame::close(CloseCode::Normal, ""))
            }
            Event::Frame(received) => {
                let frame = received.context("receive frame")?;
                args.display(&frame, &mut output)?;
                if frame.opcode() == OpCode::Close {
                    if closing_code.is_none() {
                        finish_peer_close(&mut ws).await?;
                    }
                    if let Some(code) = frame.close_code().map(u16::from) {
                        ensure!(
                            matches!(code, 1000 | 1001) || closing_code == Some(code),
                            "peer closed with code {code}: {}",
                            frame.close_reason()?.unwrap_or("")
                        );
                    }
                    return Ok(());
                }
                None
            }
            Event::Input(event) => match event {
                Some(Input::Line(line)) => args.parse(&line)?,
                Some(Input::Frame(frame)) => Some(frame),
                Some(Input::Error(err)) => return Err(err).context("read stdin"),
                Some(Input::Interrupted) => Some(Frame::close(CloseCode::Normal, "")),
                None => {
                    input_done = true;
                    deadline = wait.map(|duration| Instant::now() + duration);
                    None
                }
            },
        };
        if let Some(frame) = frame {
            if frame.opcode() == OpCode::Close {
                closing_code = frame.close_code().map(u16::from);
                input_done = true;
                deadline = Some(Instant::now() + Duration::from_secs(2));
            }
            timeout(Duration::from_secs(5), ws.send(frame))
                .await
                .context("timed out sending frame")??;
        }
    }
}

// yawc queues an automatic close reply while reading a peer's Close frame.
// Poll once more to flush that reply before shutting down the transport.
pub async fn finish_peer_close<S>(ws: &mut WebSocket<S>) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    match timeout(Duration::from_secs(2), ws.next_frame()).await {
        Ok(Err(yawc::WebSocketError::ConnectionClosed)) => {}
        Ok(Err(err)) => return Err(err.into()),
        Ok(Ok(_)) => {}
        Err(_) => bail!("timed out replying to close frame"),
    }
    Ok(())
}
