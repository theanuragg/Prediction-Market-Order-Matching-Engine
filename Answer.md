# Take-Home Assignment Answers

## Question 1: How does your system handle multiple API server instances without double-matching an order?

The system uses **Redis Streams as a globally-ordered append-only log + a single matching worker**:

1. **All API servers are stateless**. They do NOT hold or match orders locally. Instead, they:
   - Atomically increment `order_id_seq` to get a unique ID
   - Append the order to `orders_stream` via `XADD` (atomic, ordered by Redis)
   - Return the ID to the client

2. **Single matcher worker** reads from the stream with `XREADGROUP`:
   - `XREADGROUP` ensures exactly one consumer processes each message
   - At any point, only ONE process is running the matching algorithm
   - There is no concurrent access to the in-memory order book by design (not by locking)

3. **Total order guarantee**: Redis Streams have monotonically increasing IDs (milliseconds-sequence). Every order has a globally unique position. No ambiguity about causality.

**Why this avoids the distributed correctness problem:**
- Naive multi-instance matching fails because local books diverge (lost updates)
- Distributed locks fail because they're bottlenecks + suffer clock skew + crash-during-processing issues
- Serializable DB transactions fail because they cause retry storms under high concurrency
- Single-writer + ordered stream succeeds because there is **no concurrent access to the critical section**

**Fault tolerance:**
- If the matcher crashes mid-processing, the message remains in the Pending Entries List (PEL)
- On restart, `XAUTOCLAIM` redelivers the message (idempotent — fills may be published twice, but they're the same fill, so clients see correct state)

---

## Question 2: What data structure did you use for the order book and why?

```rust
pub struct Book {
    bids: BTreeMap<Reverse<u64>, VecDeque<Order>>,
    asks: BTreeMap<u64, VecDeque<Order>>,
}
```

**Why BTreeMap + VecDeque:**

| Layer | Structure | Why |
|-------|-----------|-----|
| **Bids** | `BTreeMap<Reverse<u64>, VecDeque<Order>>` | Reverse sorts descending (best bid first). O(log N) lookup of best bid. |
| **Asks** | `BTreeMap<u64, VecDeque<Order>>` | Natural ascending order (best ask first). O(log N) lookup of best ask. |
| **Within level** | `VecDeque<Order>` | FIFO (price-time priority). O(1) pop_front (oldest = fill first), O(1) push_back (new order rests). |

**Matching performance:**
- Best bid/ask lookup: O(log N) where N = number of price levels
- Match loop: O(k log N) where k = number of levels crossed (typically 1-3 in liquid markets)
- Worst case (very illiquid): O(L log N) where L = total orders in book (but this is rare)

**Why NOT alternatives:**
- Heap (PriorityQueue): Can't efficiently remove arbitrary levels; would be O(L log L) per match
- LinkedList: No ordering, O(N) scan for best price
- Vec (sorted): O(N) insertion/deletion to maintain sorted order
- HashMap alone: No ordering, can't efficiently find best price

**Why NOT skip the VecDeque level:**
- Could flatten to `BTreeMap<u64, u64>` (price → total qty)
- But then we lose FIFO within level — multiple orders at $100 wouldn't match in submission order
- Price-time priority requires tracking individual orders at each level

**Correctness guarantee:**
- BTreeMap iterator order is stable and sorted
- VecDeque FIFO is deterministic
- Result: matching is **fully deterministic** (same input order always produces same output)

---

## Question 3: What breaks first under production load?

The **single matcher worker** becomes the bottleneck, not Redis or the API servers.

**Capacity analysis:**
- **Redis**: 1M+ ops/sec (handles 100k orders/sec easily; each order is ~3 Redis ops: INCR, XADD, PUBLISH)
- **Axum API servers**: Can saturate network before CPU (stateless, accept orders faster than matcher can process)
- **Matcher**: Single-threaded, CPU-bound. At O(k log N) per order with 100k orders/sec = ~100M primitive operations/sec. Hits CPU ceiling.

**At ~100k orders/sec with deep books (N = 10k levels):**
- Each order match = O(k log N) ≈ O(10 × 13) = ~130 operations
- 100k orders × 130 ops = 13M ops/sec (feasible on modern CPU)
- But add Redis I/O latency, context switching, GC: matcher becomes bottleneck

**Solution: Per-market sharding**
- Run N independent matcher instances (one per market/instrument)
- Each has its own `orders_stream:{market}`, `orderbook:{market}` in Redis
- Zero coordination between matchers (fully parallelizable)
- Horizontal scaling: 10 matchers = 1M orders/sec capacity

**Redis then becomes the limit** (but you get there at much higher throughput with sharding).

---

## Question 4: What would you build next with 4 more hours?

In priority order (by impact × implementation time):

### 1. **Order cancellation** (1 hour)
**Why:** Orders should be cancellable; traders expect this.

**Implementation:**
- Add `CancelOrder { order_id: u64 }` to the stream message type
- Matcher processes cancellations the same way as matches (total order preservation)
- When processing cancel: remove order from book if found, publish cancellation event
- Idempotent: canceling twice = no-op (order already gone)

**Code impact:** ~30 lines in matcher, ~20 lines in types

### 2. **Fill history endpoint** (1 hour)
**Why:** Clients need to query past fills (audit trail, position reconstruction).

**Implementation:**
- Store each fill in a new Redis Stream: `fills_stream`
- Add `GET /fills?since=<timestamp>&limit=100` endpoint
- Return paginated fills sorted by time
- Optional: store user_id with fill for per-user queries (requires extending types)

**Code impact:** ~40 lines

### 3. **Order status / open orders endpoint** (0.5 hour)
**Why:** Traders want to know if their order is resting or filled.

**Implementation:**
- Add `GET /orders/{id}` endpoint
- Query current orderbook snapshot to see if order is still resting
- Return `{ status: "resting" | "partially_filled" | "filled", remaining_qty, filled_qty }`
- Cache state in Redis `order:{id}` with TTL (expires after 24h)

**Code impact:** ~30 lines

### 4. **Prometheus metrics** (1.5 hours)
**Why:** Production needs observability (what's actually happening).

**Implement counters/gauges:**
- `orders_submitted_total` — counter
- `fills_generated_total` — counter  
- `order_match_latency_seconds` — histogram (matcher loop time)
- `orderbook_depth` — gauge (bids/asks count at each level)
- `matcher_queue_depth` — gauge (pending orders in stream)

**Code impact:** ~60 lines (instrument main loop + add prometheus crate)

### Why I skip these (not worth the time):

- ❌ **Persistence to Postgres**: Redis Stream + snapshot is good enough for a toy system; adds complexity
- ❌ **Authentication**: No users to authenticate against; out of scope
- ❌ **Market orders**: Adds matching complexity; not worth it in 4 hours vs. the above
- ❌ **API docs (OpenAPI/Swagger)**: Nice-to-have but not critical functionality
- ❌ **Load testing**: Could be useful but doesn't add features users see

**Total time: ~4 hours. Result: a system traders can actually use (cancel orders, see history, monitor).**