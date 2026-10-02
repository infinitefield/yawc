# Second performance pass

Before means the working tree at the start of this pass, including the first round of optimizations.
All socket runs use Unix sockets, one server thread, four client workers on separate physical cores,
no TLS or compression, and payload validation. Each final case has five repetitions,
0.5 s warmup and 2 s measurement. This is a shared machine; sample ranges overlap.

## Selected implementation

Median messages/s. Tuned means batches of up to 32 immediately available frames,
a 128 KiB initial read buffer and a 64 KiB write backpressure boundary.
Each batch flushes when input is unavailable or the batch limit is reached. There is no timer delay.

| Connections / bytes / window | Before | Default | Default change | Tuned | uWebSockets |
|---|---:|---:|---:|---:|---:|
| 16 / 1024 / 1 | 337,732 | 351,645 | +4.1% | 337,876 | 405,608 |
| 16 / 1024 / 16 | 649,964 | 662,237 | +1.9% | 2,320,440 | 2,863,449 |
| 64 / 20 / 1 | 376,433 | 382,028 | +1.5% | 373,930 | 450,906 |

The final socket check covers the two workloads that regressed and the pipelined workload.
The earlier eight-workload sweeps are retained, including slower results, in
[the initial sweep](round2-paired.json) and [the read waker candidate sweep](round2-final-paired.json).
They used earlier waker policies. The [selected raw results](round2-selected.json) include ranges,
per-worker RTT percentiles, CPU use, binary hashes and source hashes.

## Buffer size study

Five repetitions per setting, before the final adjustment to write wakers. Only read capacity changes;
the write boundary stays at 64 KiB. One KiB binary frames, 16 connections, window 16.

| Initial read capacity | Median messages/s | Sample range |
|---|---:|---:|
| 64 KiB | 2,325,633 | 2,309,825 to 2,385,146 |
| 128 KiB | 2,415,471 | 2,348,090 to 2,460,663 |
| 512 KiB | 2,426,347 | 2,367,342 to 2,465,610 |
| uWebSockets reference | 2,928,461 | 2,829,358 to 3,076,842 |

128 KiB provides most of the measured benefit. 512 KiB costs four times as much read-buffer memory
with little further gain. Library defaults remain unchanged. Use `with_read_buffer_capacity()`
and `with_backpressure_boundary()` with `feed()` and bounded flushing, as in the benchmark adapter.
`send()` still flushes each frame. Larger buffers can hurt other workloads.
Adding `Options::read_buffer_capacity` requires updating exhaustive `Options` struct literals;
code using the existing builder methods is unchanged.

## CPU work

The final in-memory echo measurements are in [selected echo data](round2-selected-echo.json).
40 samples per size, 1 s warmup and 2 s measurement. Values are Criterion mean nanoseconds.

| Payload bytes | Before ns | After ns | Change |
|---|---:|---:|---:|
| 20 | 492.0 | 444.9 | -9.6% |
| 125 | 484.7 | 465.1 | -4.1% |
| 126 | 513.7 | 572.1 | +11.4% |
| 1024 | 593.2 | 676.0 | +14.0% |
| 16384 | 2,377.8 | 2,335.4 | -1.8% |
| 65536 | 9,389.9 | 8,584.2 | -8.6% |

The 126-byte and 1 KiB echo regressions above prompted a second
[repeat across fresh processes](round2-selected-repeat.json), five runs per binary
in randomized order. Medians of process means were:

| Benchmark | Before ns | After ns | Change |
|---|---:|---:|---:|
| echo_duplex/126 | 568.1 | 553.8 | -2.5% |
| echo_duplex/1024 | 651.7 | 629.9 | -3.3% |

Both runs are retained: variation between processes matters at these timescales.

The [codec and masking study](round2-final-micro.json) used the same masking code with the earlier
read waker policy. Its 1 KiB client-codec result varied, so a [repeat across fresh processes](round2-micro-repeat.json)
ran each binary five times in randomized order. These repeat medians use each process mean:

| Benchmark | Before ns | After ns | Change |
|---|---:|---:|---:|
| codec_roundtrip/client/1024 | 86.4 | 73.9 | -14.4% |
| mask/1024 | 11.3 | 9.6 | -15.0% |

## Kept and rejected

- Generate each client mask with one random `u32`, preserving the existing RNG and explicit masks.
- Copy and mask client payloads in one pass, with AVX2 for larger x86 buffers and a scalar fallback.
- Skip waker registration when write readiness is guaranteed by buffer space, and for immediately ready buffered reads.
  Explicit flushes register before polling the transport; reads that wait after probing are registered and polled again.
- Expose initial read capacity and add bounded batching adapters for yawc and tokio-tungstenite.
- Reject the stack-buffer header rewrite and repeated read-buffer reservation: neither gave a consistent gain.
  Deferring every waker registration also regressed some socket cases; the final policy is narrower.

[Experiment data](round2-experiments.json) preserves the intermediate results. All 268 tests pass;
library and comparison harness Clippy checks pass with warnings denied.

## Remaining gap

[Syscall tracing](round2-syscalls.json) of the 64 KiB candidate measured one write per message normally,
two writes per 16-message batch with default buffers, and one write per batch with tuned buffers.
uWebSockets also used one write per batch. Its remaining advantage therefore needs CPU profiling:
frame ownership, reference counts and payload copies are candidates, not established causes.
The traces include startup and handshake calls and should be used for counts, not timing.
TCP, TLS, compression, ARM and multicore server scaling were not measured in this pass.
