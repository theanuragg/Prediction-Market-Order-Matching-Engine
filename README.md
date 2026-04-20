# Prediction Market — Order Matching Engine

A toy order matching engine for a prediction market written in Rust.  
Users submit limit orders via HTTP. Orders are matched by **price-time priority**.  
Matched fills are broadcast in real time over WebSocket.  
The system runs correctly with **any number of API server instances** running simultaneously.

---

## Table of Contents

1. [Quick Start](#quick-start)
2. [Architecture Overview](#architecture-overview)
3. [The Distributed Correctness Problem](#the-distributed-correctness-problem)
4. [Data Model](#data-model)
5. [HTTP API Reference](#http-api-reference)
6. [WebSocket Feed Reference](#websocket-feed-reference)
7. [Matching Engine Semantics](#matching-engine-semantics)
8. [Crate Structure](#crate-structure)
9. [Redis Key Layout](#redis-key-layout)
10. [Configuration](#configuration)
11. [Running Tests](#running-tests)
12. [Design Decisions & Trade-offs](#design-decisions--trade-offs)
13. [What Is Not Built (and Why)](#what-is-not-built-and-why)
14. [Production Scaling Path](#production-scaling-path)

---

## Quick Start

### Prerequisites

- [Rust toolchain](https://rustup.rs/) (stable, 1.77+)
- [Docker + Docker Compose](https://docs.docker.com/compose/)

### Run everything with Docker Compose

```bash
git clone <repo>
cd prediction-market
docker compose up --build
```

This starts:
- `redis` — the serialization backbone on port 6379
- `api-server-1` — HTTP + WebSocket server on port 8080
- `api-server-2` — second HTTP + WebSocket server on port 8081 (proves multi-instance correctness)
- `matcher` — the single-writer matching worker (no HTTP port, internal only)

### Smoke-test the system

```bash
# Submit a sell order via instance 1
curl -s -X POST http://localhost:8080/orders \
  -H 'Content-Type: application/json' \
  -d '{"side":"sell","price":100,"qty":10}' | jq
# → {"order_id": 1}

# Submit a buy order via instance 2 (different server instance!)
curl -s -X POST http://localhost:8081/orders \
  -H 'Content-Type: application/json' \
  -d '{"side":"buy","price":100,"qty":6}' | jq
# → {"order_id": 2}

# Check the orderbook (partial fill: sell has 4 remaining)
curl -s http://localhost:8080/orderbook | jq
# → {"bids":[],"asks":[{"price":100,"qty":4}]}

# Open a WebSocket listener (run before submitting orders to see fills live)
websocat ws://localhost:8080/ws/fills
# or: websocat ws://localhost:8081/ws/fills  (either instance works)
```

### Run without Docker (development)

```bash
# Terminal 1: Redis
docker run --rm -p 6379:6379 redis:7-alpine

# Terminal 2: Matching worker
REDIS_URL=redis://127.0.0.1:6379 cargo run -p matcher

# Terminal 3: API server instance 1
REDIS_URL=redis://127.0.0.1:6379 PORT=8080 cargo run -p api-server

# Terminal 4: API server instance 2 (optional, proves multi-instance)
REDIS_URL=redis://127.0.0.1:6379 PORT=8081 cargo run -p api-server
```

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                        CLIENTS                              │
│          HTTP (POST /orders, GET /orderbook)                │
│          WebSocket (/ws/fills)                              │
└────────────┬──────────────────────────────┬────────────────┘
             │                              │
   ┌─────────▼──────────┐       ┌──────────▼──────────┐
   │   api-server :8080  │       │  api-server :8081   │
   │   (stateless Axum)  │       │  (stateless Axum)   │
   └─────────┬──────────┘       └──────────┬──────────┘
             │  XADD                        │  XADD
             │  INCR                        │  INCR
             │  HGETALL                     │  HGETALL
             │  SUBSCRIBE                   │  SUBSCRIBE
             └──────────────┬───────────────┘
                            │
               ┌────────────▼────────────┐
               │          REDIS           │
               │                         │
               │  orders_stream          │  ← XADD from any API
               │  fills_channel (pubsub) │  ← PUBLISH from matcher
               │  orderbook:bids (hash)  │  ← HSET from matcher
               │  orderbook:asks (hash)  │  ← HSET from matcher
               │  order_id_seq           │  ← INCR from any API
               └────────────┬────────────┘
                            │ XREADGROUP BLOCK
                            │ (single consumer)
               ┌────────────▼────────────┐
               │   matcher (1 process)   │
               │                         │
               │  BTreeMap in-memory     │
               │  price-time priority    │
               │  partial fills          │
               │  PUBLISH fills          │
               │  XACK when done         │
               └─────────────────────────┘
```

The API servers are **completely stateless**. They do three things:

1. **Receive** an order → atomically get an ID (`INCR`) → enqueue to stream (`XADD`) → return the ID
2. **Read** the orderbook snapshot from Redis hashes (`HGETALL`)
3. **Forward** fill events from Redis PubSub to connected WebSocket clients

The matcher is the **only process that touches the order book**. It reads from the stream one message at a time, runs the matching algorithm in memory, writes fills back to Redis PubSub and updates the snapshot hashes. There is no locking, no distributed transaction, no coordination protocol between API instances.

---

## The Distributed Correctness Problem

This is the most important design question in the assignment. Here is why the chosen approach (Redis Stream + single matching worker) is correct and why alternatives fail.

### Why naive multi-instance matching is broken

Suppose two API servers each have a copy of the order book in memory:

```
Time T=0: Both books are empty.

T=1: Instance A receives SELL 10 @ 100 → inserts into its local book
T=2: Instance B receives BUY  10 @ 100 → checks its local book → empty → inserts

Result: Two unmatched resting orders that should have crossed.
This is a lost update. The market is broken.
```

Any design where matching happens inside the API server and the book state is not globally consistent will produce this failure under concurrent submission.

### Why distributed locks are subtly wrong

Redis `SETNX`-based locking (Redlock) is the first instinct. It fails for several reasons:

- **Lock holder crash**: if the process holding the lock dies mid-match, the lock expires after TTL. Any fills that were computed but not yet written are lost. There is no way to know which orders were partially processed.
- **Clock skew**: Redlock assumes synchronized clocks across nodes. It is subtly broken under clock drift (Lamport, 1978; Martin Kleppmann's critique, 2016).
- **Throughput**: every order submission must acquire a global lock, wait for any holder to release it, do the match, and release. At 10k orders/sec, the lock becomes a bottleneck and lock contention causes thundering herd on release.
- **No replay**: if the lock holder processes an order but crashes before persisting, there is no record of the order at all — the client got an ID back, but the order vanished.

### Why database serializable transactions are insufficient alone

Using Postgres `BEGIN; SET TRANSACTION ISOLATION LEVEL SERIALIZABLE; ... COMMIT` would work for correctness but has two problems for this use case:

- **Retry logic**: serializable isolation aborts and requires client-side retry on conflict. Under high concurrency, retry storms occur.
- **Performance**: every order submission becomes a read-modify-write transaction touching the entire book. The database becomes the matching engine bottleneck.

Postgres `SERIALIZABLE` is the right tool for the **settlement path** (updating user positions, marking fills as settled) but wrong for the hot matching path.

### Why Redis Stream + single writer is correct

```
Property 1 — Total order:
  Redis Streams are an append-only log with monotonically increasing IDs.
  XADD returns an ID of the form <milliseconds>-<sequence>.
  Every order ever submitted has a globally unique, totally ordered position
  in the stream. There is no ambiguity about which order came first.

Property 2 — Single consumer:
  The matcher uses XREADGROUP with a single consumer group and a single
  consumer. At any point in time, exactly one process is executing the
  match() function. There is no concurrent access to the in-memory book.
  No locking is needed because concurrent access is structurally impossible.

Property 3 — At-least-once delivery with deduplication:
  XREADGROUP delivers a message to the consumer. The consumer calls XACK
  only after the fill is published and the book snapshot is updated.
  If the matcher crashes mid-processing, the message remains in the
  Pending Entries List (PEL). On restart, XAUTOCLAIM reclaims messages
  that have been pending longer than a configured timeout and reprocesses
  them. The match() function is deterministic — replaying the same order
  against the same book state produces the same fills.

Property 4 — Atomic ID generation:
  Redis INCR is atomic by definition (single-threaded Redis event loop).
  Two API instances calling INCR simultaneously will always get distinct
  sequential values. Order IDs are globally unique across all instances.
```

The stream is the coordination mechanism. The API servers do not need to know about each other. They simply enqueue to the same stream. The matcher processes entries in stream order. Correctness is structural, not dependent on timeouts, clocks, or lock management.

---

## Data Model

The required data model is defined in `crates/types/src/lib.rs` and cannot have fields removed.

```rust
pub enum Side { Buy, Sell }

pub struct Order {
    pub id:    u64,   // globally unique, assigned by INCR at submission time
    pub side:  Side,
    pub price: u64,   // integer ticks — no floats, no rounding errors
    pub qty:   u64,   // remaining quantity (decremented on partial fills)
}

pub struct Fill {
    pub maker_order_id: u64,  // the resting order that was on the book
    pub taker_order_id: u64,  // the incoming order that triggered the match
    pub price:          u64,  // always the maker's price (maker price governs)
    pub qty:            u64,  // quantity exchanged in this fill event
}
```

### Added fields (permitted by the assignment)

The `SubmitOrder` wire type (request body for `POST /orders`) does not have an `id` field — the server assigns the ID. It carries only `side`, `price`, `qty`.

The `PriceLevel` type (used in `GET /orderbook` response) carries `price` and `qty` as an aggregated view of all resting orders at that price level.

### Why integer ticks, no floats

Float arithmetic introduces rounding errors that are catastrophic in financial systems. A price of `0.1 + 0.2` in IEEE 754 float64 is `0.30000000000000004`, not `0.3`. By representing prices as integer ticks (e.g., tick size = 0.01 USDC, so price 100 = 1.00 USDC), all arithmetic is exact. The frontend is responsible for converting ticks to display units.

---

## HTTP API Reference

### POST /orders

Submit a new limit order. Returns the assigned order ID immediately. The order is enqueued for matching asynchronously — it may not be reflected in `GET /orderbook` for a few milliseconds while the matcher processes it.

**Request body**

```json
{
  "side":  "buy" | "sell",
  "price": <u64>,
  "qty":   <u64>
}
```

**Response 200**

```json
{
  "order_id": <u64>
}
```

**Response 422** — validation failure (price = 0, qty = 0, missing fields)

```json
{
  "error": "price must be > 0"
}
```

**Example**

```bash
curl -s -X POST http://localhost:8080/orders \
  -H 'Content-Type: application/json' \
  -d '{"side":"buy","price":99,"qty":5}'
# → {"order_id":7}
```

**Semantics**

- The response `order_id` is the canonical ID for this order. Use it to correlate fills in the WebSocket feed.
- The order enters the matching queue in stream order. If the stream is empty and the matcher is idle, the order will typically be matched within 1–2 milliseconds.
- There is no guarantee the order has been matched by the time the HTTP response is returned. This is fire-and-forget semantics on the submission side; fills are delivered over WebSocket.

---

### GET /orderbook

Returns the current state of the order book — all resting (unmatched or partially matched) bids and asks, aggregated by price level.

**Response 200**

```json
{
  "bids": [
    {"price": 101, "qty": 15},
    {"price": 100, "qty": 8},
    {"price":  99, "qty": 3}
  ],
  "asks": [
    {"price": 102, "qty": 6},
    {"price": 103, "qty": 20}
  ]
}
```

- `bids` are sorted descending by price (best bid first)
- `asks` are sorted ascending by price (best ask first)
- `qty` is the total remaining quantity across all orders at that price level
- The snapshot is updated by the matcher after each fill. It is eventually consistent with a typical lag of < 5ms.

**Example**

```bash
curl -s http://localhost:8080/orderbook | jq
```

---

## WebSocket Feed Reference

### Connect

```
ws://localhost:8080/ws/fills
ws://localhost:8081/ws/fills   (any instance works — all receive all fills)
```

No authentication required. The server sends fill events as they happen. Clients are passive listeners — the server does not process any messages sent by the client (they are ignored).

### Fill event message format

```json
{
  "maker_order_id": 3,
  "taker_order_id": 7,
  "price":          100,
  "qty":            5
}
```

Fields match the `Fill` struct exactly.

### Behaviour

- Every fill generated by the matching engine is published to **all** connected WebSocket clients across **all** API server instances simultaneously via Redis PubSub.
- If a client is not connected when a fill occurs, the fill is not buffered or replayed. Clients receive only fills that occur after they connect.
- One order submission can generate multiple fill events (e.g., a large buy that consumes multiple resting sell orders at different price levels will generate one `Fill` per resting order consumed).

### Example listener (websocat)

```bash
# Install: cargo install websocat
websocat ws://localhost:8080/ws/fills
```

---

## Matching Engine Semantics

### Price-time priority (FIFO within price level)

Orders are matched by two criteria in order:

1. **Price priority**: the best-priced resting order matches first.
   - For a BUY taker: the resting SELL with the **lowest** ask price matches first.
   - For a SELL taker: the resting BUY with the **highest** bid price matches first.
2. **Time priority**: among orders at the same price, the one that arrived **earliest** (lowest stream ID, i.e., lowest `order_id`) matches first.

### Match condition

A BUY order at price `P_buy` matches against a resting SELL at price `P_ask` if and only if `P_buy >= P_ask`.

A SELL order at price `P_sell` matches against a resting BUY at price `P_bid` if and only if `P_sell <= P_bid`.

If no resting order satisfies the condition, the incoming order rests on the book (or the remaining unfilled quantity rests).

### Partial fills

A single order may be filled in multiple steps:

```
Book state: SELL 10 @ 100, SELL 5 @ 101

Incoming: BUY 12 @ 102

Step 1: match against SELL 10 @ 100 → Fill{price:100, qty:10}
        SELL @ 100 fully consumed, removed from book
        Incoming BUY has 2 remaining

Step 2: match against SELL 5 @ 101 → Fill{price:101, qty:2}
        SELL @ 101 partially filled, 3 remaining on book
        Incoming BUY fully consumed

Result: 2 Fill events emitted
        Book: SELL 3 @ 101
```

### Fill price

The fill price is always the **maker's price** (the resting order). The taker accepts the maker's price. This is standard CLOB (Central Limit Order Book) semantics.

### Data structures

```
bids: BTreeMap<Reverse<u64>, VecDeque<Order>>
        key  = Reverse(price) so BTreeMap::iter() yields highest price first
        value = queue of orders at that price, front = oldest (time priority)

asks: BTreeMap<u64, VecDeque<Order>>
        key  = price so BTreeMap::iter() yields lowest price first
        value = queue of orders at that price, front = oldest
```

Both structures live entirely in the matcher's process memory. No database is consulted during matching. The Redis orderbook snapshot (`orderbook:bids`, `orderbook:asks`) is written after each match for `GET /orderbook` reads, but is never read by the matching algorithm itself.

### Complexity

| Operation | Complexity | Notes |
|---|---|---|
| Insert resting order | O(log P) | P = number of distinct price levels |
| Match (best level) | O(log P + F) | F = number of fills generated |
| Remove exhausted level | O(log P) | amortised |
| `GET /orderbook` | O(L) | L = total price levels, Redis HGETALL |

In practice P is small (hundreds, not millions) so all operations are fast.

---

## Crate Structure

```
prediction-market/
├── Cargo.toml              # workspace manifest
├── docker-compose.yml      # full system
├── crates/
│   ├── types/              # shared data model, wire types
│   │   └── src/lib.rs      # Order, Fill, Side, SubmitOrder, OrderAck, OrderBook
│   │
│   ├── matching/           # pure matching logic — NO I/O, NO Redis, NO Tokio
│   │   └── src/
│   │       ├── lib.rs      # pub fn match_order() → Vec<Fill>
│   │       └── book.rs     # Book struct, BTreeMap internals
│   │
│   ├── matcher/            # binary: Redis XREADGROUP → match() → PUBLISH + HSET
│   │   └── src/main.rs
│   │
│   └── api-server/         # binary: Axum HTTP + WebSocket
│       └── src/
│           ├── main.rs     # router, state, startup
│           ├── routes/
│           │   ├── orders.rs     # POST /orders
│           │   └── orderbook.rs  # GET /orderbook
│           └── ws.rs             # WS /ws/fills handler
└── README.md
```

### Dependency graph

```
types  ←──  matching  ←──  matcher
  ↑                            ↑
  └──────  api-server  ────────┘
                ↑
              (redis, axum, tokio)
```

`matching` has zero async dependencies. It is pure synchronous Rust. This is intentional — the matching logic is the most critical correctness surface and should be independently testable without any runtime, database, or network.

---

## Redis Key Layout

| Key | Type | Owner | Description |
|---|---|---|---|
| `order_id_seq` | String (integer) | API servers (INCR) | Global monotonic order ID counter |
| `orders_stream` | Stream | API servers (XADD), matcher (XREAD/XACK) | Durable ordered log of all submitted orders |
| `orderbook:bids` | Hash | Matcher (HSET/HDEL) | price → qty snapshot for bids |
| `orderbook:asks` | Hash | Matcher (HSET/HDEL) | price → qty snapshot for asks |
| `fills_channel` | PubSub channel | Matcher (PUBLISH), API servers (SUBSCRIBE) | Real-time fill fan-out |

### Stream message format

Each entry in `orders_stream` is a Redis Stream entry with fields:

```
id        <u64>           the assigned order ID
side      "buy" | "sell"
price     <u64 as string>
qty       <u64 as string>
```

### Consumer group

The matcher creates (or re-uses) a consumer group named `matching_workers` with a single consumer named `worker-0`. The `>` special ID in `XREADGROUP` means "deliver only new, undelivered messages". On restart, the matcher first calls `XAUTOCLAIM matching_workers worker-0 <min-idle-ms> 0-0` to reclaim any messages that were in-flight when the previous instance died.

---

## Configuration

All configuration is via environment variables.

### api-server

| Variable | Default | Description |
|---|---|---|
| `REDIS_URL` | `redis://127.0.0.1:6379` | Redis connection string |
| `PORT` | `8080` | HTTP listener port |
| `RUST_LOG` | `info` | Log level (trace/debug/info/warn/error) |

### matcher

| Variable | Default | Description |
|---|---|---|
| `REDIS_URL` | `redis://127.0.0.1:6379` | Redis connection string |
| `CONSUMER_NAME` | `worker-0` | Redis consumer group consumer name |
| `CLAIM_MIN_IDLE_MS` | `5000` | Min idle time before XAUTOCLAIM reclaims a message |
| `RUST_LOG` | `info` | Log level |

---

## Running Tests

### Unit tests (matching logic — no Redis required)

```bash
cargo test -p matching
```

The matching crate has extensive unit tests covering:

- Empty book (no match)
- Exact fill (buy qty == sell qty, same price)
- Partial fill (taker larger than maker)
- Partial fill (maker larger than taker)
- Multiple fills from one order (sweeps multiple price levels)
- Time priority (two orders at same price, earlier one fills first)
- Price priority (lower ask fills before higher ask)
- Buy price below best ask (no match, order rests)
- Sell price above best bid (no match, order rests)
- Fill price is always maker's price

### Integration tests (requires Redis)

```bash
docker run --rm -d -p 6379:6379 --name test-redis redis:7-alpine
cargo test -p matcher -- --test-threads=1
docker stop test-redis
```

### End-to-end test (requires full Docker Compose stack)

```bash
docker compose up -d
./scripts/e2e_test.sh   # submits orders, checks fills, verifies orderbook
docker compose down
```

The `e2e_test.sh` script specifically tests multi-instance correctness by submitting orders to alternating API instances and verifying that fills are generated correctly regardless of which instance receives which side.

---

## Design Decisions & Trade-offs

### Decision: Redis Stream as the serialization mechanism

**Chosen over**: distributed lock (Redlock), database serializable transactions, CRDTs, consensus protocol (Raft/Paxos).

**Rationale**: The matching algorithm requires total order on incoming orders — there must be an unambiguous "which order came first" answer. A Redis Stream provides this as a first-class primitive. The single consumer group + single consumer pattern makes concurrent access to the in-memory book structurally impossible without any explicit locking. Crash recovery is automatic via the Pending Entries List and XAUTOCLAIM.

**Trade-off**: There is exactly one matching worker process at a time. If the matcher crashes, matching pauses until it restarts (typically < 1 second). This is acceptable for a toy system and for single-market production systems. For multi-market systems, the correct scaling unit is one matcher per market (see Production Scaling Path).

### Decision: In-memory order book with Redis snapshot

**Chosen over**: storing the live order book in Redis sorted sets, Postgres, or RocksDB.

**Rationale**: The matching algorithm needs O(log P) access to the best bid/ask, which BTreeMap provides natively. Redis sorted sets would add network round trips to every matching step. The book state is reconstructed from the stream on restart (deterministic replay). The Redis hash snapshot is a read model for the HTTP API only — it is never consulted by the matching algorithm.

**Trade-off**: If the matcher crashes and has a large backlog of orders to replay, startup is slower. Mitigated by periodic snapshots and MAXLEN trimming on the stream.

### Decision: Fire-and-forget order submission (async fills)

**Chosen over**: synchronous submission where the HTTP response includes the fill result.

**Rationale**: The assignment specifies WebSocket for fill delivery. Synchronous submission would require the API server to wait for the matcher to process the order and reply, adding matcher latency (potentially 1–50ms) to every HTTP response. The current design returns an order ID in < 1ms (one Redis round trip for INCR + XADD) regardless of matcher load.

**Trade-off**: The client cannot know if an order was filled by looking at the HTTP response alone. Clients must connect to the WebSocket feed to observe fill events. For a production system, a separate `GET /orders/{id}` endpoint would be added to query order status.

### Decision: Exactly one matcher process, not sharded by price or user

**Rationale**: Sharding a CLOB by price range requires cross-shard communication for any order that might sweep multiple price levels. Sharding by user requires knowing all users' orders before matching. The only correct shard key for a CLOB is the market — one matcher per market, each market's book is fully independent.

### Decision: No order cancellation

**Rationale**: The assignment data model does not include cancellation. Adding it correctly (cancel must also be serialized through the stream, cancel must not race with a concurrent fill) would add significant complexity beyond the time budget. The correct approach in production is a `CancelOrder` stream message type processed by the same matcher.

---

## What Is Not Built (and Why)

| Feature | Why excluded | How to add |
|---|---|---|
| Order cancellation | Not in assignment; complex to get right | Add `CancelOrder` message type to stream |
| Order status endpoint `GET /orders/:id` | Not required; fills are on WS | Store orders in Redis hash, read by ID |
| Fill history / replay | PubSub is fire-and-forget | Store fills in a Redis Stream or ClickHouse |
| Authentication | Not required | Add API key middleware to Axum router |
| Multiple markets | Single market assumed | Shard by market_id, one matcher per market |
| Persistent fill storage | Redis pubsub is ephemeral | Write fills to ClickHouse (columnar, fast ingest) |
| On-chain settlement | Beyond toy scope | Quasar program with `settle_fill` instruction + SPL token CPI |
| Market orders | Only limit orders implied | Add `order_type` field, bypass price check |
| IOC / FOK order types | Not in scope | Add `time_in_force` field, cancel remainder after match |

---

## Production Scaling Path

This section is informational — none of this is implemented in the toy.

### Horizontal API scaling

API servers are already stateless. Add as many as needed behind a load balancer. Each instance shares the same Redis and produces identical behaviour. The load balancer does not need session affinity.

### Horizontal matching scaling (per market)

The single matching worker is the correct architecture for a single market. For N markets, run N matching workers, each with its own stream key (`orders_stream:btc-usd`, `orders_stream:eth-usd`) and its own consumer group. Each worker maintains its own in-memory book. There is zero coordination between workers — markets are fully independent.

### Fill persistence at scale

Write fills from the matcher to ClickHouse using the async insert feature. ClickHouse's `ReplicatedMergeTree` engine sorted by `(market_id, timestamp)` supports 10M+ rows/second ingest and sub-10ms analytical queries over billions of fills. This is the correct choice over Postgres for time-series fill data.

### On-chain settlement (Solana / Quasar)

For a real prediction market, fills need to settle on-chain: the winner receives the payout. The settlement flow is:

1. Matcher emits fill → writes to Redis + to a settlement queue
2. Settlement worker reads from queue → builds a Solana transaction calling the Quasar `settle_fill` instruction
3. Quasar program verifies the fill (signed by the matcher's keypair), executes an SPL token transfer from the loser's escrow to the winner's wallet

Quasar (not Anchor) is preferred because its zero-copy account model means the `settle_fill` instruction consumes minimal compute units, reducing transaction fees at high fill volume. The Quasar CU profiler flamegraph makes it possible to identify and eliminate CU waste before mainnet deployment.
