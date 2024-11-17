use clap::{Parser, Subcommand};

mod client;

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
    Client(client::Cmd),
    // Server(server::Cmd),
}

fn main() {
    let args = Cli::parse();
    let res = match args.command {
        Commands::Client(cmd) => client::run(cmd),
    };
    if let Err(err) = res {
        eprintln!("{:?}", err);
    }
}

// async fn run_server(port: &str) {
//     let addr = format!("127.0.0.1:{}", port);
//     let listener = TcpListener::bind(&addr).await.expect("Failed to bind");
//     println!("WebSocket server listening on: ws://{}", addr);

//     while let Ok((stream, _)) = listener.accept().await {
//         tokio::spawn(handle_connection(stream));
//     }
// }

// async fn handle_connection(stream: TcpStream) {
//     let ws_stream = accept_async(stream).await.expect("Failed to accept");
//     println!("New WebSocket connection");

//     let (mut write, mut read) = ws_stream.split();

//     while let Some(msg) = read.next().await {
//         let msg = msg.unwrap();
//         if msg.is_text() || msg.is_binary() {
//             println!("Received: {}", msg);
//             write.send(msg).await.unwrap();
//         }
//     }
// }
