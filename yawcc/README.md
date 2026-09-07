# yawcc

A WebSocket client and test server built on [yawc](https://github.com/infinitefield/yawc). Supports `ws://` and `wss://`, interactive history, shell pipelines, TLS client certificates, SOCKS5 and HTTP CONNECT proxies.

## Installation

```sh
cargo install yawcc
```

## Interactive client

```sh
yawcc c wss://example.com/ws
# The full command names are `client` and `server`.
```

Type a message and press Enter to send one text message. Ctrl+R searches history; Ctrl+C starts a close handshake. Ctrl+D ends input and waits for replies according to `--wait`.

Messages are sent literally by default, including `//` and URLs. To annotate commands, enable `--comments`:

```sh
yawcc c ws://localhost:9090 --comments
```

```text
> {"url":"https://example.com"} // saved annotation
```

Only `//` at the start of a line or after whitespace, outside double-quoted strings, starts a comment. Escaped quotes are respected. Comment-only lines are skipped. Without `--comments`, empty lines send empty text messages.

Interactive history is saved after each entered line to `~/.yawcc_history`. Existing `~/.yawc_history` is imported when the new file does not exist. Use `--no-history` to disable persistence. History stores the messages you enter, including annotations.

## Scripts and pipelines

```sh
printf '%s\n' '{"type":"ping"}' |
  yawcc c ws://localhost:9090 --wait 1s |
  jq .

# Send several messages in order, then receive for 10 seconds.
yawcc c wss://example.com/ws \
  -x '{"type":"subscribe","channel":"trades"}' \
  -x '{"type":"status"}' --wait 10s
```

- With redirected stdin or stdout, yawcc uses plain line input without prompts or history. Each stdin line is one text message; a final line without a newline is sent too.
- Repeat `-x` / `--execute` to send several messages. This mode does **not** read stdin.
- `-w` / `--wait` starts after stdin EOF or the last executed message. Its default is `1s`. It accepts seconds (`0.5`), human-readable durations (`500ms`, `1m`), or `-1` to receive until the peer closes or you press Ctrl+C. `0` starts closing immediately after input finishes.
- Diagnostics, connection status, control frames, and close details go to stderr. Received data goes to stdout; terminal messages have a `<` prefix only when both stdin and stdout are terminals.
- Shutdown waits up to two seconds for the peer's close response. A stalled send fails after five seconds.
- Runtime errors exit with status 1; invalid command-line syntax exits with status 2. A normal peer close (1000, 1001, or no code) succeeds. Other peer close codes fail, unless they echo a code explicitly sent with `/close`.

These flags use yawcc's existing subcommand syntax; it is not a drop-in parser for wscat's command line.

## Connection options

```sh
yawcc c wss://example.com/ws \
  -H 'Authorization: Bearer token' -H 'X-Request-ID: example' \
  --origin https://example.com --subprotocol graphql-transport-ws

yawcc c wss://example.com/ws --auth user:password

# Retain the URL's Host header and TLS server name, but dial another address.
yawcc c wss://example.com/ws --tcp-host 127.0.0.1:8443
```

| Option | Behavior |
| --- | --- |
| `-t, --timeout 5s` | One total deadline for DNS, proxy setup, TLS, the handshake, and redirects |
| `-H, --header 'Key: Value'` | Repeatable custom headers; malformed headers are rejected |
| `-o, --origin ORIGIN` | Set the Origin header |
| `-s, --subprotocol NAME` | Repeatable, ordered subprotocol offer; the server must select an offered protocol |
| `--auth user:password` | Basic HTTP authentication |
| `--host HOST` | Override the HTTP Host header without changing the TCP destination or TLS server name |
| `--tcp-host host:port` | Override the TCP destination, including through a proxy |
| `-L, --location` | Follow 301, 302, 303, 307, and 308 redirects |
| `--max-redirects 10` | Redirect limit with `--location` |

WebSocket handshake headers are managed by yawcc: use the dedicated subprotocol option instead of setting `Sec-WebSocket-*` headers. Compression is negotiated automatically when supported by the server.

Redirects are disabled by default. When enabled, relative locations and HTTP(S) locations are supported. WSS-to-WS downgrades are rejected. On a cross-origin redirect, all custom headers and authentication are dropped, and `--tcp-host` stops applying. Cross-origin redirects with a client certificate are rejected.

## TLS

```sh
# Trust an additional CA.
yawcc c wss://localhost:8443 --ca ca.pem

# Mutual TLS.
yawcc c wss://example.com/ws --ca ca.pem --cert client.pem --key client-key.pem

# Connect to a test endpoint without verifying its certificate.
yawcc c wss://localhost:8443 --no-check
```

Public WebPKI roots are trusted by default. `--ca` adds PEM certificates. `--cert` accepts a PEM certificate chain and requires `--key`, an unencrypted PEM private key. Encrypted private keys/passphrase prompting are not supported. `-n` / `--no-check` disables certificate verification, while TLS handshake signatures are still verified.

## Proxies

```sh
yawcc c wss://example.com/ws --proxy socks5h://127.0.0.1:1080
yawcc c wss://example.com/ws --proxy socks5://user:password@127.0.0.1:1080
yawcc c wss://example.com/ws --proxy http://user:password@127.0.0.1:3128
yawcc c wss://example.com/ws --proxy https://proxy.example.com:8443
```

`socks5h://` sends the destination hostname to the proxy. `socks5://` resolves it locally. HTTP and HTTPS proxies use CONNECT for both WS and WSS targets. Proxy credentials are URL-decoded and used only for proxy authentication. TLS to a WSS target runs end to end through the tunnel. HTTPS proxy TLS also uses `--ca` and `--no-check`, but never receives the target's client certificate. Proxy environment variables are not read.

## Message formatting and control frames

```sh
yawcc c ws://localhost:9090 --input-as-json --include-time
yawcc c ws://localhost:9090 --slash --show-ping-pong
```

With `--slash`:

```text
> /ping hello
> /pong hello
> /close 1000, finished
```

Control payloads are limited to 125 bytes; close reasons to 123 bytes. Invalid or reserved close codes are rejected. Unknown slash-prefixed messages remain literal text. `-P` / `--show-ping-pong` reports incoming control payloads in base64 on stderr. Ping replies are automatic.

`--input-as-json` (alias `--json`) pretty-prints received JSON and fails on invalid JSON. It formats **received** data, not outgoing messages. `--include-time` also works with JSON output.

`--binary hex` is the default binary display. `--binary base64` writes base64. Both add a newline per message. `--binary raw` writes exact binary bytes with no newline, timestamp, or terminal prefix; adjacent binary messages are concatenated. Text input always sends text frames.

## Server

```sh
# Echo text and binary messages.
yawcc s --listen 127.0.0.1:9090 --path /ws

# Manually respond to clients; stdin messages are broadcast to all clients.
yawcc s --listen 127.0.0.1:9090 --interactive --slash

# Require a matching subprotocol.
yawcc s --subprotocol chat
```

The server defaults to an echo endpoint at `ws://127.0.0.1:9090/`. The path must match exactly; query strings are allowed. Port `0` selects an available port, printed on stderr. The built-in server listens over WS; TLS options apply to the client.

`--interactive` displays incoming data on stdout and broadcasts entered messages instead of echoing. It also accepts piped input. EOF or Ctrl+C closes connected clients and stops the server. Without `--interactive`, stdin is ignored. Message-formatting and slash-command options are shared with the client.

## Development

```sh
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt -- --check
```

The CLI has its own Cargo workspace. Run these commands from `yawcc/`, or pass `--manifest-path yawcc/Cargo.toml` from the repository root. Integration tests launch real CLI processes and local peers/proxies, including generated TLS certificates; they do not require internet services. A dedicated CI workflow runs on Linux, macOS, and Windows.

Use `yawcc c --help` and `yawcc s --help` for the complete option list.
