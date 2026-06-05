# How to build a distributed queue in a single JSON file on object storage

February 12, 2026•Dan Harrison (Engineer)

We recently replaced our internal indexing job queue, which notifies indexing nodes to build and update search indexes after data is written to the [WAL](https://turbopuffer.com/docs/concepts#wal). The queue is not part of the write path; it's purely a notification system used to schedule asynchronous indexing work. The prior version sharded queues across indexing nodes, so a slow node would block all jobs assigned to it even if other nodes were idle. The new version uses a single queue file on object storage with a stateless broker for FIFO execution, at-least-once guarantees, and 10x lower tail latency versus our prior implementation, so indexing jobs spend less time in the queue.

### turbopuffer indexing job median queue time, switch to new queue at 20:17:30

Why are we so obsessed with building on object storage? Because it's simple, predictable, easy to be on-call for, and extremely scalable. We know how it behaves, and as long as we design within those boundaries, we know it will perform.

Rather than present the final design of our new queue from the top down, let's build it from the bottom up, starting with the simplest thing that works and adding complexity as needed.

## Step 1: queue.json

The total size of the data in a turbopuffer job queue is small, well less than 1 GiB. This easily fits in memory, so the simplest functional design is a single file (e.g., `queue.json`) repeatedly overwritten with the full contents of the queue.

A queue **pusher** reads the contents of the queue, appends a new job to the end, and writes it using [compare-and-set (CAS)](https://aws.amazon.com/about-aws/whats-new/2024/11/amazon-s3-functionality-conditional-writes/).

A queue **worker** similarly uses CAS to mark the first unclaimed job as in progress (○ → ◐), and then gets to work.

We'll call pushers and workers **clients**, and push and claim operations **requests**.

The compare-and-set (CAS) primitive makes this atomic. The write only succeeds if `queue.json` hasn't changed since it was read. If it _has_ changed, the client reads the new contents and tries again. This gives strong consistency guarantees without complex locking.

```
queue.json
 ┌──────────────────────────────────────┐
 │ {"jobs": ["◐", "○", "○", "○", "○",]} │
 └──────────────────────────────────────┘
              ▲                     ▲
              │                     │
    CAS write │           CAS write │
              │                     │
        ┌─────┴────┐         ┌──────┴───┐
        │  worker  │         │  pusher  │
        └──────────┘         └──────────┘
```

```
queue.json
┌─────────────────────────────────┐
│ {"jobs":["◐","○","○","○","○",]} │
└─────────────────────────────────┘
            ▲                 ▲
            │                 │
            │                 │
        CAS │             CAS │
      write │           write │
            │                 │
            │                 │
      ┌─────┴──┐        ┌─────┴──┐
      │ worker │        │ pusher │
      └────────┘        └────────┘
```

This simplest of queues works surprisingly well! For up to 1 request per second (a limit [imposed by GCS](https://docs.cloud.google.com/storage/docs/objects#immutability)), it's already production grade thanks to everything that object storage does for us.

But most queues (including ours) receive more than one request per second. We need more throughput.

## Step 2: queue.json with group commit

Object storage has many virtues, but low write latency is not one of them. Replacing a file can take [up to 200ms](https://turbopuffer.com/docs/tradeoffs), so instead of writing jobs one-by-one, we need to batch. Whenever a write is in flight, we buffer incoming requests in memory. As soon as the write finishes, we flush the buffer as the next CAS write.

This technique is commonly called _[group commit](https://turbopuffer.com/docs/concepts#group-commit)_, and it's the same pattern turbopuffer uses for [batching writes to the WAL](https://turbopuffer.com/docs/architecture). Traditional databases also use this technique to coalesce `fsync(2)` calls to maximize the committed throughput to disk.

```
queue.json
 ┌───────────────────────────────────────────────────────────────┐
 │ {"jobs": ["◐", "◐", "◐", "◐", "○", "○", "○", "○", "○", "○",]} │
 └───────────────────────────────────────────────────────────────┘
                             ▲                               ▲
                             │                               │
                             │                               │
                group commit │                  group commit │
                             │                               │
                ┌── buffer ──┴──────┐      ┌── buffer ───────┴─┐
                │ ┌───┬───┬───┬───┐ │      │ ┌───┬───┬───┬───┐ │
                │ │ ◐ │ ◐ │ ◐ │ ◐ │ │      │ │ ○ │ ○ │ ○ │ ○ │ │
                │ └───┴───┴───┴───┘ │      │ └───┴───┴───┴───┘ │
                └─────────▲─────────┘      └─────────▲─────────┘
                          │                          │
                          │                          │
                    ┌─────┴────┐               ┌─────┴────┐
                    │  worker  │               │  pusher  │
                    └──────────┘               └──────────┘
```

```
queue.json
┌─────────────────────────────────┐
│ {"jobs":["◐","◐","◐","○","○",]} │
└─────────────────────────────────┘
                ▲             ▲
          group │       group │
         commit │      commit │
                │             │
    ┌─buffer────┴─┐ ┌─buffer──┴───┐
    │┌───┬───┬───┐│ │┌───┬───┬───┐│
    ││ ◐ │ ◐ │ ◐ ││ ││ ○ │ ○ │ ○ ││
    │└───┴───┴───┘│ │└───┴───┴───┘│
    └──────▲──────┘ └──────▲──────┘
           │               │
      ┌────┴───┐      ┌────┴───┐
      │ worker │      │ pusher │
      └────────┘      └────────┘
```

Group commit solves our throughput problem by decoupling write rate from request rate. The scaling bottleneck shifts from write latency (~200ms/write) to network bandwidth (~10 GB/s) – far greater than what turbopuffer needs to track indexing jobs.

However, there’s still a problem. In any turbopuffer region, tens or hundreds of clients will contend over the single queue object as new data is written to many namespaces.

Since CAS ensures strong consistency by forcing each write to be non-overlapping in time, we can only fit `1 / ~200ms` = ~5 writes / second (and we still have the 1 RPS limit on GCS).

The problem is no longer throughput. We need fewer writers.

_Note: This design, coupled with sharding to local queues, is roughly what we had in production prior to this update. The next sections describe turbopuffer's current production indexing queue._

## Step 3: queue.json with a brokered group commit

To eliminate contention over the queue object, we introduce a stateless **broker** which is responsible for all interactions with object storage. All clients must now liaise with the broker instead of writing to object storage directly.

The broker runs a single group commit loop on behalf of _all_ clients, so no one contends for the object. Critically, it doesn't acknowledge a write until the group commit has landed in object storage. No client moves on until its data is durably committed.

Now the broker is the bottleneck, but a single broker process can serve hundreds or thousands of clients without breaking a sweat because the writes are so small. It's just holding open connections and buffering requests in memory while waiting on I/O. Object storage does the heavy lifting.

```
queue.json
 ┌───────────────────────────────────────────────────────────────────────────────────┐
 │ {"jobs": ["◐", "◐", "◐", "◐", "○", "○", "○", "○", "○", "○", "○", "○", "○", "○",]} │
 └───────────────────────────────────────────────────────────────────────────────────┘
                                            ▲
                                            │
                                            │ brokered group commit
                                            │
 ╔═ broker ═════════════════════════════════╧════════════════════════════════════════╗
 ║                                                                                   ║
 ║  ┌─ buffer ────────────────────────────────────────────────────────────────────┐  ║
 ║  │ ┌───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┐       │  ║
 ║  │ │ ◐ │ ◐ │ ◐ │ ◐ │ ◐ │ ◐ │ ◐ │ ◐ │ ◐ │ ○ │ ○ │ ○ │ ○ │ ○ │ ○ │ ○ │ ○ │       │  ║
 ║  │ └───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┘       │  ║
 ║  └─────────────────────────────────────────────────────────────────────────────┘  ║
 ║                                                                                   ║
 ╚═══════════════════════════════════════════════════════════════════════════════════╝
         ▲          ▲          ▲                         ▲          ▲          ▲
         │          │          │                         │          │          │
    ┌────┴───┐ ┌────┴───┐ ┌────┴───┐                ┌────┴───┐ ┌────┴───┐ ┌────┴───┐
    │ worker │ │ worker │ │ worker │                │ pusher │ │ pusher │ │ pusher │
    └────────┘ └────────┘ └────────┘                └────────┘ └────────┘ └────────┘
```

```
queue.json
┌─────────────────────────────────┐
│ {"jobs":["◐","◐","◐","○","○",]} │
└─────────────────────────────────┘
                ▲
                │ brokered
                │ group commit
                │
╔══ broker ═════╧═════════════════╗
║  ┌─ buffer ───────────────────┐ ║
║  │ ┌───┬───┬───┬───┬───┬───┐  │ ║
║  │ │ ◐ │ ◐ │ ◐ │ ○ │ ○ │ ○ │  │ ║
║  │ └───┴───┴───┴───┴───┴───┘  │ ║
║  └────────────────────────────┘ ║
╚════════╤═══════════════╤════════╝
         │               │
    ┌────┴────┐     ┌────┴────┐
    │ workers │     │ pushers │
    └─────────┘     └─────────┘
```

That's it for scaling. The system can now handle turbopuffer's indexing traffic. But we need high-availability.

## Step 4: queue.json with an HA brokered group commit

The broker's machine might die at any time. Similarly, some worker might claim a job and then never finish it. The fix for each of these has the same shape — notice when something is gone and hand off the responsibility — but the details differ.

If any request from a client to the broker takes too long, we start a new broker. Clients will need a way to find the new broker, so we write the broker's address to `queue.json`.

The broker is stateless, so it's easy and inexpensive to move. And if we end up with more than one broker at a time? That's fine: CAS ensures correctness even with two brokers. The previous broker eventually discovers it's no longer the broker when it gets a CAS failure on `queue.json`. The only downside is a bit of contention, and thus slowness, for this brief duration.

For the job claims, we add a heartbeat. Periodically, the worker will confirm that it's still on track by sending the broker a timestamp, which is then written to `queue.json` for that job (one heartbeat per claimed job). If the last heartbeat for a job in the queue is ever more than some timeout, we assume the original worker is gone and the next worker takes over where it left off.

```
queue.json
  ┌──────────────────────────────────────────────────────────────────────────────────┐
  │ {                                                                                │ read
  │   "broker": "10.0.0.42:3000",                                                    │◀──┐
  │   "jobs": ["◐(♥)", "◐(♥)", "◐(♥)", "◐(♥)", "◐(♥)", "○", "○", "○", "○", "○",]     │   │
  │ }                                                                                │   │
  └──────────────────────────────────────────────────────────────────────────────────┘   │
                                            ▲                                            │
                                            │                                            │
                                            │ brokered group commit                      │
                                            │                                            │
 ╔═ broker ═════════════════════════════════╧════════════════════════════════════════╗   │
 ║                                                                                   ║   │
 ║  ┌─ buffer ────────────────────────────────────────────────────────────────────┐  ║   │
 ║  │ ┌───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┬───┐       │  ║   │
 ║  │ │ ◐ │ ◐ │ ◐ │ ◐ │ ◐ │ ○ │ ○ │ ○ │ ○ │ ○ │ ○ │ ○ │ ○ │ ○ │ ○ │ ○ │ ○ │       │  ║   │
 ║  │ └───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───┘       │  ║   │
 ║  └─────────────────────────────────────────────────────────────────────────────┘  ║   │
 ║                                                                                   ║   │
 ╚═══════════════════════════════════════════════════════════════════════════════════╝   │
         ▲          ▲          ▲                         ▲          ▲          ▲         │
         │          │          │                         │          │          │         │
    ┌────┴───┐ ┌────┴───┐ ┌────┴───┐                ┌────┴───┐ ┌────┴───┐ ┌────┴───┐     │
    │ worker │ │ worker │ │ worker │                │ pusher │ │ pusher │ │ pusher │─────┘
    └────────┘ └────────┘ └────────┘                └────────┘ └────────┘ └────────┘
```

```
queue.json
┌─────────────────────────────────┐
│  {                              │
│   "broker":"10.0.0.42:3000",    │
│   "jobs":["◐(♥)","◐(♥)","○",]   │
│  }                              │
└─────────────────────────────────┘
                ▲               ▲
       brokered │          read │
   group commit │               │
                │               │
╔══ broker ═════╧═════════════════╗
║  ┌─ buffer ───────────────────┐ ║
║  │ ┌───┬───┬───┬───┬───┬───┐  │ ║
║  │ │ ◐ │ ◐ │ ○ │ ○ │ ○ │ ○ │  │ ║
║  │ └───┴───┴───┴───┴───┴───┘  │ ║
║  └────────────────────────────┘ ║
╚════════╤═══════════════╤════════╝
         │               │      │
    ┌────┴────┐     ┌────┴────┐ │
    │ workers │     │ pushers │─┘
    └─────────┘     └─────────┘
```

## Ship it

We built a reliable distributed job queue with just a single file on object storage and a handful of stateless processes. It easily handles our throughput, guarantees at-least-once delivery, and fails over to any node as needed. Those familiar with [turbopuffer's core architecture](https://turbopuffer.com/docs/architecture) will see the parallels. Object storage offers few, but powerful, primitives. Once you learn how they behave, you can wield them to build resilient, performant, and highly scalable distributed systems with what's already there.
