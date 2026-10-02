use std::{hint::black_box, time::Duration};

use bytes::{Bytes, BytesMut};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use futures::SinkExt;
use tokio::{io::duplex, runtime::Builder};
use tokio_util::codec::{Decoder as _, Encoder as _};
use yawc::{
    codec::{Decoder, Encoder},
    mask::apply_mask,
    Frame, OpCode, Options, Role, WebSocket,
};

fn performance(c: &mut Criterion) {
    let sizes = [20, 125, 126, 1024, 16384, 65536];
    let mut group = c.benchmark_group("mask");
    for size in sizes {
        group.throughput(Throughput::Bytes(size as u64));
        let mut data = vec![0x5a; size];
        group.bench_function(BenchmarkId::from_parameter(size), |b| {
            b.iter(|| apply_mask(black_box(&mut data), black_box([1, 7, 19, 31])));
        });
    }
    group.finish();

    let mut group = c.benchmark_group("codec_roundtrip");
    for size in sizes {
        let payload = Bytes::from(vec![0x5a; size]);
        for role in [Role::Client, Role::Server] {
            let mut encoder = Encoder::new(role);
            let peer = if role == Role::Client {
                Role::Server
            } else {
                Role::Client
            };
            let mut decoder = Decoder::new(peer, 1024 * 1024);
            let mut wire = BytesMut::with_capacity(size + 16);
            group.throughput(Throughput::Bytes(size as u64));
            group.bench_function(BenchmarkId::new(role.to_string(), size), |b| {
                b.iter(|| {
                    encoder
                        .encode(Frame::binary(payload.clone()), &mut wire)
                        .unwrap();
                    black_box(decoder.decode(&mut wire).unwrap().unwrap());
                });
            });
        }
    }
    group.finish();

    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let mut group = c.benchmark_group("echo_duplex");
    for size in sizes {
        let (client, server) = duplex(4 * size + 1024);
        let mut client = WebSocket::from_stream(client, Role::Client, Options::default()).unwrap();
        let mut server = WebSocket::from_stream(server, Role::Server, Options::default()).unwrap();
        let payload = Bytes::from(vec![0x5a; size]);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(BenchmarkId::from_parameter(size), |b| {
            b.iter(|| {
                runtime.block_on(async {
                    client.send(Frame::binary(payload.clone())).await.unwrap();
                    let frame = server.next_frame().await.unwrap();
                    server.send(frame).await.unwrap();
                    black_box(client.next_frame().await.unwrap());
                })
            });
        });
    }
    group.finish();
}

fn control_frames(c: &mut Criterion) {
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let mut group = c.benchmark_group("automatic_pong");
    for size in [20, 125] {
        // The larger control frame must wait for the peer to drain the transport.
        let (client, server) = duplex(64);
        let mut client = WebSocket::from_stream(client, Role::Client, Options::default()).unwrap();
        let mut server = WebSocket::from_stream(server, Role::Server, Options::default()).unwrap();
        let payload = Bytes::from(vec![0x5a; size]);
        group.bench_function(BenchmarkId::from_parameter(size), |b| {
            b.iter(|| {
                runtime.block_on(async {
                    tokio::join!(
                        async {
                            client.send(Frame::ping(payload.clone())).await.unwrap();
                            let pong = client.next_frame().await.unwrap();
                            assert_eq!(pong.opcode(), OpCode::Pong);
                            assert_eq!(pong.payload(), &payload);
                            client.send(Frame::binary(Bytes::new())).await.unwrap();
                        },
                        async {
                            assert_eq!(server.next_frame().await.unwrap().opcode(), OpCode::Ping);
                            // Polling for the next frame flushes the automatic Pong.
                            assert_eq!(server.next_frame().await.unwrap().opcode(), OpCode::Binary);
                        },
                    );
                })
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2)).sample_size(40);
    targets = performance, control_frames
}
criterion_main!(benches);
