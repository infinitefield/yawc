use std::{io::IsTerminal, time::Duration};

use anyhow::Context;
use clap::Args;

use crate::{connection, session, terminal};

/// Connect to a WebSocket server.
#[derive(Args)]
#[command(alias = "c")]
pub struct Cmd {
    #[command(flatten)]
    connection: connection::Args,
    #[command(flatten)]
    messages: session::MessageArgs,
    /// Send a message after connecting; repeat to send several. Does not read stdin.
    #[arg(short = 'x', long = "execute")]
    execute: Vec<String>,
    /// Receive replies after stdin EOF or --execute: seconds, a duration, or -1 forever.
    #[arg(short = 'w', long, default_value = "1s", allow_hyphen_values = true, value_parser = parse_wait)]
    wait: Wait,
    /// Disable loading and saving interactive history.
    #[arg(long)]
    no_history: bool,
}

#[derive(Clone, Debug)]
struct Wait(Option<Duration>);

fn parse_wait(value: &str) -> Result<Wait, String> {
    if value == "-1" {
        return Ok(Wait(None));
    }
    let duration = if let Ok(seconds) = value.parse::<f64>() {
        Duration::try_from_secs_f64(seconds).map_err(|e| e.to_string())?
    } else {
        humantime::parse_duration(value).map_err(|e| e.to_string())?
    };
    // Tokio deadlines must fit in a platform Instant.
    if std::time::Instant::now().checked_add(duration).is_none() {
        return Err("wait duration is too large".into());
    }
    Ok(Wait(Some(duration)))
}

pub fn run(cmd: Cmd) -> anyhow::Result<()> {
    let config = connection::Config::new(cmd.connection)?;
    // Validate scripted commands before opening a connection.
    let frames = cmd
        .execute
        .iter()
        .map(|line| cmd.messages.parse(line))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let interactive =
        cmd.execute.is_empty() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let ws = config.connect().await?;
        eprintln!("Connected");
        let (input, output) = if cmd.execute.is_empty() {
            terminal::start(interactive, !cmd.no_history)?
        } else {
            terminal::scripted(frames)
        };
        session::run(ws, input, output, &cmd.messages, cmd.wait.0)
            .await
            .context("WebSocket session failed")
    })
}
