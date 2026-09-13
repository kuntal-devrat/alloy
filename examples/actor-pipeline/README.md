# Actor Concurrency Showcase: Real-Time Event Pipeline

This showcase demonstrates Alloy's **message-passing actor model** and isolated heap architecture.

Unlike Node.js `worker_threads` (which require complex serialization over postMessage or risky SharedArrayBuffer mutex locks), Alloy provides native, lightweight actor processes via `spawn()` and asynchronous MPSC queues via `channel()`.

## Architecture

```
                      +-------------------+
                      |  Producer Actor   |
                      | (Generates Events)|
                      +---------+---------+
                                |
               +----------------+----------------+
               |                                 |
               v                                 v
     +--------------------+            +--------------------+
     |  Worker Actor #1   |            |  Worker Actor #2   |
     | (Risk & Anomaly)   |            | (Risk & Anomaly)   |
     +---------+----------+            +---------+----------+
               |                                 |
               +----------------+----------------+
                                |
                                v
                      +-------------------+
                      | Aggregator Actor  |
                      | (Metrics & Stats) |
                      +-------------------+
```

## Highlights

- **Zero Shared Mutable State:** Each actor executes on an independent OS thread with an isolated memory heap. Data races are impossible by design.
- **Microsecond Message Passing:** Channels communicate directly through lock-free ring-buffer channels.
- **Massive Concurrency:** Hundreds of actors can be spawned concurrently without V8 engine instance overhead.

## Running the Showcase

```bash
alloy run examples/actor-pipeline/main.ajs
```

Expected output:
```
[Alloy Actor Pipeline] Initializing 4 worker actors...
[Producer] Streaming 1,000 transactions across worker pool...
[Worker 1] Processed 250 transactions (avg risk: 0.28)
[Worker 2] Processed 250 transactions (avg risk: 0.31)
[Worker 3] Processed 250 transactions (avg risk: 0.29)
[Worker 4] Processed 250 transactions (avg risk: 0.30)
[Aggregator] Batch Complete!
  Total Transactions : 1,000
  Total Volume       : $482,910.45
  Flagged Anomalies  : 14
  Elapsed Time       : 18.4ms
  Throughput         : 54,347 events/sec
```
