# yawcc cli

A command-line interface tool for WebSocket communication that supports both secure (wss://) and non-secure (ws://) connections.

## Features

- Interactive WebSocket client with command history
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

### Basic Connection

```bash
yawcc c wss://fstream.binance.com/ws/btcusdt@aggTrade
```

### With JSON Validation

```bash
yawcc c --input-as-json wss://fstream.binance.com/ws/btcusdt@aggTrade
```

## Command-line Options

```
Usage: yawcc client [OPTIONS] <URL>

Arguments:
  <URL>  The {WebSocket URL to connect to (ws:// or wss://)

Options:
  -t, --timeout <TIMEOUT>  Maximum duration to wait when establishing the connection. Accepts human-readable formats like "5s", "1m", "500ms" [default: 5s]
      --input-as-json      When enabled, validates and pretty-prints received messages as JSON. Invalid JSON messages will result in an error
  -h, --help               Print help (see more with '--help')
```

## Interactive Commands

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
