# Current library comparison

Library source: `e3bcfa3`.

Median messages/s, higher is better. Unix sockets, one server thread, four client workers on separate physical cores, binary frames, no TLS or compression. Five randomized repetitions, 0.5 s warmup and 2 s measurement per sample. All 105 samples validate every echoed payload. The first two columns use window 1.

| Library / configuration | 20 B, 64 connections | 1 KiB, 16 connections | 1 KiB, 16 connections, window 16 |
|---|---:|---:|---:|
| yawc, default | 392,183 | 345,448 | 670,007 |
| yawc, batched + tuned buffers | 381,158 | 333,115 | 2,332,732 |
| tokio-tungstenite 0.30.0 | 243,024 | 251,942 | 646,685 |
| tokio-tungstenite 0.30.0, batched | 222,099 | 210,030 | 1,809,416 |
| fastwebsockets 0.10.0 | 409,105 | 372,946 | 700,590 |
| uWebSockets (C++) | 462,790 | 421,603 | 3,001,028 |
| Boost.Beast 1.90 (C++) | 276,501 | 270,112 | 315,656 |

Tuned yawc uses a 128 KiB initial read buffer, a 64 KiB write backpressure threshold, and batches of up to 32 immediately available messages. The batched tungstenite adapter uses the same batch limit with its default buffers. Other Rust adapters and Beast flush each response; uWebSockets batches writes internally. Larger buffers and batching help the pipelined case but reduce throughput in some window-1 cases. These are application configurations, not equivalent library defaults. The machine is shared, and raw samples include ranges and CPU use; these rates are not a TCP/TLS or application-throughput guarantee.

[Raw measurements](pr-comparison.json) include source and binary hashes, compiler versions, pinned C++ revisions and per-worker RTT percentiles. [Configuration and echo loop](../README.md#throughput-settings).
