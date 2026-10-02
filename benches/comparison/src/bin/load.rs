use std::{
    env,
    mem::MaybeUninit,
    net::SocketAddr,
    time::{Duration, Instant},
};

use anyhow::{ensure, Context, Result};
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

struct Client {
    stream: BufReader<Box<dyn Transport>>,
    payload: Vec<u8>,
    outgoing: Vec<u8>,
    incoming: Vec<u8>,
    window: usize,
    opcode: u8,
    latencies: Vec<u64>,
}

impl Client {
    async fn connect(address: &str, size: usize, window: usize, text: bool) -> Result<Self> {
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
        let payload = (0..size).map(|i| b'!' + (i % 90) as u8).collect();
        Ok(Self {
            stream: BufReader::with_capacity(128 * 1024, stream),
            payload,
            outgoing: Vec::with_capacity(window * (size + 14)),
            incoming: vec![0; size],
            window,
            opcode: if text { 1 } else { 2 },
            latencies: Vec::with_capacity(16384),
        })
    }

    async fn batch(&mut self) -> Result<()> {
        self.outgoing.clear();
        for _ in 0..self.window {
            let size = self.payload.len();
            self.outgoing.push(0x80 | self.opcode);
            if size < 126 {
                self.outgoing.push(0x80 | size as u8);
            } else if size <= u16::MAX as usize {
                self.outgoing.push(0x80 | 126);
                self.outgoing
                    .extend_from_slice(&(size as u16).to_be_bytes());
            } else {
                self.outgoing.push(0x80 | 127);
                self.outgoing
                    .extend_from_slice(&(size as u64).to_be_bytes());
            }
            let mask: [u8; 4] = random();
            self.outgoing.extend_from_slice(&mask);
            self.outgoing.extend(
                self.payload
                    .iter()
                    .enumerate()
                    .map(|(i, b)| b ^ mask[i & 3]),
            );
        }
        self.stream.get_mut().write_all(&self.outgoing).await?;
        for _ in 0..self.window {
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
            ensure!(
                offset == self.payload.len() && self.incoming == self.payload,
                "corrupted echo"
            );
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
        "expected address connections bytes window warmup_seconds seconds binary|text"
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
    ensure!(
        matches!(args[6].as_str(), "binary" | "text"),
        "invalid message type"
    );
    let mut clients = try_join_all(
        (0..connections).map(|_| Client::connect(address, size, window, args[6] == "text")),
    )
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
