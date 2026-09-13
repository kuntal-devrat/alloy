# Actor Concurrency & Message Passing

Modern multicore CPUs have dozens of cores, yet standard JavaScript remains trapped in a single-threaded execution model. Node.js `worker_threads` and `SharedArrayBuffer` introduce notorious data race conditions and synchronization deadlocks.

Alloy solves concurrency through the **Actor Model**, inspired by Erlang and Go:
- **Shared-Nothing:** Each actor runs on an independent thread with its own isolated memory heap and execution context.
- **Asynchronous Message Passing:** Actors communicate strictly through typed, bounded message channels.
- **Fearless Multithreading:** Data races and deadlocks are mathematically impossible because mutable state is never shared across threads.

---

## 1. Spawning Actors (`spawn`)

The global `spawn(fn, ...args)` function launches a background worker actor. It returns a `Promise` that resolves to the return value of the actor function:

```javascript
import { spawn } from 'alloy:core';

async function calculatePrimes() {
    print("Main thread: launching worker...");

    // Worker executes on an independent OS thread with isolated heap
    const result = await spawn(function (limit) {
        let count = 0;
        for (let n = 2; n <= limit; n++) {
            let isPrime = true;
            for (let i = 2; i * i <= n; i++) {
                if (n % i === 0) { isPrime = false; break; }
            }
            if (isPrime) count++;
        }
        return count;
    }, 100000);

    print(`Worker calculated ${result} primes.`);
}

calculatePrimes();
```

---

## 2. Channels (`channel`)

For streaming data or bidirectional coordination between actors, Alloy provides asynchronous channels:

```javascript
import { channel, spawn } from 'alloy:core';

// Create a named channel accessible across actors
channel.create("orders");

// Spawn worker actor
spawn(async function () {
    const ch = channel.get("orders");
    print("Worker: waiting for orders...");

    while (true) {
        const order = await ch.recv();
        if (order.type === "STOP") break;
        print(`Processing order #${order.id} ($${order.amount})`);
    }
});

// Producer sending orders
const orderChannel = channel.get("orders");
orderChannel.send({ id: 101, amount: 49.99 });
orderChannel.send({ id: 102, amount: 120.00 });
orderChannel.send({ type: "STOP" });
```

---

## 3. Channel API Reference

| Method | Description |
| :--- | :--- |
| `channel.create(name)` | Registers a named cross-actor channel across all threads. |
| `channel.get(name)` | Retrieves an existing named channel by identifier. |
| `channel()` | Creates an anonymous channel within the current execution context. |
| `ch.send(value)` | Enqueues a message. If an actor is waiting, immediately fulfills its waiter. |
| `ch.recv()` | Returns a `Promise` that resolves when the next message arrives. |
| `ch.tryRecv()` | Non-blocking receive: returns the next message if present, or `undefined`. |
