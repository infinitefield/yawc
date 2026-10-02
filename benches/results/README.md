# Measured baseline

For the current implementation, see the [latest comparison of all libraries](pr-comparison.md).

Transport: Unix sockets. One server thread, two client processes
(one for the single-connection case), separate physical cores, no TLS or compression.
Baseline: 3 repetitions, 0.5 s warmup,
2 s measurement per sample. Values are median messages/s.
These results describe local echo on a shared machine.

Raw data: [baseline](baseline.json), [paired run](paired.json),
[microbenchmarks](micro.json). Compiler versions and C++ revisions are in the baseline;
Rust versions are pinned in [Cargo.lock](../comparison/Cargo.lock).

| Connections / bytes / window / type | yawc-before | tokio-tungstenite | fastwebsockets | uWebSockets | Boost.Beast |
|---|---:|---:|---:|---:|---:|
| 1 / 20 / 1 / binary | 117,119 | 107,489 | 126,841 | 98,469 | 101,944 |
| 16 / 1024 / 1 / binary | 294,038 | 225,031 | 342,818 | 387,276 | 250,378 |
| 16 / 1024 / 1 / text | 307,062 | 223,924 | 353,106 | 385,035 | 249,755 |
| 16 / 1024 / 16 / binary | 568,515 | 600,522 | 648,156 | 2,736,460 | 290,387 |
| 16 / 16384 / 1 / binary | 158,851 | 147,844 | 178,809 | 178,916 | 145,399 |
| 16 / 65536 / 1 / binary | 51,256 | 51,651 | 51,450 | 51,733 | 49,609 |
| 64 / 20 / 1 / binary | 347,192 | 241,549 | 363,075 | 423,340 | 260,326 |
| 128 / 16384 / 1 / binary | 159,822 | 146,121 | 178,123 | 178,629 | 143,461 |

## Paired yawc comparison

5 repetitions of each binary in randomized order using the same client.
Ranges show the minimum and maximum sample throughput, not confidence intervals.

| Connections / bytes / window / type | Before median (range) | After median (range) | Change |
|---|---:|---:|---:|
| 1 / 20 / 1 / binary | 121,497 (116,782 to 122,287) | 120,132 (116,726 to 123,605) | -1.1% |
| 16 / 1024 / 1 / binary | 299,130 (286,486 to 311,687) | 319,945 (308,975 to 327,203) | +7.0% |
| 16 / 1024 / 1 / text | 296,933 (285,811 to 307,036) | 297,355 (292,080 to 321,391) | +0.1% |
| 16 / 1024 / 16 / binary | 578,324 (572,315 to 583,377) | 607,203 (602,044 to 622,583) | +5.0% |
| 16 / 16384 / 1 / binary | 160,063 (155,309 to 165,153) | 169,782 (169,395 to 172,274) | +6.1% |
| 16 / 65536 / 1 / binary | 50,845 (50,120 to 52,339) | 50,496 (49,566 to 52,093) | -0.7% |
| 64 / 20 / 1 / binary | 345,153 (344,520 to 348,235) | 326,505 (324,617 to 354,728) | -5.4% |
| 128 / 16384 / 1 / binary | 158,675 (151,587 to 163,331) | 165,671 (162,840 to 168,567) | +4.4% |

Highest client CPU fraction in any sample: 1.00 of one core.
Raw samples include CPU usage, per-worker batch RTT percentiles and binary hashes.
RTT includes client work and queueing; window 16 measures batch completion.

## Small-message repeat check

The first paired run changed throughput by -5.4% for 64 connections and 20-byte messages.
A repeat with 10 samples per binary, 1 s warmup
and 3 s measurement gave:

| Library | Median messages/s | Range |
|---|---:|---:|
| yawc-before | 344,174 | 336,076 to 347,719 |
| yawc | 351,714 | 326,527 to 353,934 |

Both runs are retained to show measurement variability.

## Client capacity check

The cases below use 4 client workers instead of two.
This checks whether client CPU capacity capped the results with two workers.
Medians in messages/s; these results are a separate workload.

| Connections / bytes / window | Before | After | uWebSockets |
|---|---:|---:|---:|
| 16 / 1024 / 16 | 576,715 | 609,252 | 2,818,681 |
| 16 / 16384 / 1 | 159,292 | 162,596 | 207,060 |
| 16 / 65536 / 1 | 63,030 | 64,433 | 75,737 |
| 128 / 16384 / 1 | 158,046 | 160,829 | 214,097 |

## In-memory echo

Criterion mean time per complete client/server echo, with no sockets.
40 samples per case; full estimates and confidence intervals are in micro.json.

| Payload bytes | Before ns | After ns | Time change |
|---|---:|---:|---:|
| 20 | 625.3 | 500.7 | -19.9% |
| 125 | 661.5 | 520.3 | -21.3% |
| 126 | 644.7 | 578.1 | -10.3% |
| 1024 | 774.4 | 652.9 | -15.7% |
| 16384 | 2,949.3 | 2,654.9 | -10.0% |
| 65536 | 10,768.9 | 9,655.2 | -10.3% |

The isolated 20-byte client codec changed by +12.4% in time.
The full JSON retains all codec and masking results, including regressions.

Further measurements and tuning: [second performance pass](round2.md).
Control-frame and wakeup measurements: [wake proxy follow-up](wake-proxy.md).
