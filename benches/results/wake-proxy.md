# Wake proxy follow-up

Baseline: `b29acd1`. Remove the control-flush proxy clone and deduplicate read/write
wakeups when `will_wake` confirms they target the same task. Distinct tasks still both wake.

Microbenchmarks use five fresh processes per variant in randomized order, pinned to one
core, with 20 samples, 0.25 s warmup and 0.5 s measurement per case.
Values below are medians of process means in nanoseconds.

| Case | Before | Both changes | Change |
|---|---:|---:|---:|
| Automatic Pong, 20 bytes | 3953.2 | 3440.3 | -13.0% |
| Automatic Pong, 125 bytes | 12242.1 | 11535.1 | -5.8% |
| Echo, 20 bytes | 474.1 | 466.5 | -1.6% |
| Echo, 1024 bytes | 571.3 | 596.6 | +4.4% |

A separate comparison isolates deduplication: both variants remove the proxy clone.
Deduplication reduces median Pong time by 7.6% at 20 bytes and 3.4% at 125 bytes.
Echo results are mixed: 20 bytes slows by 5.9%, while 1024 bytes improves by 2.2%.
These results support a control-frame improvement, not a universal speedup.

The socket comparison uses Unix sockets, one server thread, four client workers,
five randomized repetitions, 0.5 s warmup and 2 s measurement. Every payload is validated.
Median default throughput with both changes rises 2.1% for 64 connections with 20-byte
messages, changes by less than 0.1% for 16 connections with 1 KiB messages, and rises
1.8% for the 1 KiB, window-16 pipeline. The machine is shared and sample ranges overlap
in the first two cases.

Raw measurements: [microbenchmarks](wake-proxy-micro.json),
[isolated deduplication](wake-proxy-isolated.json), [sockets](wake-proxy-sockets.json).
