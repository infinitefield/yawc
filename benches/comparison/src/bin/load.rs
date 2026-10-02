use std::{
    env,
    mem::MaybeUninit,
    net::SocketAddr,
    str::FromStr,
    time::{Duration, Instant},
};

use anyhow::{bail, ensure, Context, Result};
use futures::future::try_join_all;
use libc::rusage;
use rand::random;
use serde_json::json;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpStream, UnixStream},
};

trait Transport: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> Transport for T {}

#[derive(Clone, Copy)]
enum MessageKind {
    Binary,
    Text,
    FragmentedBinary,
    Telemetry,
}

impl FromStr for MessageKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "binary" => Ok(Self::Binary),
            "text" => Ok(Self::Text),
            "fragmented-binary" => Ok(Self::FragmentedBinary),
            "telemetry-binary" => Ok(Self::Telemetry),
            _ => bail!("invalid message type"),
        }
    }
}

fn encode_frame(outgoing: &mut Vec<u8>, payload: &[u8], opcode: u8, fin: bool) {
    let size = payload.len();
    outgoing.push((u8::from(fin) << 7) | opcode);
    if size < 126 {
        outgoing.push(0x80 | size as u8);
    } else if size <= u16::MAX as usize {
        outgoing.push(0x80 | 126);
        outgoing.extend_from_slice(&(size as u16).to_be_bytes());
    } else {
        outgoing.push(0x80 | 127);
        outgoing.extend_from_slice(&(size as u64).to_be_bytes());
    }
    let mask: [u8; 4] = random();
    outgoing.extend_from_slice(&mask);
    outgoing.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
}

struct Client {
    stream: BufReader<Box<dyn Transport>>,
    payload: Vec<u8>,
    outgoing: Vec<u8>,
    incoming: Vec<u8>,
    window: usize,
    opcode: u8,
    fragmented: bool,
    telemetry: Option<TelemetryExpected>,
    latencies: Vec<u64>,
}

struct TelemetryExpected {
    next_sequence: u64,
    messages_seen: u64,
    total: u64,
    batch_sum: u64,
}

impl Client {
    async fn connect(address: &str, size: usize, window: usize, kind: MessageKind) -> Result<Self> {
        let mut stream: Box<dyn Transport> = if let Some(path) = address.strip_prefix("unix:") {
            Box::new(UnixStream::connect(path).await?)
        } else {
            let address: SocketAddr = address.parse()?;
            let stream = TcpStream::connect(address).await?;
            stream.set_nodelay(true)?;
            Box::new(stream)
        };
        let request = "GET / HTTP/1.1\r\nHost: benchmark.invalid\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        stream.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            ensure!(response.len() < 8192, "oversized upgrade response");
            response.push(stream.read_u8().await?);
        }
        let response = String::from_utf8(response)?.to_ascii_lowercase();
        ensure!(response.starts_with("http/1.1 101 "), "upgrade failed");
        ensure!(
            response.contains("s3pplmbitxaq9kygzzhzrbk+xoo="),
            "invalid accept key"
        );
        ensure!(
            !response.contains("sec-websocket-extensions:"),
            "unexpected compression"
        );
        let mut payload: Vec<u8> = (0..size).map(|i| b'!' + (i % 90) as u8).collect();
        let telemetry = if matches!(kind, MessageKind::Telemetry) {
            ensure!(size >= 12, "invalid telemetry size");
            payload[..8].fill(0);
            let (readings, remainder) = payload[8..].as_chunks_mut::<4>();
            ensure!(remainder.is_empty(), "invalid telemetry size");
            let mut batch_sum = 0;
            for (index, reading) in readings.iter_mut().enumerate() {
                let value = (index % 31 + 1) as u32;
                reading.copy_from_slice(&value.to_le_bytes());
                batch_sum += value as u64;
            }
            Some(TelemetryExpected {
                next_sequence: 0,
                messages_seen: 0,
                total: 0,
                batch_sum,
            })
        } else {
            None
        };
        Ok(Self {
            stream: BufReader::with_capacity(128 * 1024, stream),
            payload,
            outgoing: Vec::with_capacity(window * (size + 28)),
            incoming: vec![0; size.max(24)],
            window,
            opcode: if matches!(kind, MessageKind::Text) {
                1
            } else {
                2
            },
            fragmented: matches!(kind, MessageKind::FragmentedBinary),
            telemetry,
            latencies: Vec::with_capacity(16384),
        })
    }

    async fn batch(&mut self) -> Result<()> {
        self.outgoing.clear();
        let first_sequence = self
            .telemetry
            .as_ref()
            .map_or(0, |state| state.next_sequence);
        for _ in 0..self.window {
            if let Some(state) = self.telemetry.as_mut() {
                self.payload[..8].copy_from_slice(&state.next_sequence.to_le_bytes());
                state.next_sequence += 1;
            }
            if self.fragmented {
                let middle = self.payload.len() / 2;
                encode_frame(
                    &mut self.outgoing,
                    &self.payload[..middle],
                    self.opcode,
                    false,
                );
                encode_frame(&mut self.outgoing, &self.payload[middle..], 0, true);
            } else {
                encode_frame(&mut self.outgoing, &self.payload, self.opcode, true);
            }
        }
        self.stream.get_mut().write_all(&self.outgoing).await?;
        for index in 0..self.window {
            let mut offset = 0;
            loop {
                let first = self.stream.read_u8().await?;
                let second = self.stream.read_u8().await?;
                ensure!(
                    first & 0x70 == 0 && second & 0x80 == 0,
                    "invalid server frame flags"
                );
                let expected = if offset == 0 { self.opcode } else { 0 };
                ensure!(first & 0x0f == expected, "unexpected echo opcode");
                let len = match second {
                    126 => self.stream.read_u16().await? as usize,
                    127 => usize::try_from(self.stream.read_u64().await?)?,
                    n => n as usize,
                };
                ensure!(len <= self.incoming.len() - offset, "oversized echo");
                self.stream
                    .read_exact(&mut self.incoming[offset..offset + len])
                    .await?;
                offset += len;
                if first & 0x80 != 0 {
                    break;
                }
            }
            if let Some(state) = self.telemetry.as_mut() {
                ensure!(offset == 24, "invalid telemetry acknowledgement length");
                state.messages_seen += 1;
                state.total += state.batch_sum;
                let sequence = u64::from_le_bytes(self.incoming[..8].try_into()?);
                let total = u64::from_le_bytes(self.incoming[8..16].try_into()?);
                let messages = u64::from_le_bytes(self.incoming[16..24].try_into()?);
                ensure!(
                    sequence == first_sequence + index as u64
                        && total == state.total
                        && messages == state.messages_seen,
                    "invalid telemetry acknowledgement"
                );
            } else {
                ensure!(
                    offset == self.payload.len() && self.incoming[..offset] == self.payload,
                    "corrupted echo"
                );
            }
        }
        Ok(())
    }

    async fn phase(&mut self, deadline: Instant, measure: bool) -> Result<u64> {
        let mut batches = 0;
        while Instant::now() < deadline {
            let sample = measure && batches % 64 == 0;
            let start = sample.then(Instant::now);
            self.batch().await?;
            if let Some(start) = start {
                self.latencies.push(start.elapsed().as_nanos() as u64);
            }
            batches += 1;
        }
        Ok(batches * self.window as u64)
    }
}

fn cpu_seconds() -> Result<f64> {
    let mut usage = MaybeUninit::<rusage>::uninit();
    // SAFETY: getrusage initializes the supplied structure on success.
    let usage = unsafe {
        ensure!(
            libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) == 0,
            "getrusage failed"
        );
        usage.assume_init()
    };
    Ok(usage.ru_utime.tv_sec as f64
        + usage.ru_stime.tv_sec as f64
        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1e6)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    ensure!(
        args.len() == 7,
        "expected address connections bytes window warmup_seconds seconds binary|text|fragmented-binary|telemetry-binary"
    );
    let address = &args[0];
    let connections: usize = args[1].parse()?;
    let size: usize = args[2].parse()?;
    let window: usize = args[3].parse()?;
    let warmup = Duration::from_secs_f64(args[4].parse()?);
    let duration = Duration::from_secs_f64(args[5].parse()?);
    ensure!(
        connections > 0 && window > 0 && size > 0 && !duration.is_zero(),
        "invalid workload"
    );
    let kind: MessageKind = args[6].parse()?;
    let mut clients =
        try_join_all((0..connections).map(|_| Client::connect(address, size, window, kind)))
            .await?;
    let deadline = Instant::now() + warmup;
    tokio::time::timeout(
        warmup + Duration::from_secs(10),
        try_join_all(clients.iter_mut().map(|c| c.phase(deadline, false))),
    )
    .await
    .context("warmup stalled")??;
    let cpu = cpu_seconds()?;
    let start = Instant::now();
    let deadline = start + duration;
    let counts = tokio::time::timeout(
        duration + Duration::from_secs(10),
        try_join_all(clients.iter_mut().map(|c| c.phase(deadline, true))),
    )
    .await
    .context("measurement stalled")??;
    let elapsed = start.elapsed().as_secs_f64();
    let cpu = cpu_seconds()? - cpu;
    let mut latencies: Vec<_> = clients.into_iter().flat_map(|c| c.latencies).collect();
    latencies.sort_unstable();
    let messages: u64 = counts.iter().sum();
    println!(
        "{}",
        json!({
            "messages": messages, "seconds": elapsed, "messages_per_second": messages as f64 / elapsed,
            "client_cpu_fraction": cpu / elapsed, "latency_samples": latencies.len(),
            "batch_rtt_p50_us": latencies[latencies.len() / 2] as f64 / 1000.0,
            "batch_rtt_p99_us": latencies[latencies.len() * 99 / 100] as f64 / 1000.0,
        })
    );
    Ok(())
}
