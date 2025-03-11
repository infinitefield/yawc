use clap::{Parser, Subcommand};

mod client;
mod server;

/// WebSocket client/server CLI tool for real-time communication
///
/// Supports inline comments using // for documenting messages and formats.
/// Comments can be searched with ctrl+r in history.
///
/// Examples:
///   {"type": "ping"} // Heartbeat
///
#[derive(Parser)]
#[command(author, version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a WebSocket client to connect to a server
    ///
    /// The client can send messages and receive responses from the server
    Client(client::Cmd),

    /// Start a WebSocket server to accept client connections
    ///
    /// The server can handle multiple client connections and echoes the messages.
    Server(server::Cmd),
}

fn main() {
    let args = Cli::parse();
    let res = match args.command {
        Commands::Client(cmd) => client::run(cmd),
        Commands::Server(cmd) => server::run(cmd),
    };
    if let Err(err) = res {
        eprintln!("{:?}", err);
    }
}
