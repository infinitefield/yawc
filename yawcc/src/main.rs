use clap::{Parser, Subcommand};

mod client;
mod connection;
mod server;
mod session;
mod terminal;

/// WebSocket client/server CLI tool for real-time communication
///
/// Supports opt-in inline comments using --comments for documenting messages.
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
    Client(Box<client::Cmd>),

    /// Start a WebSocket server to accept client connections
    ///
    /// The server can handle multiple client connections and echoes the messages.
    Server(server::Cmd),
}

fn main() -> std::process::ExitCode {
    // A network disconnect can finish while the line editor is still reading.
    // Restore the original console modes even when that input thread is blocked.
    let _terminal = terminal::Restore::capture();
    let args = Cli::parse();
    let res = match args.command {
        Commands::Client(cmd) => client::run(*cmd),
        Commands::Server(cmd) => server::run(cmd),
    };
    if let Err(err) = res {
        eprintln!("error: {err:#}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
