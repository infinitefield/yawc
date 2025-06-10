# yawc cli

A command-line interface tool for WebSocket communication that supports both secure (wss://) and non-secure (ws://) connections with both client and server capabilities.

## Features

- Interactive WebSocket client with command history
- Simple WebSocket server implementation
- Support for both ws:// and wss:// connections
- JSON validation and pretty-printing
- Command history with search capabilities (Ctrl+R)
- Inline comments support using // for quick search and documentation
- Configurable connection timeout

## Installation

```bash
cargo install yawcc
```

## Usage

### Client Mode

#### Basic Connection

```bash
yawcc c wss://fstream.binance.com/ws/btcusdt@aggTrade
```

#### With JSON Validation

```bash
yawcc c --input-as-json wss://fstream.binance.com/ws/btcusdt@aggTrade
```

#### Direct TCP Connection

```bash
yawcc c https://echo.websocket.org --tcp-host 127.0.0.1:8080
```

### Server Mode

#### Start a Basic Echo Server

```bash
yawcc s
```

This starts a WebSocket server on localhost:9090 that echoes back any messages it receives.

#### Custom Port and Path

```bash
yawcc s --listen 0.0.0.0:8080 --path /ws
```

This starts a WebSocket server on all interfaces (0.0.0.0) on port 8080 with the endpoint path /ws.

## Command-line Options

### Client Options

```
Usage: yawcc client [OPTIONS] <URL>

Arguments:
  <URL>  The WebSocket URL to connect to (ws:// or wss://)

Options:
  -t, --timeout <TIMEOUT>    Maximum duration to wait when establishing the connection. Accepts human-readable formats like "5s", "1m", "500ms" [default: 5s]
      --include-time         Includes the timestamp for each message
  -H, --header <Headers>     Custom headers to send to the server in "Key: Value" format For example: --header "Authorization: Bearer token123"
      --input-as-json        When enabled, validates and pretty-prints received messages as JSON. Invalid JSON messages will result in an error
      --tcp-host <TCP_HOST>  Connect directly to a TCP host instead of using WebSocket URL. Format: host:port (e.g., "127.0.0.1:8080")
  -h, --help                 Print help (see more with '--help')
```

### Server Options

```
Usage: yawcc server [OPTIONS]

Options:
  -l, --listen <LISTEN>  Network address and port to listen on [default: 127.0.0.1:9090]
  -p, --path <PATH>      URI path to serve the WebSocket endpoint [default: /]
  -h, --help             Print help (see more with '--help')
```

## Interactive Client Commands

Once connected, you can:

- Send messages by typing and pressing Enter
- Add comments to messages using // (comments are saved in history but not sent)
- Search through command history using Ctrl+R
- Exit the client using Ctrl+C or Ctrl+D

Example with comments:

```
> {"type": "ping"} // Heartbeat message
> {"command": "subscribe", "channel": "updates"} // Subscribe to updates
```

## WebSocket Server

The built-in WebSocket server:

- Acts as a simple echo server, sending back any messages it receives
- Supports both text and binary frames
- Handles WebSocket protocol details like handshake and frame masking automatically
- Can be terminated with Ctrl+C

Example of connecting to the server with the client:

```bash
# In terminal 1, start the server
yawcc s --listen 127.0.0.1:9090 --path /ws

# In terminal 2, connect with the client
yawcc c ws://127.0.0.1:9090/ws

# Now you can send messages that will be echoed back
```

## History

Command history is automatically saved to `~/.yawcc_history` and is loaded when the client starts.

## Building from Source

```bash
git clone https://github.com/infinitefield/yawc
cd yawc/yawcc
cargo build --release
```

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
