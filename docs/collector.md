# ModernWeather — the collector: multi-node ingest and forwarding

**Work item:** W7. **Produces:** this document, including the wire protocol and the spool schema.
**Depends on:** W0 (`docs/DESIGN.md`), W2a (`docs/component-model.md`), W3 (`docs/service-architecture.md`),
W2 (`docs/metric-registry.md`). **Consumed by:** W5 (what the operator sees), W9 (migrating the
existing fleet), W6 (reconciliation).

This document owns the **envelope and the RPCs**: `Provenance`'s wire encoding and `OriginId`
allocation, the spool's on-disk format, the ack/resume state machine, admission control for peers,
and the configuration shape that makes a node's position in a topology legible. It writes no
implementation code.

It does **not** own the component model (W2a), the pipeline internals, buffer sizes or crate layout
(W3), the reading's *field* layout or the metric registry (W2), the database schema (W4), or anything
user-facing (W5).

---

## Three things to read before §0

### 1. `OnReading { NotFrom(self) }` on the uplink is wrong, and it is wrong in the direction that sends nothing

W2a §4.1 and W3's assumption 44 both give the collector uplink the trigger
`OnReading { NotFrom(self), Durable, 10 s }`. Taken literally, that filter drops **every** envelope in
the node's spool:

- W3 §2.2 defines `path` as "every `OriginId` this envelope has traversed, **including `origin`**".
- `Ingress::admit` therefore produces `path = {self}` for a locally measured reading.
- An envelope that arrived from a peer got there by the peer calling `Envelope::forward(me)`, which
  inserted *me* into the path before it was sent.

So `has_visited(self)` is `true` for every envelope that can ever be in the local spool, and
`ReadingFilter::NotFrom(self)` — implemented per W3 assumption 42 as `!prov.has_visited(origin)` —
matches nothing. **The filter is per-peer: `NotFrom(peer.node_id)`**, where `peer` is the destination
of that uplink. One component per peer, one filter per component (§1.4).

This is not a difference of taste and it is not a renaming; it is the kind of thing the output
contract's assumptions section exists to catch, so it is recorded in **Assumptions about other work
items** 1 and 2 as well as here. Everything below is written against `NotFrom(peer)`.

### 2. Two mechanisms the W7 prompt names are replaced, with the reasoning stated

Both are mechanisms, not goals; both goals are met.

- **`IngestAck { origin, through_seq }` becomes an ack in the *sender's* coordinate space.** A
  cumulative per-origin ack cannot advance a relay's spool cursor, because a relay's log interleaves
  envelopes from several origins and a cursor is a position in one log. §4.2 derives the replacement
  and keeps a per-origin high-water vector where it is actually useful — fast resume at stream open.
- **`UNIQUE (origin, seq)` with `ON CONFLICT DO NOTHING` on the collector's reading table becomes a
  dedup gate in front of the spool.** The ack means "durably spooled", not "written to PostgreSQL"
  (§4.1) — if it meant the latter, every satellite's ack would be gated on the collector's database,
  which is the coupling W3 §2 removed. A constraint that fires after fan-out is also too late: a
  duplicate that reaches the spool has already been forwarded to downstream peers. §4.4 puts
  idempotency at admission and keeps a database-level backstop on the natural key.

### 3. The vocabulary here is internal

`Envelope`, `Provenance`, `OriginId`, `NodeId`, `Spool`, `Cursor`, `uplink`, `relay`, `enrolment` are
names chosen for Rust source and for a TOML rendering of configuration rows. Per D-36 and W2a §2,
**W5 owns the translation and an internal term on screen is a bug.** What this document owes W5 is
structure: §7's per-peer numbers exist so that "satellite-a is 4 hours and 29,000 readings behind"
is a rendering of two gauges rather than a thing an operator has to infer from a log.

---

## 0. The role model

### 0.1 There is no collector mode, because there is no collector field

The owner's words are *"we didn't have a clear model for a RW station acting as a collector for other
RW stations."* The reason there is no model is visible in two type declarations:

| Direction | Go implementation | Interface it implements |
|---|---|---|
| **egress** — send my readings to another node | `internal/storage/grpcstream` | `StorageEngineInterface.StoreReading` — being a source for someone else is modelled as *storing* |
| **ingress** — receive readings from other nodes | `internal/weatherstations/grpcreceiver` | `WeatherStation` — being a sink is modelled as *a device* |

Neither is wrong by accident; each reached for the nearest existing interface, which is exactly what
D-31 is a decision about. The consequence is that the two directions share no type, the collector
role has no name anywhere in the tree, and a node cannot describe its own position in a topology.

Under W2a the two directions are **components with transports**, and that is the whole of the role
model:

| Component | W2a shape | Trigger | Requires | Provides |
|---|---|---|---|---|
| `ingest:grpc` | `Transport::GrpcIn { mount }` · `Session::None` · `Codec` = generated `CollectorV1` · `StationIdentity::FromRegistration` | `Service` | `StationRegistry` H W, `ReadingStream` H W | `HttpRoutes` |
| `ingest:rest` | `Transport::HttpIn { mount: "/v1/ingest" }` · `Session::None` · same codec, JSON encoding · `FromRegistration` | `Service` | `StationRegistry` H W, `ReadingStream` H W | `HttpRoutes` |
| `uplink:<peer>` | `Transport::GrpcOut { streaming: true }` | `OnReading { NotFrom(peer), Durable, 10 s }` | `PeerRegistry` H (§5.1) | — |

> **A collector is a node whose components include an ingest endpoint. A satellite is a node whose
> components include an uplink. Same binary, same code path, different configuration rows.**

So "both a source of its own readings and a sink for others" is not a mode: it is a node that has
stations, an `ingest:*`, and an `uplink:*`, and nothing in `plan()` treats it specially. There is no
`role` column, no `mode` flag and no `is_collector` boolean anywhere in §8's configuration — which is
the property that makes the legibility claim testable: you can read a node's position off its
component list, and `multica`-style tooling aside, so can `plan()`.

### 0.2 `FromRegistration` is already the ingest side

W2a §4a.4 lists `StationIdentity::FromRegistration` — "a peer registers and is issued an id it
presents thereafter" — as one of five identity strategies, and W2a Decision 45 makes the
`RemoteStationRegistry` bolt-on into the registration path of the general `StationRegistry`. That is
exactly the ingest side of a collector, and it needs **no new machinery**:

- `GrpcIn` / `HttpIn` are existing transport variants that imply `Trigger::Service` and provide
  `HttpRoutes` (W2a §4a.3).
- Cardinality is `N`, because `identity != Fixed` (W2a Decision 42). One `ingest:grpc` component
  serves the whole fleet; a new peer is a configuration row, not a component and not a port.
- Identity resolution is the single authentication point and runs before a reading reaches the
  stream (W2a Decision 46), which is where §5.4's admission control lives.

Two names change, and only one of them is substantive:

- **`station:grpcrx` → `ingest:grpc`.** Substantive: the `station:` prefix is the weather-station
  framing this work item exists to remove, and the prompt's **Do not** section forbids preserving it
  for either direction. A peer is not a station; it is a node that *has* stations.
- **`uplink:collector:<peer>` → `uplink:<peer>`.** Cosmetic. Either spelling is fine; W6 may keep
  W2a's.

### 0.3 What the uplink's transport and trigger mean together

W2a §4a.3's table implies `Trigger::Service` for `GrpcOut { streaming: true }`. That table is scoped
to **station** components (§4a decomposes "driver"), and the uplink is not one. The uplink is
`OnReading` and *also* holds a long-lived `GrpcOut` stream, because the two describe different things:

- the **trigger** is how work arrives — a batch of envelopes from the spool cursor;
- the **transport** is a resource the component owns across ticks, opened in `build` and re-opened by
  the state machine in §4.3.

This is the same shape as `writer:timescale`, which is `OnReading` and owns a connection pool handle.
Recorded as assumption 4 so the scoping is greppable rather than inferred.

---

## 1. Provenance, and loop prevention as an invariant

### 1.1 The defect, measured

`sendReading` calls `getGRPCReceiverDevices()` — a full `LoadConfig()` plus a scan of every device —
**on every single reading** (`internal/storage/grpcstream/client.go:373-386`, calling `:187-199`), in
order to drop readings whose `StationName` matches the name of a local `grpcreceiver` device:

```go
// client.go:375
grpcReceivers, err := c.getGRPCReceiverDevices()
...
for _, receiver := range grpcReceivers {
    if reading.StationName == receiver {
        return nil // Not an error, just filtered
    }
}
```

`convertToInternalReading` preserves the **origin's** station name
(`grpcreceiver/station.go:194-196`), so the guard fires only when an incoming station's name happens
to coincide with a local receiver device's name. Name them differently — which is the normal case,
since one is a station name and the other is a device name — and a mutual uplink or any A→B→C→A cycle
is not stopped. It is a configuration query per reading enforcing a naming convention rather than an
invariant, and multi-hop relay is not merely unsafe but *inexpressible*, because there is nowhere in
`WeatherReading` to record a hop.

### 1.2 The type, as W3 landed it

W3 §2.2 already made this structural, and this document does not restate it except to fix what it
owns:

```rust
// mw-types, as landed by W3 §2.2
pub struct Provenance {
    origin: OriginId,       // the node that first admitted this reading. Never rewritten.
    path:   PathSet,        // every OriginId this envelope has traversed, including `origin`
    seq:    Seq,            // §1.5 — this document narrows what it counts
}

impl Envelope {
    /// The ONLY way to produce an envelope for forwarding to a peer.
    pub fn forward(mut self, hop: OriginId) -> Result<Self, LoopDetected> { /* … */ }
}
```

`Provenance`'s fields are private to `mw-types`; there is no public constructor, no `set_origin`, no
`Default`. So:

> **The loop invariant: an envelope addressed at peer `P` can only be obtained from
> `Envelope::forward(P)`, and `forward` refuses when `P` is already on the path.** A cycle is not
> detected, it is unrepresentable — the value that would have to exist cannot be constructed.

What W7 adds is the receive-side half, because the sender's `forward` is only trustworthy if the
sender runs this code:

> **On accept, an envelope is rejected unless `path` contains the sending peer and does not contain
> this node** (§4.4 step 2). A peer that forged a path, or a Go-era peer that has no path at all
> (§9.2), is caught here.

Two independent guards, one of them a compile-time property of the type, and neither consults
configuration.

### 1.3 `NodeId` and `OriginId` — the allocation this document owns

W3 assumption 41 leaves "`Provenance`'s wire encoding and `OriginId` allocation" to W7, and the split
is the point:

| | `NodeId` | `OriginId` |
|---|---|---|
| What | the globally unique, permanent identity of a node | a dense local index for that node |
| Form | 128-bit UUIDv7, 16 bytes fixed on the wire | `u16` |
| Allocated | **once, by the node itself, at first start**, and written to `[node].id` | on first sight of a `NodeId`, by the node doing the interning |
| Scope | global and stable forever | **local to one node's spool**; node A's `OriginId` 3 and node B's `OriginId` 3 are unrelated |
| Where it lives | the config database, and `spool/MANIFEST` | `spool/MANIFEST`'s origin table |
| Reused | never | never — like a metric column (D-21) |

**The wire carries `NodeId`s; the spool and memory carry `OriginId`s.** That is forced rather than
chosen: a local index is meaningless to a peer, and a 16-byte id per path element in a bitset is
wasteful for a structure whose entire purpose is a membership test. The translation happens exactly
twice, at encode and at decode, and interning an unseen `NodeId` on decode is the only place the
origin table grows.

```
spool/MANIFEST (little-endian, CRC32C-protected, rewritten atomically via write-new + rename + fsync-dir)
  magic           "MWSPOOL\0"
  format_version  u16              = 1
  node_id         [u8; 16]         this node's NodeId
  origin_count    u16
  origins[]       { origin_id: u16, node_id: [u8;16], first_seen: i64, hw_seq: u64 }
```

`hw_seq` is the per-origin high-water sequence used by §4.4's dedup gate, kept here so that one
fsync covers both the origin table and the dedup state.

**Two bounds, because both are growth driven by an untrusted peer.**

| Bound | Value | On exceeding |
|---|---|---|
| `MAX_PATH` — path elements on an accepted envelope | **8** | `Reject::PathTooLong`, counted, envelope dropped, batch still acked |
| `MAX_ORIGINS` — entries in the origin table | **1024** | `Reject::OriginTableFull`, counted, envelope dropped, component `Running { degraded }` |

Eight hops is three more than any topology this system has or plausibly grows into, and an uncapped
path is both an unbounded allocation driven by a peer and an unbounded intern-table growth. 1024
origins at 26 bytes each is a 26 KiB manifest; the cap exists so that a peer cannot make the manifest
grow without limit, and it is loud rather than silent because reaching it means either a fleet far
larger than designed for or a misbehaving peer.

**A note for W3 and W6, not a change.** With `MAX_PATH = 8`, W3 §2.2's 64-bit-bitset-plus-overflow
`PathSet` is carrying an optimisation it does not need: a `SmallVec<[OriginId; 8]>` with a linear
scan is smaller, has no overflow case and therefore no correctness cliff to guard, and is what the
W7 prompt's own sketch used. W3 owns the in-memory type, so this is recorded as an observation
(assumption 3) rather than taken.

### 1.4 The forwarding check runs in the component, with no configuration lookup

```
uplink:<peer>, per batch from the spool cursor:
  for env in batch:
      if env.provenance.has_visited(peer.origin_id):      # the filter, as an optimisation
          cursor_advance_without_send(env)                 # counted: uplink_filtered_total{peer}
          continue
      match env.forward(peer.origin_id):                   # the invariant
          Ok(fwd)            => stage(fwd)
          Err(LoopDetected)  => { loop_refused_total{peer} += 1; cursor_advance_without_send(env) }
```

Three properties worth stating because each closes something:

1. **No configuration read.** `peer.origin_id` is resolved once in `build` from the peer registry
   handle (W2a §2.3 — typed handles, resolved once). The per-reading `LoadConfig()` has no analogue.
2. **A filtered or refused envelope advances the cursor.** This is load-bearing and easy to get
   wrong: a cursor that stalls on an envelope it will never send stalls *forever*, the spool never
   reclaims, and `max_lag` eventually abandons the cursor. A filtered envelope is "durably handled"
   in W3 `Sink::deliver`'s sense and counts toward `Acked(n)`.
3. **The filter is an optimisation; `forward` is the invariant.** Removing the filter would cost
   throughput, not correctness. Removing `forward`'s check is impossible without changing
   `mw-types`.

### 1.5 `seq` counts per origin node, not per (origin, station)

W3 §2.2 annotates `seq` as "monotonic per `(origin, station)`". This document narrows it:

> **`seq` is monotonic per `origin` alone, and is the origin node's spool append position.**
> `(origin, seq)` is therefore the globally unique, immutable identity of an envelope, allocated
> exactly once, by the node that first admitted it.

Three reasons, in order of weight:

1. **A cumulative ack needs one sequence per log.** §4.2's ack is cumulative — "everything at or
   below this point is durable" — because that is the only ack shape a cursor can consume. With
   per-`(origin, station)` sequences, "cumulative" is a vector keyed by every station the origin has
   ever had, which grows when a satellite gains a station and must be persisted per peer per station
   on both sides. With one sequence per origin it is a `u64`.
2. **The spool is one log.** A per-station counter is extra persisted state with no consumer: the
   spool's own append order already *is* a per-origin monotonic sequence, so deriving `seq` from it
   costs nothing and cannot drift from it.
3. **Resume is then one number.** "Resume from the last acked `seq`" is literally a seek, which is
   what the prompt asks for.

Gaps in a node's `seq` are **visible and permitted** — a batch refused at the spool ceiling (§3.5)
leaves one — and reuse is not: the counter is persisted with the spool and only ever increases.
Recorded as assumption 2.

---

## 2. Multi-hop relay, worked

Multi-hop is the capability the current design cannot express. Under §1 it needs no new mechanism, so
the only thing to demonstrate is that the invariant holds in the three topologies that matter.

### 2.1 A chain: A → B → C

**First, the receive check, because the naive reading of it is wrong.** `forward(B)` inserts **the
destination**, so by the time B receives the envelope the path already contains B. So "reject if the
path contains me" — the obvious formulation — rejects everything. What B must actually establish is
that the envelope has not been here *before*, and that is not expressible as a membership test,
because a set cannot distinguish "you were added by my sender's `forward`" from "you were here two
hops ago".

The fix is to make the path **ordered** and carry the hop explicitly:

```proto
message Envelope {
  bytes  origin       = 1;   // NodeId, 16 bytes
  uint64 seq          = 2;
  repeated bytes path = 3;   // NodeIds, oldest first, path[0] == origin, len <= 8
  // path.last() is the destination this hop is addressed to; the sender is path[len-2]
  ...
}
```

With `path` **ordered** rather than a bare set, the receive check is exact and cheap:

```
accept check, per envelope, at node S:
  1. path.len() in 1..=MAX_PATH                         else Reject::PathTooLong
  2. path[0] == origin                                  else Reject::MalformedPath
  3. path.last() == S.node_id                           else Reject::NotAddressedHere
  4. path[..len-1] does not contain S.node_id           else Reject::Loop            ← the loop guard
  5. path[len-2] == peer.node_id  (len >= 2)            else Reject::PathForged
  6. all elements distinct                              else Reject::MalformedPath
```

`forward` applies the same length bound on the way out — it refuses at `MAX_PATH` rather than
emitting an envelope the next hop is guaranteed to reject — so `Reject::PathTooLong` on accept only
ever fires against a non-conforming peer.

Ordering the path is the one change this document makes to W3's `PathSet`, and it is what makes the
check exact. It also makes the path a **route record**, which is what W5 renders as "measured at
satellite-a, relayed via relay-west" and what makes a diamond's two arrivals distinguishable in a
log. Recorded as assumption 3; it is compatible with `has_visited` as a membership test, which is all
W3's `ReadingFilter::NotFrom` needs.

**Now the chain.** `A`, `B`, `C` are `NodeId`s; the list is the envelope's `path`.

| Step | Action | `origin` | `seq` | `path` |
|---|---|---|---|---|
| 1 | A admits | A | A:4711 | `[A]` |
| 2 | A's `uplink:B` — `forward(B)` | A | A:4711 | `[A, B]` |
| 3 | B accepts: checks 1–6 pass (`path.last()==B`, `path[..1]=[A] ∌ B`, `path[0]==A==peer`) | A | A:4711 | `[A, B]` |
| 4 | B spools it. B's own `seq` counter is **untouched** — `(origin, seq)` is not reassigned | A | A:4711 | `[A, B]` |
| 5 | B's `uplink:C` — filter: `has_visited(C)`? no. `forward(C)` | A | A:4711 | `[A, B, C]` |
| 6 | C accepts: `path.last()==C`, `path[..2]=[A,B] ∌ C`, `path[1]==B==peer` ✓ | A | A:4711 | `[A, B, C]` |
| 7 | C's `writer:timescale` stores it under station key `A:backyard`, `origin = A` | | | |

C's database row records that the reading was **measured at A**, which is the fact the current
implementation loses the moment the name-based guard is the only record of provenance.

### 2.2 A cycle: C → A, with A already on the path

| Step | Action | Result |
|---|---|---|
| 5′ | B also has `uplink:A` (a mutual uplink). Filter: `has_visited(A)` → **true** | cursor advances without sending; `uplink_filtered_total{peer="A"} += 1` |
| 5″ | Suppose the filter were removed: `forward(A)` on `path=[A,B]` | `Err(LoopDetected { node: A, path: [A,B] })` — the value cannot be built |
| 5‴ | Suppose a non-conforming peer forged `path=[A,B,A]` and sent it to A | A's check 4: `path[..2]=[A,B] ∋ A` → `Reject::Loop`, counted |

The A→B→C→A cycle is the same three lines with one more hop. Nothing in any of them reads
configuration, and nothing depends on what anything is named.

### 2.3 A diamond: A → {B, C} → D

This is the topology that produces a legitimate duplicate, and it is why §4.4's dedup gate exists
rather than being an optimisation.

| Step | Action | `path` on arrival at D |
|---|---|---|
| 1 | A admits `A:4711`; A has `uplink:B` and `uplink:C` | |
| 2 | B relays to D | `[A, B, D]` |
| 3 | C relays to D | `[A, C, D]` |
| 4 | D accepts the first: `hw[A] < 4711` → spooled, `hw[A] = 4711` | |
| 5 | D accepts the second: `seq 4711 <= hw[A]` → **duplicate**, dropped, `ingest_duplicates_total{origin="A"} += 1`, batch acked | |

Both arrivals pass every loop check, because neither is a loop. The second is dropped **before the
spool**, which is the part that matters: had it been dropped by a database constraint instead, it
would already have been forwarded to D's own downstream peers and published to D's `Latest`
subscribers.

### 2.4 Why multi-hop is expressible now and was not before

| | Go today | Here |
|---|---|---|
| Record of hops | none — `WeatherReading` has no field for it | `path`, ordered, on the envelope |
| Loop guard | `StationName` vs local `grpcreceiver` device names, re-read per reading | an ordered-path check plus a type with no public constructor |
| Cost per reading | one `LoadConfig()` + device scan | one `u16` membership scan over ≤ 8 elements |
| Generalises to A→B→C | no — nothing to record B in | yes |
| Fails when | two names differ (the normal case) | never, without changing `mw-types` |
| Provenance in the database | lost; the origin's station name is all that survives | `origin` and the full path travel with the envelope |

---

## 3. The spool

The spool is W3 §2's fan-out point, and W2a Decision 62 makes it the mechanism by which *any*
`Durable` sink is durable — `writer:timescale` reads the same spool from its own cursor. This
document owns the format; W3 §2.4 and §2.7 own the sizes and the policies, and they are cited rather
than restated.

### 3.1 Where it lives

```
/var/lib/modernweather/              StateDirectory, per docs/release.md §6
├── config.db                        the config SQLite database (W3 §3.1)
└── spool/
    ├── MANIFEST                     §1.3 — node id, origin table, per-origin high water
    ├── 00000000000004608.seg        segment, named by the seq of its first record
    ├── 00000000000011392.seg
    ├── 00000000000018004.seg        the open (appendable) segment
    └── cursors/
        ├── writer-timescale.cur     one file per registered cursor
        ├── uplink-suncrest.cur
        └── uplink-relay-west.cur
```

`spool/` is a sibling of `config.db` and not inside it, for three reasons: SQLite's write path is the
wrong shape for a 2 GiB append-only log; the configuration database is in `conf-files` (it survives
and is backed up across package upgrades, `docs/release.md` §6) and a 2 GiB spool must not be; and a
filesystem-level failure of the spool must be distinguishable from a failure of configuration. The
path is configurable (`[spool].path`), because an operator with an SD-card root and a USB disk needs
it to be.

### 3.2 Segment and record format

A segment is an append-only file of self-describing records. Nothing is ever updated in place.

```
segment header (32 bytes, written at creation, fsynced before the first record)
  magic            "MWSEG\0\0\0"        8
  format_version   u16 = 1              2
  reserved         u16                  2
  first_seq        u64                  8   the seq this segment starts at
  created_at       i64 (unix ms)        8
  header_crc       u32 (CRC32C)         4

record
  len              u32                  4   length of payload
  seq              u64                  8   redundant with the payload; here so a scan need not decode
  crc             u32 (CRC32C of seq ‖ payload)  4
  payload          [u8; len]                protobuf-encoded `collector.v1.Envelope`
```

**The payload is the wire message.** The record's payload is byte-for-byte the same
`collector.v1.Envelope` that §4.1 puts on the wire, generated by W2 from the metric registry
(`generated/proto/`, W3 §6.5). Three consequences, and the first is the reason for the choice:

1. **A replay after restart needs no domain type.** Recovering from the spool is `read → validate CRC
   → decode → send`, and the decode is `prost`'s, not a hand-written conversion. The 591 lines of
   hand-maintained mapping that W3 §6.5 deletes have no spool-shaped sibling to reintroduce.
2. **One schema, one evolution story.** A spool written by version *n* is read by version *n+1*
   under protobuf's own compatibility rules, which is the property that makes an in-place upgrade
   with a non-empty spool safe. `format_version` covers the framing; protobuf covers the payload.
3. **Field presence is carried.** W3 Decision 89 requires W2's generator to emit proto3 `optional`
   on every metric field; without it a `NULL` becomes `0.0` across a hop *and* across a restart,
   because the spool has the same problem the wire does. Recorded as assumption 9.

The uplink **decodes and re-encodes** rather than copying the payload bytes, because `forward` must
append a hop to `path`. At the measured rate — roughly 2 envelopes/s across ~4 stations, ~120 bytes
encoded at the measured 30 % column population — that is a few microseconds per envelope against a
10 s deadline, and the byte-copy optimisation (hoisting `path` into the record header so the payload
is immutable) is available if a measurement ever justifies it. It is not taken, because it would put
the same field in two places.

**Integrity.** CRC32C per record (hardware-accelerated on every release target) and per header.
A record whose CRC fails, or whose `len` runs past the end of the file, **truncates the segment at
that point**: everything before it is valid, everything after is discarded, and `spool_truncated_total`
is incremented with the segment name logged at `ERROR`. That is the correct behaviour for the only
way this happens in practice — a power loss mid-append — and it is why the record carries its own
length and CRC rather than relying on the file length.

### 3.3 Appending: the group commit, and what the atom is

W3 §2.4 specifies a `Mutex` plus batched `fsync`, with a **50 ms window** and a cap of **64
appends**. One clarification, because the cap is otherwise ambiguous once a reconnecting satellite
replays:

> **The cap of 64 counts append *calls*, not records.** One `accept()` of a 256-envelope
> `IngestBatch` is a single append call containing 256 records and a single `fsync`. The cap bounds
> how many concurrent callers coalesce, which at ~4 stations is never reached in steady state and is
> exactly what makes a replay burst cost one disk sync instead of 256.

> **The append call is the atom.** `admit` of a decoded batch, and `accept` of one `IngestBatch`,
> each commit entirely or not at all. This is what lets §4.2's ack be per-batch with no
> within-batch partial ack to express, and it is what W3's `Ingress::admit` cancellation contract
> ("appended the whole batch or none of it") already asserts.

The `MANIFEST` rewrite that carries `hw_seq` updates is part of the same commit: the manifest is
written to `MANIFEST.new`, fsynced, renamed over `MANIFEST`, and the directory fsynced, after the
segment's `fsync` and before the caller is told the append committed. The ordering matters — a
manifest claiming a high water the segment does not contain would drop a real envelope as a
duplicate, whereas the reverse merely costs one deduplicated replay.

### 3.4 Cursors: the ack, and the one place an offset advances

```
cursor file (two slots, 64 bytes each; the valid slot with the higher generation wins)
  slot:
    generation   u64        monotonic; incremented on every write
    acked_seq    u64        every record at or below this seq is durably handled
    acked_at     i64        unix ms, for spool_oldest_unacked_age_seconds
    state        u8         Active | Abandoned
    reserved     [u8; 31]
    crc          u32        CRC32C of the preceding 60 bytes
```

Two fixed slots written alternately is the standard torn-write-safe pattern and it is deliberate
rather than habitual: a cursor is written on every ack — the single most frequent durable write in
the system — so it must be a fixed-size, in-place, `O_SYNC` write to an already-allocated block with
no allocation, no rename and no directory fsync. A crash can leave one slot torn; the CRC finds it
and the other slot is a valid earlier position, which costs a replay and never a loss.

`Spool::open_cursor(id)` returns a `Cursor` owned by exactly one task (W3 Decision 20). Cursor
identity is the component id (`writer:timescale`, `uplink:suncrest`), so a cursor survives restart
and a renamed peer is a new cursor starting at the current tail — which is correct, because a renamed
peer is operationally a new peer.

**Reclamation.** A segment is deleted when every `Active` cursor has `acked_seq >= ` the first `seq`
of the *next* segment. `Abandoned` cursors are ignored for reclamation (§3.5). That is W3's §2.4
reasoning for an 8 MiB segment: the segment is the granularity of reclamation.

### 3.5 Bounds, and what happens at the bound

W3 §2.7 decided both degenerate cases and they stand without amendment:

| Case | Decision (W3 §2.7) |
|---|---|
| the spool cannot be written | **fatal.** `SpoolError` propagates out of `admit`, logged at `ERROR` with the chain, exit non-zero, systemd restarts, the restart loop is visible. A daemon that cannot durably accept readings is wrong, not degraded. |
| the spool reaches its ceiling (default **2 GiB**) | **drop at the edge, loudly.** `AdmitError::SpoolFull`, `admit_rejected_total{reason="spool_full"}`, deduped log once per minute, every station component `Failed`. |
| a cursor is permanently behind (default `max_lag` **7 days**) | the cursor is marked `Abandoned`, its component moves to `Failed { CursorAbandoned }`, and its segments are reclaimed. |

What W7 adds is **the invariant that makes "drop at the edge" the right answer rather than a coin
flip**, because on its own it has an unattractive consequence: W3 §2.6 publishes to `Latest`
subscribers *after* the commit, so at the ceiling a node's own `/latest` goes stale too.

> **`max_lag` must fire before the ceiling does.** At the measured ~2 envelopes/s and ~120 bytes
> encoded, 2 GiB is roughly 100 days of spool and the worst case (~1 KiB fully-populated envelopes)
> is ~12 days — both longer than `max_lag`'s 7 days. So a spool filled by a *stuck cursor* is
> always resolved by abandoning that cursor, and the ceiling is reachable only when the **admit
> rate** is far above what the node was configured for: a misconfigured 100 Hz poll, or a peer
> flooding an ingest endpoint. In that case the node *is* broken, every station is `Failed`, and a
> stale `/latest` is an accurate report rather than collateral damage.

That invariant has to be checked, not assumed, because an operator who sets a 256 MiB spool on an SD
card and leaves `max_lag` at 7 days has inverted it. It cannot be a `plan()` check (W2a Decision 2),
because `plan()` is pure and the admit rate is a runtime fact. So:

- **`spool_time_remaining_seconds`** — a gauge: `(ceiling - depth) / (trailing-hour admit byte rate)`.
  This is the number an operator watches, and it is the number W5 renders as "about 94 days".
- **A startup and config-change warning** at `WARN`, surfaced by W5 as a configuration advisory:
  when `ceiling < max_lag × trailing_byte_rate`, the message names both values and the implication —
  "the spool will fill before a stuck peer is abandoned; raise `[spool].ceiling` or lower
  `[spool].max_lag`".

`drop-oldest` as an alternative ceiling policy is rejected: it silently converts "this node is
misconfigured" into "this node has holes in its history", and D-4's indefinite raw retention only
means something if raw is complete.

### 3.6 Recovery on start

```
open(spool_dir):
  1. read MANIFEST; CRC fail → refuse to start (a lost origin table would mis-dedup, §3.3)
  2. list segments, sort by first_seq
  3. scan the last segment forward, validating each record's CRC and len
       → truncate at the first bad or short record; count spool_truncated_total
  4. tail_seq = seq of the last valid record (or MANIFEST's node seq if the spool is empty)
  5. rebuild the dedup seen-window (§4.4) by scanning back over the newest segment only
  6. for each cursor file: pick the valid slot with the higher generation
       → acked_seq > tail_seq (a truncation ate acked records) ⇒ clamp to tail_seq
       → acked_seq < first_seq of the oldest segment (segments were reclaimed under an
         Abandoned cursor that has since been re-activated) ⇒ clamp forward, count
         spool_cursor_skipped_envelopes, log at WARN naming the gap
  7. ready: no component has started yet (W3 §3.1 — this is a root resource)
```

Only the newest segment is scanned, so recovery is bounded by 8 MiB regardless of spool depth.

---

## 4. Delivery semantics

### 4.1 The wire protocol

**A new service in a new file, and `WeatherV1` is not touched.** This matters more than it looks:
`WeatherV1` currently carries both the collector RPCs *and* the app-facing query RPCs, with
`google.api.http` annotations that map onto `/v1/weather/span/{spanDuration}` and
`/v1/weather/latest` (`protocols/remoteweather/remoteweather.proto:12-30`), and it is registered at
**three** separate sites (`restserver/controller.go:133`, `grpcreceiver/station.go:132`,
`grpcstream/grpc.go:163`). Changing `SendWeatherReadings` in place would therefore reach into a
surface W1 froze. So:

```proto
// generated/proto/collector.proto — W2's generator emits the Envelope's Reading field
syntax = "proto3";
package collector.v1;

service Collector {
  // Identity only. No secrets, ever. §5.2
  rpc Hello  (NodeIdentity)        returns (HelloAck);
  // Bidirectional and acked. §4.2
  rpc Ingest (stream IngestBatch)  returns (stream IngestAck);
  // Bootstrap only, separately gated. §5.3
  rpc Enroll (EnrolRequest)        returns (EnrolGrant);
}

message Envelope {
  bytes            origin      = 1;  // NodeId, 16 bytes
  uint64           seq         = 2;  // per-origin, §1.5
  repeated bytes   path        = 3;  // NodeIds, oldest first, path[0] == origin, len <= 8
  string           station_key = 4;  // resolved at the origin; W2a §4a.6
  int64            observed_at = 5;  // unix ms, at the origin
  int64            received_at = 6;  // unix ms, at the origin
  weather.v1.Reading reading   = 7;  // generated from the metric registry; every field `optional`
}

message IngestBatch {
  repeated Envelope envelopes     = 1;  // <= 256, <= 1 MiB encoded
  uint64            through       = 2;  // the SENDER's spool seq of the last envelope here
  uint64            sender_depth  = 3;  // envelopes still unsent in the sender's spool. §7
}

message IngestAck {
  uint64            through       = 1;  // cumulative: every sender position <= this is durable
  repeated OriginHigh known_through = 2; // advisory, sent on the first ack of a stream. §4.3
  AckStatus         status        = 3;
  uint32            duplicates    = 4;  // in the batch just acked, for the sender's counters
  uint32            rejected      = 5;
}

message OriginHigh { bytes origin = 1; uint64 seq = 2; }

enum AckStatus {
  ACK_OK                 = 0;
  ACK_RESOURCE_EXHAUSTED = 1;  // collector's spool at its ceiling; `through` unchanged; back off
  ACK_REJECTED           = 2;  // the whole batch was refused; see `rejected`; do NOT retry blindly
}
```

Two things the old contract made impossible, both closed by the signature alone:

| | `SendWeatherReadings(stream WeatherReading) returns (Empty)` | `Ingest(stream IngestBatch) returns (stream IngestAck)` |
|---|---|---|
| per-reading acknowledgement | none — one terminal `Empty` at end of stream | cumulative `through` per batch |
| resume offset | none | `through`, plus `known_through` at stream open |
| what a sender can learn about what was persisted | nothing, ever | exactly how far it got |

The second row is why defect 2 could not be fixed by buffering alone: even with a perfect local
buffer, a Go satellite has no way to learn what its collector kept.

> **An ack means "durably appended to the collector's spool", not "written to PostgreSQL".** That is
> forced by W3 §2: the collector's own database write is a *cursor* on the same spool, so making the
> ack wait for PostgreSQL would reintroduce exactly the coupling W3 removed — a stalled database
> back-pressuring every satellite in the fleet. The collector's spool is itself durable and its
> writer cursor cannot lose data, so "spooled" is a complete promise.

### 4.2 Why the ack is in the sender's coordinate space

The prompt's `IngestAck { origin, through_seq }` is correct for a **leaf satellite**, where every
envelope in the spool has `origin == self` and `seq` is the spool position, so the per-origin ack
*is* the cursor position. It breaks for a **relay**, whose spool interleaves envelopes from several
origins:

```
relay-west's spool, in append order:   [A:4711] [B:901] [A:4712] [C:55] [A:4713]
collector acks:                        origin A through 4713, origin B through 901
relay-west's cursor can advance to:    ...where?
```

The cursor is one number — a position in one log. To turn a set of per-origin cumulative acks into a
cursor position you must compute the longest prefix of the log all of whose elements are acked, which
means holding every origin's high water and re-deriving the prefix on every ack. And if the collector
acks origin A but not origin C, the cursor is pinned behind `C:55` anyway, so the per-origin
granularity buys nothing it could use.

> **Decision: `IngestBatch.through` and `IngestAck.through` are the sender's spool `seq` of the last
> envelope in the batch. The ack is cumulative in the sender's log.** For a leaf satellite this is
> numerically identical to the prompt's `through_seq`, because the sender is the sole origin. For a
> relay it is the only form that a cursor can consume.

The per-origin vector survives where it is genuinely useful: `IngestAck.known_through`, sent once at
stream open, is the collector's per-origin high water. The sender uses it to skip a leading run of
its unacked prefix that the collector demonstrably already has — the common case being an ack that
was lost when the link dropped. It is **advisory**: the sender may ignore it entirely and the only
cost is deduplicated resends.

### 4.3 The uplink state machine

```
                   ┌──────────────┐
                   │ Disconnected │◀──────────────────────────────┐
                   └──────┬───────┘                               │
                 connect  │                             stream error / deadline
                          ▼                                       │
                   ┌──────────────┐                               │
                   │  Connecting  │──── TLS or auth failure ──────┤
                   └──────┬───────┘                               │
                mTLS done │                                       │
                          ▼                                       │
                   ┌──────────────┐                               │
                   │    Hello     │──── NOT_FOUND / PERM_DENIED ──┤
                   └──────┬───────┘                               │
          HelloAck{known_through}                                 │
                          ▼                                       │
                   ┌──────────────┐                               │
            ┌─────▶│   Streaming  │───────────────────────────────┤
            │      └──────┬───────┘                               │
            │  IngestAck  │  shutdown signal                      │
            └─────────────┤                                       │
                          ▼                                       │
                   ┌──────────────┐                               │
                   │   Draining   │──── grace expired / drained ──┘
                   └──────────────┘
```

**`Disconnected → Connecting`.** The supervisor drives this, not the component. There is no
`go c.reconnect(ctx)`: the uplink is a `SpooledSink` whose `deliver` returned `Err`, the supervisor
moves it `Failed → Backoff → Starting` (W2a Decisions 28 and 29), and the backoff is exponential with
full jitter, 1 s base, 60 s cap (W3 §3.4). That is the whole of defect 2's unbounded-goroutine half:
the current code spawns a fresh `reconnect` goroutine per failed send
(`grpcstream/client.go:362-367`) *and* on every connection-state transition (`:426`), guarded only by
a `reconnecting bool` read without the mutex that protects it.

**`Hello`.** One unary call per connection, carrying `NodeIdentity` (§5.2). `HelloAck` returns the
collector's `NodeId`, the station keys it accepts, `known_through`, and `server_time` for §4.6's skew
check. A failure here is terminal for the attempt and distinguishable by code: `PERMISSION_DENIED`
(not enrolled or revoked) is logged differently from `UNAVAILABLE` (collector starting), because the
first needs an operator and the second needs patience.

**`Streaming`.** Batches are read from the spool cursor, 256 envelopes at a time — W3 §2.4's
read-ahead, which lines up exactly with one `IngestBatch`. At most **8 batches / 2048 envelopes** are
in flight unacked, which bounds the memory a slow collector can cost a satellite and is the
backpressure the current unbounded `stream.Send` lacks. On `IngestAck { through }` the cursor is
advanced durably (§3.4) — the only place an offset moves. `ACK_RESOURCE_EXHAUSTED` leaves the cursor
where it is and backs the component off without treating it as an error on this side; the collector's
spool being full is the collector's `Failed`, not the satellite's.

**`Draining`.** On shutdown the uplink stops reading new batches, waits for outstanding acks up to
its grace deadline (W3 §3.6 gives it 15 s), then aborts. Unacked batches are still in the spool and
replay on next start. Nothing is lost by shutting down mid-flight, which is the property the current
implementation cannot claim.

**Resume, end to end.** `acked_seq` from the cursor file → `Hello` → skip the leading run covered by
`known_through` → stream from `acked_seq + 1`. The satellite does not need to remember anything about
the stream, which is why a crash and a clean restart are the same path.

### 4.4 Idempotency: a dedup gate in front of the spool

The collector's `Ingress` gains a second entry point. `admit` seals a locally measured reading;
`accept` takes envelopes that are **already sealed** and must not be re-sealed:

```rust
// mw-component — extends the trait W3 §2.3 defines
#[async_trait]
pub trait Ingress: Send + Sync {
    /// Locally measured readings. Mints origin = self and seq = next spool position.
    async fn admit(&self, batch: &[PartialReading], meta: &FrameMeta)
        -> Result<Admitted, AdmitError>;

    /// Envelopes from a peer. Provenance is PRESERVED — origin, seq and path are never
    /// reassigned. One IngestBatch is one append call and one atom (§3.3).
    async fn accept(&self, batch: &IngestBatch, peer: &PeerId)
        -> Result<Accepted, AcceptError>;
}

pub struct Accepted {
    pub accepted: u32, pub duplicates: u32, pub rejected: u32,
    pub through: Seq,            // the sender position being acked
}
```

Having only `admit` would be the defect, not a simplification: W3 §2.6 step 3 is
`seq = seq_table.next(origin, station)`, and running that on a forwarded envelope would **reassign
the origin's sequence number**, destroying the one identity the ack and the dedup both depend on.
Recorded as assumption 5.

```
accept(batch, peer):
  0. peer is already resolved (§5.4); the stream would not exist otherwise
  1. batch.envelopes.len() <= 256 and encoded size <= 1 MiB     else Reject::BatchTooLarge (stream closed)
  2. per envelope, the path checks of §2.1 steps 1–6            else Reject::{PathTooLong,
                                                                   MalformedPath, NotAddressedHere,
                                                                   Loop, PathForged}
  3. per envelope, origin ∈ peer.origins (§5.4)                 else Reject::UnauthorizedOrigin
  4. per envelope, observed_at within the skew window (§4.6)    else Reject::ClockSkew
  5. per envelope, station_key resolves in StationRegistry      else Reject::UnknownStation
  6. intern origin NodeId -> OriginId, bounded by MAX_ORIGINS   else Reject::OriginTableFull
  7. DEDUP: seq <= hw[origin], or seq ∈ seen_window[origin]     -> duplicate; counted; NOT spooled
  8. enrich(&mut reading) — idempotent, fills only `None` (W3 §2.5 consequence 3)
  9. spool.append(surviving envelopes)  — one call, one fsync, hw[origin] updated in the same commit
 10. notify Latest / Lossy subscribers (W3 §2.6 step 6 — after the commit, never before)
 11. return Accepted { through: batch.through, .. }
```

Steps 2–7 run **before** step 9, which is the structural form of defect 4's fix: an envelope that
fails any of them never reaches the spool, and therefore never reaches the reading stream, the
downstream peers, the snapshot or the database. Contrast the current order
(`grpcreceiver/station.go:174-194`), where the reading is pushed into the distributor at `:175` and
the registry is consulted at `:182`, with an unknown sender getting a `Debugf`.

**The dedup state.**

| | Form | Durability |
|---|---|---|
| `hw[origin]` — highest `seq` accepted per origin | `u64` per origin in `MANIFEST` | fsynced in the same group commit as the records (§3.3) |
| `seen_window[origin]` — seqs accepted *above* the high water | 4096-bit ring per origin, in memory only | rebuilt on start by scanning the newest segment (§3.6 step 5) |

The high water alone handles the ordinary case. The window handles arrival out of order — the diamond
of §2.3 where `A:4712` arrives via B before `A:4711` arrives via C — without which `hw` would have to
advance only on a contiguous run and a single lost envelope would pin it forever. The window is
in-memory because losing it costs at most 4096 deduplicated replays per origin, which the database
backstop absorbs, and persisting it would add a second fsynced structure for no correctness gain.

**The database backstop.** W4's raw hypertable keeps `UNIQUE (station_id, time)` — the natural key the
aggregate hierarchy already requires — and W3's writer uses
`ON CONFLICT (station_id, time) DO UPDATE`. Replayed envelopes carry identical values, so the update
is a no-op write. **W7 adds no columns to the raw table**: `(origin, seq)` on 19 GB/year of rows, for
a property whose enforcement point is the spool gate, is cost with no consumer. `origin` *is* worth a
column for provenance — W4's call, recorded as assumption 8 — but not as a uniqueness key.

> **At-least-once delivery (the ack/resume state machine) plus an idempotent admission gate (the high
> water and the window) plus an idempotent write (the natural key) is effectively-once.** Three
> layers, each cheap, and the one that fires in practice is the first.

### 4.5 REST parity

W3 treats transport as a codec choice (D-31, §6.5), so REST ingest must carry the **same contract**
or the fleet acquires two reliability tiers and the model is unclear again.

```
POST /v1/ingest
  Content-Type: application/json  (protobuf JSON mapping, W3 §6.5) | application/x-protobuf
  Authorization: Bearer <psk>     (only when peer_auth = "psk"; mTLS otherwise)
  body:  IngestBatch              identical message, identical limits

200 OK   IngestAck { through, known_through, status: ACK_OK, duplicates, rejected }
200 OK   IngestAck { through: <unchanged>, status: ACK_RESOURCE_EXHAUSTED }   + Retry-After
401/403  admission refused (§5.4) — no body is read
413      batch over 256 envelopes or 1 MiB
422      IngestAck { status: ACK_REJECTED, rejected } — every envelope failed a step-2..7 check
503      the collector is not ready to accept                                  + Retry-After
```

Everything that makes the gRPC path reliable is present and is the *same code*:

- **Same spool, same `accept()`, same dedup gate.** The handler is a thin codec over `Ingress::accept`.
- **Same atom.** One request is one `IngestBatch` is one append call is one `fsync`.
- **Same idempotency**, so a retried POST after a lost response is safe by construction and re-acks
  rather than duplicating. This is what makes the absence of a stream harmless.
- **Same cursor semantics.** The sender advances its cursor to the response's `through`, which may be
  lower than the request's. There is no second code path to keep in step.
- **`known_through` on every response** rather than once per stream, because there is no stream to
  open; the sender uses the most recent one.

What REST does *not* get is server-initiated flow control, so a REST sender paces itself: at most one
request in flight, and `Retry-After` honoured. The cost is throughput, not reliability, and at the
measured rate it is not a cost at all. `/v1/ingest` is additive to the frozen surface (D-37) and
collides with nothing in `docs/legacy-api-contract.md`; W1's route table has `/v1/weather/*` only.

### 4.6 Envelope validation at the boundary

Admission control decides *who* may speak (§5.4). These checks decide whether *what they said* is
storable, and they exist because nothing validates it today.

| Check | Default | Rejection | Why |
|---|---|---|---|
| `observed_at` not more than `max_future_skew` ahead of the collector's clock | **5 min** | `Reject::ClockSkew` | A satellite with a wrong clock writes rows into the future, which breaks `/latest` (newest-first) and every window computation on the collector. Today nothing checks this. |
| `observed_at` not more than `max_past_skew` behind | **30 d** | `Reject::ClockSkew` | Chosen above both the 7-day `max_lag` and the ~12-day worst-case ceiling, so a legitimate replay after a long outage is never rejected. |
| `station_key` resolves in `StationRegistry` | — | `Reject::UnknownStation` | An envelope naming a station the collector has never heard of would create history under a key nothing else references. |
| `reading` has at least one populated metric | — | `Reject::EmptyReading` | An all-`None` reading becomes an all-`NULL` row, which is a row the aggregates must still process. |

`HelloAck.server_time` lets the satellite compute and export `clock_skew_seconds` against its
collector, so a drifting clock is a number before it is a rejection.

---

## 5. Identity, registration and admission

### 5.1 The peer registry is the station registry's registration path

W2a Decision 45 already settled the shape: `RemoteStationRegistry` is not a thing, it is
`StationRegistry`'s `FromRegistration` path, and `StationRegistry` is the same capability APRS uses
for coordinates and callsign and the same one Ambient's `?ID=` resolves against. One registry, five
ways to arrive in it.

A peer is a **node**, and a node has stations, so the registration path writes two kinds of row:

```
peers        node_id (PK, 16 bytes) · name · auth_kind · auth_material · relay · enrolled_at
                                   · last_seen · revoked_at
peer_origins node_id · origin_node_id            which origins this peer may deliver
stations     station_key (PK) · owning_node_id · name · lat · lon · altitude · timezone
                               · aprs_callsign · …   (the existing station metadata)
```

`PeerRegistry` is not a new capability. It is the peer-facing accessor set on
`StationRegistryHandle`, which is why §0.1's component table lists `StationRegistry` H **W** for
`ingest:*` — `Write` for `ingest:*` alone, exactly as W2a §4.1b has it, because registration mints
keys and nothing else may.

### 5.2 Registration carries identity only

```proto
message NodeIdentity {
  bytes  node_id        = 1;   // 16 bytes, allocated once by the node itself
  string node_name      = 2;   // operator-facing, mutable, never an identity
  string software        = 3;  // "modernweather 1.4.2" — for W5 and for compat decisions
  repeated StationMeta stations = 4;
}

message StationMeta {
  string station_key   = 1;
  string name          = 2;
  string station_type  = 3;
  double latitude      = 4;
  double longitude     = 5;
  double altitude_m    = 6;
  string timezone      = 7;   // IANA name; W2a Decision 11
  string aprs_callsign = 8;   // identity, not a credential — see §6.2
}

message HelloAck {
  bytes  collector_node_id      = 1;
  repeated string accepted_stations = 2;
  repeated OriginHigh known_through = 3;
  int64  server_time            = 4;
  uint32 max_batch_envelopes    = 5;
}
```

Set against what is on the wire today, the deletion is the whole point. `RemoteStationConfig`
(`remoteweather.proto:353-377`) has 15 fields, of which **12 are third-party service configuration
and 4 are plaintext secrets**: `aprs_password`, `wu_api_key`, `aeris_client_secret`, `pws_password`.
`buildRegistrationConfig` (`grpcstream/client.go:275-336`) reads them straight out of the satellite's
device row and puts them on the wire at every registration and re-registration; `Register`
(`grpcreceiver/registry.go:147-233`) unpacks them into `RemoteStationData` and
`RegisterRemoteStation` (`pkg/config/remote_stations.go:74-104`) `INSERT OR REPLACE`s them into the
collector's `remote_stations` table in cleartext.

The *intent* is sound — the collector is the fleet's egress point for third-party services. But "here
is who I am" and "here are my secrets, upload on my behalf" are two operations welded into one RPC,
so the first cannot happen without the second.

**And nothing consumes the secrets.** The only reader of `remote_stations` outside the registry that
writes it is `GET /api/remote-stations` (`restserver/handlers.go:1182-1260`), and it projects the four
`*_enabled` booleans, the name, the type and `last_seen` — never a credential. No uploader reads the
table at all. So today the credential handoff is pure liability: four secrets per satellite crossing
the wire in plaintext and coming to rest in a second database, for a capability that was never wired
up. §6 is what replaces it.

### 5.3 Enrolment: how a peer comes to be trusted at all

`Hello` authenticates an **already-enrolled** peer. Enrolment is the one-time bootstrap and it is
deliberately a separate RPC with a separate gate, because it is the only operation that can add a
peer:

```
operator, on the collector:   mw-admin peer enrol-token --name satellite-a --ttl 1h
                              → one single-use token, printed once, stored hashed (Argon2id, D-35's
                                primitive) with its expiry
operator, on the satellite:   mw-admin uplink join --peer suncrest:50051 --token <token>
satellite:                    generates a keypair, sends Enroll { token, csr, identity }
collector:                    verifies the token (constant-time, `subtle`, D-33), burns it,
                              signs the CSR with the collector's peer CA (or records the SPKI
                              fingerprint when auth_kind = "mtls-pinned"),
                              inserts `peers` + `peer_origins` + `stations` rows,
                              returns EnrolGrant { certificate, ca_chain, collector_node_id }
```

- **`Enroll` is served only while at least one unexpired token exists.** Otherwise it returns
  `FAILED_PRECONDITION` with "enrolment is closed" — distinguishable from `UNIMPLEMENTED` (wrong
  address) and from `PERMISSION_DENIED` (bad token), because an operator debugging a join needs to
  tell those three apart.
- **Rate-limited** to 1 attempt per 5 s per source address, and every attempt is logged at `INFO`
  with the source and the outcome, successful or not.
- **The token is a `SecretString`** (D-33) and is never logged, unlike the current management token,
  which is printed in cleartext at startup and on every failed authentication.
- **W5 surfaces** outstanding tokens, their expiry, and the enrolment log. An operator should be able
  to see that enrolment is open, because leaving it open is the mistake.

A node may also be enrolled entirely out of band — the operator copies a certificate and inserts the
`peers` row through the management UI. `Enroll` exists so that the common case is not "copy a
certificate by hand", not to be the only path.

### 5.4 Admission happens before the pipeline, in three layers

> **The gate is W2a Decision 46's single authentication point, reached through `FrameMeta`.** An
> unknown peer is **rejected**, not stored with a debug log.

**Layer 1 — the TLS handshake.** `ingest:grpc` is mounted on the configured `grpc_port` (W3 §6.1 —
the port keeps its own socket, and W3 Decision 92 means a satellite's endpoint does not change at
upgrade). `rustls` is configured with a client-certificate verifier alongside the SNI resolver (W3
assumption 47):

| `auth_kind` | Material | Verification |
|---|---|---|
| `mtls-ca` | the collector's peer CA | chain validation; `NodeId` read from the certificate's SAN URI `urn:modernweather:node:<uuid>` |
| `mtls-pinned` | the peer's SPKI SHA-256, in `peers.auth_material` | the presented certificate's SPKI must match, constant-time. No CA, for an operator who does not want to run one |
| `psk` | an Argon2id hash of a pre-shared secret | **TLS still required** (server-authenticated at minimum); the secret arrives in gRPC metadata / `Authorization: Bearer` and is compared constant-time (`subtle`, D-33) |

An unauthorised peer is refused **at the TLS layer, before any frame is decoded** — W2a §4a.6's
policy for `GrpcIn`, applied here. Today there is no client authentication at all: the receiver
configures server TLS only (`grpcreceiver/station.go:118-128`), so anyone who can reach the port is
a peer.

**Plaintext ingest is a plan-time error.** W3 §6.6 allows plaintext h2c on the main port for a LAN
peer, which is fine for a query but not for ingest: a PSK on a plaintext socket is a credential in
the clear, and an unauthenticated ingest endpoint is the current defect restated. `plan()` rejects an
`[[ingest]]` whose socket has no TLS unless `allow_insecure_ingest = true` is set explicitly, in
which case it is logged at `WARN` on every start and surfaced by W5 as a standing advisory.

**Layer 2 — per-RPC.** A `tonic` interceptor resolves the connection's `NodeId` to a `PeerId` against
`peers` (checking `revoked_at`) before the service method body runs. An unresolved or revoked peer
gets `PERMISSION_DENIED` and **no stream is created**, so no `IngestBatch` is ever read. The
resolved `PeerId` reaches `accept` as part of `FrameMeta`.

**Layer 3 — per envelope.** Authentication says *who is speaking*; this says *what they may say*.

```
peers.relay = false (the default):
    every envelope's origin must be the peer itself, and path must be exactly [peer, self]
peers.relay = true:
    origin may be any node_id listed in peer_origins for this peer
    (the §2.1 path checks already prove the peer is on the path and that we are not)
```

So "this satellite may only speak for itself" is the default and relaying is an explicit grant,
recorded per peer. A compromised leaf cannot inject history for another station, which is a property
the current design cannot express because it has no notion of who is allowed to say what.

**Every rejection is a counted number with a reason**, per W2a §4a.6: `ingest_rejected_total{peer,
reason}` over `Reject::{UnknownPeer, Revoked, BadSecret, UnauthorizedOrigin, Loop, PathForged,
PathTooLong, MalformedPath, NotAddressedHere, UnknownStation, ClockSkew, EmptyReading, BatchTooLarge,
OriginTableFull}`. A non-zero counter on an ingest endpoint is the signal that someone is pointing a
node at you, and it is the signal that does not exist today.

---

## 6. Credentials stay where they are configured

### 6.1 The collector selects origins; it does not receive secrets

The collector remains the fleet's egress point for APRS, CWOP, Weather Underground and PWS Weather.
What changes is the direction the authority flows:

| | Today | Here |
|---|---|---|
| How the collector learns it should upload for a satellite | the satellite tells it, at registration | the operator configures it, on the collector |
| Whose credentials are used | the satellite's, transmitted and stored | the collector's own |
| What crosses the wire | `aprs_password`, `wu_api_key`, `aeris_client_secret`, `pws_password` | nothing |
| What the collector's database holds | 4 plaintext secrets per satellite in `remote_stations` | the collector's own credentials, in its own config rows |

An uploader component (W2a §4.2) gains one field:

```toml
# on the collector
[[uploader]]
kind       = "wunderground"
for        = ["satellite-a:backyard"]   # a station selector, validated at plan time
station_id = "KCOXXXX"                  # the collector's own account for that station
api_key    = "…"                        # the collector's own credential, a SecretString (D-33)
```

**`for` is a station selector and its arity is the uploader's, not the selector's.** `wu:*`, `pws:*`
and `aprs:*` are one component per remote account (W2a Decision 55) and therefore take exactly
*one* station; more than one is a plan error naming the kind. The list form exists for kinds whose
remote identity is the fleet rather than the station — a future MQTT publisher, a future fleet-level
exporter — and writing the field as a list from the start avoids a migration when one arrives.

Three properties follow, and they are what makes defect 3 unrepresentable rather than fixed:

1. **A satellite has no uploader components at all**, so there is no code path on a satellite that
   reads a third-party credential in order to send it anywhere. The `RemoteStationConfig` fields have
   no successor to migrate into.
2. **`for` is validated in `plan()`** against `stations` (W2a Decision 2): an uploader naming a
   station that does not exist, or naming a station whose origin the collector has never been
   registered for, is a configuration error before anything starts.
3. **The uploader requires `WindowedView`, not `ReadingStore`** (W2a Decision 50), and the windowed
   view resolves over the selected station regardless of which node measured it. "Upload on behalf
   of a remote station" needs no collector-side special case: a forwarded envelope is an envelope.

### 6.2 The two cases that look like exceptions and are not

**APRS.** The satellite's `aprs_callsign` *is* carried in `StationMeta` — a callsign is public
identity, printed on every packet, not a secret. The APRS-IS passcode is not carried: it is
configured on the collector alongside the callsign it is authorised to beacon for, which makes
beaconing on someone's behalf an explicit operator action rather than an implication of registration.
That is the correct trust shape for a licensed callsign.

**Aeris.** `aeris_client_secret` is in the current registration payload, but Aeris is a *fetch*, not
an upload — the collector pulls forecasts and alerts for a location. It needs a location, which
`StationMeta` carries, and its own API account, which it already has. There was never a reason for
this one to be on the wire.

### 6.3 What W9 has to do

Recorded here because it is a consequence of this design, not a migration design:

1. Move each `remote_stations` row's enabled services into collector-side `[[uploader]]` rows with
   `for = [<that station>]`, carrying the credential across inside the collector's own database.
2. **Drop the four secret columns** from `remote_stations` (and retire the table in favour of
   `peers` / `peer_origins` / `stations`, §5.1).
3. **Recommend rotating all four credentials.** They crossed the wire in plaintext and sat in a
   second database; migrating them preserves service continuity, and rotating them is what actually
   closes the exposure.

---

## 7. Observability: "how far behind is each satellite" is a number

Today it is invisible. There is no counter, no gauge and no log line from which an operator can
derive how much a satellite is holding, because the satellite holds nothing — a failed send is a
dropped reading and an `Errorf` (`grpcstream/client.go:362-367`).

**On every node (the spool is not collector-specific).**

| Metric | Type | Labels | What it answers |
|---|---|---|---|
| `spool_depth_bytes` | gauge | — | how much is on disk |
| `spool_depth_envelopes` | gauge | — | …and how many |
| `spool_lag_envelopes` | gauge | `cursor` | how far behind each consumer is |
| `spool_oldest_unacked_age_seconds` | gauge | `cursor` | how long it has been behind — the one W5 renders as a duration |
| `spool_time_remaining_seconds` | gauge | — | §3.5 — when the ceiling arrives at the trailing rate |
| `spool_appends_total`, `spool_fsyncs_total` | counter | — | group-commit coalescing, which is the one thing that makes a replay burst cheap |
| `spool_truncated_total` | counter | — | a torn tail was discarded (§3.2) |
| `spool_cursor_abandoned_total` | counter | `cursor` | `max_lag` fired |
| `admit_rejected_total` | counter | `reason` | includes `spool_full` (W3 §2.7) |

**On a satellite or relay, per uplink.**

| Metric | Type | Labels | What it answers |
|---|---|---|---|
| `uplink_state` | gauge (enum) | `peer` | §4.3's state machine, as a number W5 maps to a sentence |
| `uplink_ack_lag_seconds` | gauge | `peer` | **the headline number**: now − `acked_at` on that cursor |
| `uplink_unacked_envelopes` | gauge | `peer` | the in-flight window plus the unsent remainder |
| `uplink_batches_sent_total`, `uplink_envelopes_sent_total` | counter | `peer` | |
| `uplink_filtered_total` | counter | `peer` | loop filter hits — a mutual uplink is visible, not silent |
| `uplink_loop_refused_total` | counter | `peer` | `forward` refused: the filter missed and the invariant caught it. **Non-zero is a bug in this code**, not a configuration problem |
| `uplink_reconnects_total` | counter | `peer` | |
| `uplink_duplicates_acked_total` | counter | `peer` | the collector already had these — resend volume |
| `clock_skew_seconds` | gauge | `peer` | from `HelloAck.server_time` (§4.6) |

**On a collector, per peer.**

| Metric | Type | Labels | What it answers |
|---|---|---|---|
| `ingest_envelopes_total` | counter | `peer`, `origin` | |
| `ingest_duplicates_total` | counter | `origin` | the dedup gate firing — expected and non-zero in a diamond |
| `ingest_rejected_total` | counter | `peer`, `reason` | §5.4's full `Reject` set |
| `peer_last_seen_seconds` | gauge | `peer` | silence is the failure mode a stream-based protocol hides |
| `peer_reported_depth_envelopes` | gauge | `peer` | from `IngestBatch.sender_depth` |
| `peer_software_info` | info | `peer`, `version` | which peers are still Go-era (§9) |

`IngestBatch.sender_depth` is one varint per batch and it is the reason the collector can render the
whole fleet's lag without scraping every satellite — which matters precisely when a satellite's link
is down and scraping it is impossible. It is **peer-reported** and must be labelled as such wherever
W5 shows it; the collector's own authoritative view is `peer_last_seen_seconds`.

**The one derived view W5 should build from these**, because it is the question an operator actually
has: a per-peer row of *state*, *ack lag as a duration*, *unacked envelopes*, *peer-reported depth*,
and *last seen* — five numbers that together distinguish "the link is slow", "the link is down",
"the collector is refusing it" and "it has nothing to send".

---

## 8. Configuration shape

The persisted form is SQLite (W3 §3.1); the TOML below is this document's rendering of configuration
rows, and is also the shape of the `ci/examples/*.db` fixtures `plan()` is checked against (W3 §8.2).

**The symmetry claim, stated so it can be checked: there is no `role`, no `mode` and no
`is_collector` anywhere below.** A node's position in the topology is the set of components its
configuration produces, which is why "both" needs no syntax.

### 8.1 Every node has these

```toml
[node]
id   = "0193f3c2-7a10-7b64-9c2e-4f1b8a6d0e11"   # allocated once at first start; never edited
name = "suncrest"                                # operator-facing; renaming it is not an identity change

[spool]
path    = "/var/lib/modernweather/spool"
ceiling = "2GiB"     # W3 §2.4
max_lag = "7d"       # W3 §2.7 — must be < ceiling at the observed rate; §3.5 checks and warns
```

### 8.2 A satellite — stations plus an uplink

```toml
[node]
id   = "0193f3c2-7a10-7b64-9c2e-4f1b8a6d0e11"
name = "satellite-a"

[[station]]
key      = "satellite-a:backyard"
type     = "davis"
transport = { kind = "serial", dev = "/dev/ttyUSB0", baud = 19200 }
latitude = 39.6403
longitude = -105.2094
timezone = "America/Denver"

[[uplink]]
peer     = "suncrest"
endpoint = "https://suncrest.example.net:50051"
auth     = { kind = "mtls", cert = "/etc/modernweather/satellite-a.pem",
             key = "/etc/modernweather/satellite-a-key.pem",
             ca  = "/etc/modernweather/peers-ca.pem" }

# No [[ingest]], no [[uploader]], no [database]. A satellite with no local PostgreSQL has no
# writer:timescale cursor on its spool, so the uplink cursor is the only consumer — which is
# what makes `[spool].ceiling` the satellite's entire outage budget.
```

### 8.3 A collector — stations, ingest, uploaders, a database

```toml
[node]
id   = "0193f3c2-8b20-7c71-ae3f-10cc92d4b7aa"
name = "suncrest"

[database]
url = "postgresql:///modernweather"

[[station]]
key       = "suncrest:roof"
type      = "weatherlink-live"
transport = { kind = "udp_in", port = 22222 }

# ── ingest: the only thing that makes this node a collector ──
[[ingest]]
listen    = "grpc"                                  # mounted on [http].grpc_port (W3 §6.1)
peer_auth = "mtls-ca"
ca        = "/etc/modernweather/peers-ca.pem"

[[ingest]]
listen    = "rest"                                  # POST /v1/ingest on the main router (§4.5)
peer_auth = "mtls-ca"

[[peer]]
node    = "satellite-a"
id      = "0193f3c2-7a10-7b64-9c2e-4f1b8a6d0e11"
relay   = false                                     # default: may speak only for itself
origins = ["satellite-a"]                           # default: [itself]

# ── egress to third parties, on the collector's own credentials (§6) ──
[[uploader]]
kind       = "wunderground"
for        = ["satellite-a:backyard"]
station_id = "KCOXXXX"
api_key    = "…"

[[uploader]]
kind     = "aprs"
for      = ["satellite-a:backyard"]
callsign = "W0ABC-13"                               # from StationMeta; the passcode is ours
passcode = "…"
```

### 8.4 A node that is both — the default case, not a mode

`relay-west` has its own station, accepts two satellites, relays everything to `suncrest`, and keeps
no database of its own.

```toml
[node]
id   = "0193f3c2-9c30-7d8e-b4a1-77de3b02f5c3"
name = "relay-west"

[[station]]
key       = "relay-west:ridge"
type      = "campbell"
transport = { kind = "serial", dev = "/dev/ttyS0", baud = 9600 }

[[ingest]]
listen    = "grpc"
peer_auth = "mtls-pinned"

[[peer]]
node = "satellite-b"
id   = "0193f3c2-a140-7e9f-8512-c4ab6e1d9077"
spki = "sha256:9f8c…"                               # mtls-pinned: no CA to run

[[peer]]
node = "satellite-c"
id   = "0193f3c2-b250-7fa0-96b3-e2fd4c8a3311"
spki = "sha256:41ba…"

[[uplink]]
peer     = "suncrest"
endpoint = "https://suncrest.example.net:50051"
auth     = { kind = "mtls", cert = "…", key = "…", ca = "…" }
```

…and on `suncrest`, the one row that makes the relay legal:

```toml
[[peer]]
node    = "relay-west"
id      = "0193f3c2-9c30-7d8e-b4a1-77de3b02f5c3"
relay   = true                                      # the explicit grant (§5.4 layer 3)
origins = ["relay-west", "satellite-b", "satellite-c"]
```

Reading `relay-west`'s configuration: one station, one ingest endpoint, two peers, one uplink. Its
components are `station:campbell:ridge`, `ingest:grpc`, `uplink:suncrest`, and no
`writer:timescale`. Its spool has exactly one cursor. That is its position in the topology, and
nothing had to declare it.

### 8.5 The plan-time checks this shape makes possible

All pure, all unit-testable, all CI-checked over `ci/examples/*.db` (W2a Decision 2):

| Check | Error |
|---|---|
| an `[[ingest]]` on a socket with no TLS and no `allow_insecure_ingest` | `PlanError::InsecureIngest` |
| an `[[uplink]]` whose `peer` endpoint is this node's own `node.id` | `PlanError::SelfUplink` |
| two `[[uplink]]`s to the same peer | `PlanError::DuplicateUplink` |
| a `[[peer]]` with `relay = false` and more than one entry in `origins` | `PlanError::RelayNotGranted` |
| a `[[peer]]` whose `origins` omits the peer itself | `PlanError::PeerCannotSpeakForItself` |
| an `[[uploader]]` whose `for` names an unknown station | `PlanError::UnknownStation` |
| an `[[uploader]]` of kind `wunderground`/`pws`/`aprs` with more than one station in `for` | `PlanError::UploaderArity` |
| `[[ingest]]` present with no `[[peer]]` rows | not an error — a `WARN`; a collector awaiting enrolment is a legitimate state |

`PlanError::SelfUplink` deserves its place: a node uplinking to itself is the degenerate loop, and
making it a configuration error means the loop invariant never has to be the thing that catches it.

---

## 9. The existing fleet

### 9.1 What has to keep working during the transition

The Go version stays deployable throughout (a standing constraint), which means a Rust collector will
have Go satellites pointed at it, and a Rust satellite may be pointed at a Go collector.

| Direction | Support | Mechanism |
|---|---|---|
| Go satellite → Rust collector | **yes**, for the transition | a compat shim: `WeatherV1.SendWeatherReadings` is implemented on the Rust collector (§9.2) |
| Rust satellite → Go collector | **yes**, for the transition | the uplink falls back to `SendWeatherReadings` when `Hello` returns `UNIMPLEMENTED`, and degrades to at-most-once with the consequence stated (§9.3) |
| Rust ↔ Rust | the design above | `Collector.Ingest` |

### 9.2 The compat shim, and exactly what it cannot do

A Go satellite sends `stream WeatherReading` with no origin, no seq and no path. The shim
synthesises the provenance it cannot receive:

```
SendWeatherReadings(stream), peer P resolved by §5.4 layers 1 and 2:
  origin := P.node_id                 the peer is asserted to be the origin; relay is impossible
  seq    := a collector-side counter per P, persisted in `peers.compat_seq`
  path   := [P, self]
  then: the §4.4 checks 4..9 as normal
```

Three honest limitations, stated because a shim that pretends to be the real contract is worse than
no shim:

1. **No resume and no dedup across a reconnect.** `seq` is minted by the collector, so a Go
   satellite that reconnects and resends re-mints new sequence numbers and the dedup gate cannot see
   the duplicate. The database's `ON CONFLICT (station_id, time)` backstop absorbs it; this is
   precisely the at-most-once-plus-natural-key behaviour of today, carried forward unchanged for
   peers that cannot do better.
2. **No relay.** `relay = true` on a compat peer is a plan error, because an unsigned, absent path
   cannot be verified and a forged `origin` cannot be caught.
3. **Field presence is lost in the Go direction.** The committed proto's bare scalars make absent and
   `0.0` the same bytes (W3 §6.5), so a Go satellite's unreported metric arrives as `0.0` and the
   collector stores a zero. Nothing on the receiving side can recover it. This is the existing
   silent corruption, and it is one more reason the shim is a transition and not a tier.

**The shim is removed when `peer_software_info` shows no Go-era peers** — which is exactly what that
metric exists for — and its removal is a W9 deliverable, not a someday.

### 9.3 A Rust satellite against a Go collector

`Hello` returns `UNIMPLEMENTED`; the uplink falls back to `SendWeatherReadings`. The cursor then
advances **on send**, not on ack, because there is no ack to wait for — so the satellite is
at-most-once against a Go collector, exactly as today. The fallback is logged at `WARN` on every
connection and exported as `uplink_degraded{peer, reason="no_ack_support"}` so it is a visible state
rather than a silent downgrade, and `[[uplink]].require_ack = true` (default `false` during the
transition, `true` after) turns it into a hard failure instead.

---

## 10. What this deletes

| Deleted | Lines | Why it existed |
|---|---|---|
| `internal/storage/grpcstream/` — egress as a storage engine | 1,095 | being a source for a peer had no abstraction but `StorageEngineInterface` |
| `internal/weatherstations/grpcreceiver/` — ingress as a weather station | 753 | being a sink for a peer had no abstraction but `WeatherStation` |
| `getGRPCReceiverDevices()` + a `LoadConfig()` per reading | 15 + a config query per reading | loop prevention was a naming convention, not an invariant |
| the name-blocklist loop guard in `sendReading` | 8 | nowhere to record a hop |
| `go c.reconnect(ctx)` per failed send and per state transition, with its unsynchronised `reconnecting bool` | — | the component owned its own lifecycle; W2a Decision 28 gives it to the supervisor |
| 12 third-party-config fields on `RemoteStationConfig`, 4 of them plaintext secrets | proto `:353-377` | registration doubled as a credential handoff |
| 4 plaintext secret columns in `remote_stations`, written by `INSERT OR REPLACE` | `remote_stations.go:74-104` | …and nothing ever read them — `GET /api/remote-stations` projects only the `*_enabled` flags |
| `RemoteStationRegistry` as a device-config bolt-on | 284 | `FromRegistration` not recognised as one strategy of five (W2a Decision 45) |
| the store-then-check order in `SendWeatherReadings` | `station.go:174-194` | admission was not on the data path |
| `Debugf("Received reading from unregistered station ID: %s")` as the entire response to an unknown sender | 1 | there was no rejection to count |
| two of three `RegisterWeatherV1Server` sites | — | one service, three servers, no owner |
| the unbuffered `readingChan` in `grpcstream`'s server and the 100-deep one in its client | `grpc.go:44`, `client.go:62` | an in-memory queue standing in for durability |

Roughly **1,850 lines** of two mutually-unaware halves become two components, one wire protocol and
one spool format — and the spool is not new work, because W2a Decision 62 already requires it for
`writer:timescale`.

---

# Decisions

1. **A collector is a node whose components include an ingest endpoint; a satellite is one whose components include an uplink.** Same binary, same code path, different configuration rows. There is no `role`, `mode` or `is_collector` field anywhere in the configuration, which is what makes "both" the default case rather than a special mode.
2. **`station:grpcrx` is renamed `ingest:grpc`**, because the `station:` prefix is the weather-station framing this work item exists to remove: a peer is not a station, it is a node that has stations. `uplink:collector:<peer>` → `uplink:<peer>` is cosmetic and W6 may keep either spelling.
3. **The uplink's filter is `NotFrom(peer)`, not `NotFrom(self)`.** Every envelope in a node's spool has `self` on its path — `admit` sets `path = [self]` and a peer's `forward(me)` inserts `me` before sending — so `NotFrom(self)` matches nothing and the uplink would send nothing. W2a §4.1 and W3 assumption 44 both need this correction.
4. **`seq` is monotonic per origin node alone, not per `(origin, station)`**, and is the origin's spool append position, so `(origin, seq)` is an envelope's globally unique immutable identity. A cumulative ack needs one sequence per log; per-station sequences turn one resume offset into a vector keyed by every station an origin ever had.
5. **`NodeId` is a 128-bit UUIDv7 allocated once by the node itself; `OriginId` is a dense per-node `u16` index into the spool manifest's origin table.** The wire carries `NodeId`s, the spool and memory carry `OriginId`s, and the translation happens exactly twice — at encode and at decode.
6. **`path` is ordered, oldest first, with `path[0] == origin` and `path.last() ==` the hop it is addressed to.** Ordering is what makes the receive-side check exact: a bare set cannot distinguish "you were added by my sender's `forward`" from "you were here two hops ago". It also makes the path a route record W5 can render.
7. **The receive check is six clauses** — length ≤ 8, `path[0] == origin`, addressed to me, I am not in `path[..len-1]`, `path[len-2]` is the sending peer, all elements distinct — and it is the second of two independent loop guards, the first being `Envelope::forward`'s refusal, which is a compile-time property of a type with no public constructor.
8. **`MAX_PATH = 8` and `MAX_ORIGINS = 1024`**, both counted rejections rather than silent drops, because both are growth driven by an untrusted peer.
9. **A filtered or loop-refused envelope advances the cursor without being sent.** A cursor that stalls on an envelope it will never send stalls forever, stops reclamation, and is eventually abandoned by `max_lag`.
10. **The spool lives at `/var/lib/modernweather/spool/`, a sibling of `config.db`, not inside it** — SQLite is the wrong write shape for a 2 GiB append-only log, `config.db` is in `conf-files` and must stay small, and a spool filesystem failure must be distinguishable from a configuration failure.
11. **The spool record's payload is byte-for-byte the wire `collector.v1.Envelope`.** Replay after restart needs no domain type and no hand-written conversion, and protobuf's compatibility rules cover an in-place upgrade with a non-empty spool.
12. **Every record carries its own length, `seq` and CRC32C; a bad or short record truncates the segment at that point**, counted and logged. That is the correct response to the only way it happens — a power loss mid-append.
13. **The group-commit cap of 64 counts append *calls*, not records**, so one 256-envelope `IngestBatch` is one append and one `fsync`. That is what makes a reconnecting satellite's replay burst cost one disk sync instead of 256.
14. **One append call is the atom.** `admit` of a decoded batch and `accept` of one `IngestBatch` each commit entirely or not at all, which is why there is no within-batch partial ack to express.
15. **A cursor is a two-slot, fixed-size, `O_SYNC`, in-place file keyed by component id**, written on every ack with no allocation, no rename and no directory fsync. A torn slot is found by its CRC and the other slot is a valid earlier position, which costs a replay and never a loss.
16. **`max_lag` must fire before the ceiling does, and that is what makes W3's "drop at the edge" correct** rather than a coin flip: a spool filled by a stuck cursor is always resolved by abandoning that cursor, so the ceiling is reachable only when the admit rate is far above what the node was configured for — in which case a stale local `/latest` is an accurate report. Enforced by a `spool_time_remaining_seconds` gauge and a startup warning, not by `plan()`, because the admit rate is a runtime fact.
17. **`drop-oldest` is rejected as a ceiling policy**: it converts "this node is misconfigured" into "this node has holes in its history", and D-4's indefinite raw retention only means something if raw is complete.
18. **Recovery scans only the newest segment**, so start-up cost is bounded by the 8 MiB segment size regardless of spool depth. A cursor ahead of the tail is clamped back; a cursor behind the oldest segment is clamped forward with a counted, named gap.
19. **The collector RPCs move to a new `collector.v1.Collector` service in a new file; `WeatherV1` is not touched.** `WeatherV1` carries app-facing query RPCs with `google.api.http` annotations onto the surface W1 froze, and is registered at three separate sites.
20. **`Ingest(stream IngestBatch) returns (stream IngestAck)`** — bidirectional and acked, replacing `SendWeatherReadings(stream WeatherReading) returns (Empty)`, under which no sender could ever learn what its collector persisted, with or without a local buffer.
21. **An ack means "durably appended to the collector's spool", not "written to PostgreSQL."** The collector's database write is a cursor on that same spool, so acking on PostgreSQL would make a stalled database back-pressure every satellite in the fleet — the coupling W3 §2 removed.
22. **The ack is cumulative in the sender's coordinate space**: `IngestBatch.through` and `IngestAck.through` are the sender's spool `seq` of the last envelope in the batch. A per-origin ack cannot advance a relay's cursor, because a cursor is a position in one log and a relay's log interleaves origins. For a leaf satellite the two are numerically identical.
23. **`IngestAck.known_through` carries the collector's per-origin high water, once at stream open, as advisory fast resume.** The sender may skip a leading run it demonstrably already delivered; ignoring it costs only deduplicated resends.
24. **At most 8 batches / 2048 envelopes are in flight unacked**, bounding what a slow collector can cost a satellite — the backpressure an unbounded `stream.Send` lacks.
25. **Reconnection is the supervisor's backoff** (exponential, full jitter, 1 s base, 60 s cap), not a goroutine the component spawns per failure. The current code spawns one per failed send *and* per connection-state transition, guarded by an unsynchronised `bool`.
26. **`Ingress` gains `accept()` alongside `admit()`, and `accept` never reassigns provenance.** Running `admit`'s `seq = seq_table.next(...)` on a forwarded envelope would destroy the identity the ack and the dedup both depend on.
27. **Idempotency is a dedup gate in front of the spool, not a database constraint**: a persisted per-origin high water plus a 4096-entry in-memory seen-window per origin. A constraint fires after fan-out, by which time the duplicate has already reached downstream peers, the snapshot and the live stream.
28. **W7 adds no columns to the raw hypertable.** The database backstop is the natural key `(station_id, time)` the aggregates already require, with W3's `ON CONFLICT DO UPDATE`; `(origin, seq)` on 19 GB/year of rows for a property enforced at the spool gate is cost with no consumer. `origin` as a provenance column is W4's call.
29. **At-least-once delivery plus an idempotent admission gate plus an idempotent write is effectively-once** — three layers, and the one that fires in practice is the first.
30. **REST ingest is the same contract, not a second tier**: `POST /v1/ingest` takes the same `IngestBatch`, returns the same `IngestAck`, and is a thin codec over the same `Ingress::accept`, the same atom and the same dedup gate. A retried POST is safe by construction, which is what makes the absent stream harmless.
31. **Envelopes are validated at the boundary**: `observed_at` within +5 min / −30 days of the collector's clock, a `station_key` that resolves, and at least one populated metric. `max_past_skew` is set above both `max_lag` and the worst-case ceiling so a legitimate long replay is never rejected.
32. **Registration carries identity only.** `Hello(NodeIdentity) returns (HelloAck)` carries node id, name, software version and station metadata, and no secrets. The 12 third-party-config fields and 4 plaintext secrets on `RemoteStationConfig` have no successor.
33. **Enrolment is a separate, separately gated RPC**: an operator-minted single-use token (stored Argon2id-hashed, compared constant-time), a CSR, a returned certificate or pinned SPKI. `Enroll` answers `FAILED_PRECONDITION` when no unexpired token exists, is rate-limited, and every attempt is logged — so "enrolment is closed", "wrong address" and "bad token" are three distinguishable answers.
34. **Admission is three layers and all of them precede the pipeline**: mTLS (CA or pinned SPKI) or a PSK-over-TLS at the handshake; a per-RPC interceptor that resolves the peer before any method body runs, so no stream exists for an unknown peer; and a per-envelope origin authorisation. An unknown peer is a counted rejection, never a stored reading with a debug log.
35. **Plaintext ingest is a plan-time error** unless `allow_insecure_ingest = true` is set explicitly, in which case it is logged at `WARN` on every start. W3 §6.6's plaintext h2c is fine for a query and not for ingest.
36. **`relay = false` is the default, so a peer may speak only for itself**; relaying is an explicit per-peer grant with an explicit `origins` list. A compromised leaf cannot inject history for another station.
37. **The collector uploads on behalf of the fleet by selecting origins, using its own credentials.** An uploader declares `for = [<stations>]`, validated in `plan()`; a satellite has no uploader components at all, so no satellite code path reads a third-party credential in order to send it anywhere.
38. **`for`'s arity is the uploader's, not the selector's**: `wu`, `pws` and `aprs` are one component per remote account and take exactly one station; more is a plan error. The list form exists for fleet-identity kinds and avoids a later migration.
39. **An APRS callsign travels in `StationMeta` and the passcode does not.** A callsign is public identity; beaconing on someone's behalf stays an explicit operator action on the collector. Aeris never needed its secret on the wire at all — it is a fetch that needs a location and the collector's own account.
40. **"How far behind is each satellite" is two gauges** — `uplink_ack_lag_seconds{peer}` and `spool_lag_envelopes{cursor}` — plus `IngestBatch.sender_depth`, one varint per batch, so the collector can render fleet-wide lag without scraping a satellite whose link is down. Peer-reported numbers are labelled as such.
41. **`uplink_loop_refused_total` non-zero is a bug in this code, not a configuration problem** — the filter should have caught it and the invariant did. Stated so the counter has a meaning when it moves.
42. **A Go satellite is supported through a compat shim with three stated limitations** (no resume or cross-reconnect dedup, no relay, no field presence), removed when `peer_software_info` shows no Go-era peers. A Rust satellite against a Go collector falls back and is at-most-once, logged at `WARN` and exported as `uplink_degraded`, with `require_ack = true` available to make it a hard failure instead.

---

# Open questions

Each is a decision for the owner, with options and a recommendation.

**OQ-W7-1. Does a satellite also write its own local database?**
§8.2's satellite has no `[database]`, so its spool's only consumer is the uplink and `[spool].ceiling`
is its entire outage budget. The alternative is a satellite that also runs `writer:timescale` locally,
giving it its own history and its own `/latest`.
*Options:* (a) uplink only — the current Go deployment's shape; (b) local database optional, which the
design already supports with no change, since a second cursor on the same spool is the whole of it.
**Recommendation: (b), documented but not default.** It costs nothing in this design — two cursors on
one spool — and it is the difference between a satellite that can serve its own `/latest` during an
outage and one that cannot. Leave it off by default because most satellites are small machines.

**OQ-W7-2. Should `[spool].ceiling` default lower on a satellite than on a collector?**
2 GiB is ~100 days at the measured rate and ~12 days at the worst case. A satellite on an 8 GB SD
card may not want to commit 2 GiB.
*Options:* (a) one default everywhere, and §3.5's warning catches a bad combination; (b) a lower
satellite default (256 MiB ≈ 1.5–12 days), which risks silently inverting the `max_lag` invariant.
**Recommendation: (a).** One default and one loud warning is better than two defaults and a subtle
interaction, and an operator who sets 256 MiB gets told what it implies.

**OQ-W7-3. Who may enrol: the collector, or an operator on the collector?**
§5.3 requires an operator-minted token per peer. A looser alternative is a long-lived fleet enrolment
secret that any node holding it can use.
*Options:* (a) per-peer single-use tokens; (b) one fleet secret; (c) both, with (b) off by default.
**Recommendation: (a).** A fleet secret is a credential that must be distributed to exactly the nodes
you are trying to authenticate, and it reintroduces a shared secret of the kind §6 removes. Enrolling
four satellites is four commands, once.

**OQ-W7-4. Should the collector's ack be delayed until the reading is also in PostgreSQL, under an opt-in?**
Decision 21 acks on spool durability. An operator who distrusts the collector's disk more than the
satellites' might want end-to-end confirmation.
*Options:* (a) never — spool durability is the promise; (b) an opt-in `ack_on = "database"` per peer.
**Recommendation: (a).** (b) makes every satellite's liveness depend on the collector's database, and
the collector's spool is not less durable than its database — it is the thing the database is written
*from*. If the concern is the collector's disk, the answer is the collector's disk.

**OQ-W7-5. Is `sender_depth` enough, or should a peer report its full spool state?**
One varint per batch gives the collector a lag number. A richer `PeerStatus` message (oldest unacked
age, spool bytes, component states) would let W5 render a satellite's health without scraping it.
*Options:* (a) `sender_depth` only; (b) add a periodic `PeerStatus` on the same stream.
**Recommendation: (a) now, (b) when W5 asks.** W5 owns what an operator needs to see, and inventing a
status message before that document exists is how two work items end up disagreeing about it. The
field is additive.

**OQ-W7-6. When is the Go compat shim removed?**
§9.2 says "when `peer_software_info` shows no Go-era peers", which is a condition, not a date.
*Options:* (a) a W9 deliverable gated on the metric; (b) a named release.
**Recommendation: (a).** A date would either strand a peer or be moved. The metric makes the
condition observable, which is the part that was missing.

---

# Assumptions about other work items

**About W2a (`docs/component-model.md`)**

1. **W2a §4.1's `uplink:collector:<peer>` trigger `OnReading { NotFrom(self), Durable, 10 s }` is
   corrected to `NotFrom(peer)`** (Decision 3). `NotFrom(self)` matches every envelope in the spool
   and the uplink would send nothing. This document is written against `NotFrom(peer)`.
2. **W2a Decision 63's `ReadingFilter::NotFrom(origin)` is a membership test over the path**, as W3
   assumption 42 states, and remains correct under §2.1's ordered path — `has_visited` is unchanged.
3. **W2a §4a.3's `Transport` → `Trigger` implication table is scoped to station components.** The
   uplink holds `GrpcOut { streaming: true }` and is `OnReading`, because the trigger describes how
   work arrives and the transport describes a resource owned across ticks. `writer:timescale` has the
   same shape with a pool handle.
4. **`StationIdentity::FromRegistration` (Decision 45) is the ingest side and needs no new
   machinery**: `GrpcIn`/`HttpIn` imply `Service` and provide `HttpRoutes`, cardinality is `N` because
   identity is not `Fixed` (Decision 42), and a new peer is a configuration row.
5. **`StationRegistry` is `Write` for `ingest:*` alone** (§4.1b), because registration mints keys.
   §5.1's `peers`, `peer_origins` and `stations` tables are that capability's registration path, not a
   new capability.
6. **Decision 46's single authentication point is where §5.4 lives**, and the resolved `PeerId`
   reaches `accept` through `FrameMeta`. W2a does not enumerate `FrameMeta`'s fields; this document
   assumes a peer-identity field is among them.
7. **Decision 62's spool is this document's spool.** §3's format is the one `writer:timescale` reads
   from its own cursor; nothing in §3 is collector-specific.
8. **Decisions 28 and 29 own reconnection.** The uplink reports an outcome and the supervisor owns
   `Failed → Backoff → Starting`; §4.3's state machine describes what happens inside one attempt.
9. **Decision 50 holds for collector-side uploaders**: `for`-selected stations resolve through
   `WindowedView`, and an uploader is never handed a `ReadingStore`. A forwarded envelope is an
   envelope, so "upload on behalf of a remote station" needs no special case.

**About W3 (`docs/service-architecture.md`)**

10. **Assumption 44's `NotFrom(self)` is corrected to `NotFrom(peer)`** — the same correction as
    assumption 1, recorded against both documents because W6 will grep for both.
11. **`seq` is per origin, not per `(origin, station)`** (Decision 4), which narrows §2.2's
    annotation. W3's `Seq` type and `Provenance`'s privacy are unchanged.
12. **`PathSet` becomes an ordered `SmallVec<[OriginId; 8]>`** rather than a 64-bit bitset with an
    overflow list (Decision 6). With `MAX_PATH = 8` the bitset is an optimisation with no benefit and
    an overflow case to guard, and ordering is required for §2.1's exact receive check. W3 owns the
    in-memory type; this is a request, not a unilateral change.
13. **`Ingress` gains `accept()`** (Decision 26). §2.6's `admit` pipeline is unchanged for locally
    measured readings; `accept` is a second entry point that preserves provenance. Without it,
    §2.6 step 3 reassigns a forwarded envelope's `seq`.
14. **§2.4's group-commit cap of 64 counts append calls, not records** (Decision 13). W3's own
    rationale — "coalescing 64 appends into one fsync" for a replay burst — reads naturally this way;
    stated because the other reading would cap a batch at 64 envelopes and contradict the 256-envelope
    read-ahead in the same table.
15. **§2.4's spool sizes and §2.7's policies are adopted unchanged**: 8 MiB segments, 50 ms commit
    window, 2 GiB ceiling, 7-day `max_lag`, 256-envelope read-ahead, 10 s uplink deadline, drop-at-the-edge
    on `SpoolFull`, fatal on an unwritable spool. §3.5 adds the invariant that makes the ceiling policy
    right and the gauge that makes it watchable.
16. **§3.6's 15 s uplink shutdown grace is the `Draining` budget** in §4.3.
17. **Decision 89 (proto3 `optional` on every metric field) is load-bearing for the spool as well as
    the wire** (§3.2), because the spool payload *is* the wire message. Without it a `NULL` becomes
    `0.0` across a restart as well as across a hop.
18. **§6.1's `grpc_port` keeps its own socket and W3 Decision 92 means a satellite's endpoint does not
    change at upgrade**, which is what lets `ingest:grpc` be a drop-in for `grpcreceiver`.
19. **§6.5's single schema and single pair of conversions serve the spool too**; there is no
    spool-specific encoding and no hand-written mapping anywhere in this document.
20. **§6.6's plaintext h2c does not extend to ingest** (Decision 35), which is a narrowing of W3's
    transport behaviour for one route table, not a change to it.
21. **W3's `SpooledSink` adapter is what wraps the uplink** (assumption 44's second half), and a
    filtered envelope counts toward `Acked(n)` (Decision 9) — otherwise the cursor deadlocks.

**About W2 (metric registry and codegen)**

22. **The generator emits `collector.proto`'s `Envelope` with `weather.v1.Reading` as a field**, from
    the same registry that produces the REST and gRPC surfaces, so the spool, the uplink and the
    ingest endpoint cannot disagree about a metric.
23. **Every metric field is `optional`** (W3 Decision 89). This is the one item another work item must
    execute for this document to be correct.
24. **`Reading` is `Option`-valued and registry-typed**, so an un-reported metric is `None` end to
    end — admit, enrich, spool, wire, accept, write.

**About W4 (database schema)**

25. **The raw hypertable keeps `UNIQUE (station_id, time)`** and W3's writer keeps
    `ON CONFLICT (station_id, time) DO UPDATE`. §4.4's effectively-once claim depends on this and on
    nothing else in the schema.
26. **`origin` is worth a column on the raw table for provenance; `(origin, seq)` is not a uniqueness
    key** (Decision 28). W4 decides whether provenance is stored and in what form.
27. **A collector-written row can be `NULL` in any metric column** (W3 assumption 45's consequence),
    including the `winddir` case W3 §2.5 consequence 4 flags against OQ-W1-2.

**About W5 (management UI)**

28. **W5 owns the translation of every term in this document** (D-36). "`uplink:suncrest` is
    `Draining`" is not a sentence for an operator.
29. **§7's per-peer five-number row is the view an operator needs** — state, ack lag as a duration,
    unacked envelopes, peer-reported depth, last seen. W5 owns the rendering; this document owns that
    all five are available as numbers.
30. **W5 surfaces outstanding enrolment tokens and the enrolment log** (§5.3), because leaving
    enrolment open is the mistake and it should be visible.
31. **W5 surfaces §3.5's configuration advisory** (`ceiling < max_lag × rate`) and
    `spool_time_remaining_seconds` as a duration.
32. **`peer_reported_depth_envelopes` must be labelled as peer-reported** wherever it is shown.

**About W9 (data migration)**

33. **W9 moves each `remote_stations` row's enabled services into collector-side `[[uploader]]` rows,
    drops the four plaintext secret columns, and retires the table** in favour of `peers`,
    `peer_origins` and `stations` (§6.3).
34. **W9 recommends rotating all four migrated credentials**, because they crossed the wire in
    plaintext and sat in a second database.
35. **W9 owns removing the Go compat shim**, gated on `peer_software_info` showing no Go-era peers
    (§9.2), and owns allocating a `NodeId` for each existing node at migration — the `station_id`
    UUIDs already in `remote_stations` are *station* identities, not node identities, and must not be
    reused as `NodeId`s.

**About W1 (frozen contract)**

36. **`POST /v1/ingest` is additive and collides with nothing**; W1's `/v1/*` surface is
    `/v1/weather/span/{spanDuration}` and `/v1/weather/latest` only.
37. **`WeatherV1` is not modified**, so every `google.api.http` annotation W1 pinned is untouched, and
    the compat shim implements the existing `SendWeatherReadings` signature exactly.

---

# Proposed amendments to `DESIGN.md`

Per the amendment protocol, these are written out and **not applied**. This document carries on
consistently with the existing decisions; nothing in `DESIGN.md` is contradicted.

### A-W7-1 — addition, not a correction: D-43, the collector role

**D-31** says transport is orthogonal to role and names the collector role as the thing the Go
version lacks. It stops short of saying what the role *is*, which left W7 to supply it. Proposed as a
new decision rather than an edit:

> **D-43. A node's position in a topology is the set of components its configuration produces, not a
> mode it is in.** A collector is a node whose components include an ingest endpoint; a satellite is a
> node whose components include an uplink; a node with both is the default case. There is no `role`,
> `mode` or `is_collector` field in the configuration.

Rationale: without it, "collector" is a word in D-31's rationale with no referent, and the next person
to add a direction to the fleet has nothing to check their design against. Stating it as the absence
of a field is what makes it falsifiable.

### A-W7-2 — addition: D-44, registration carries identity only

Nothing in `DESIGN.md` says a node may not send its credentials to another node, and the Go
implementation does exactly that. D-33 covers how a secret is *held*, not whether it should be
*transmitted*.

> **D-44. Registration between nodes carries identity and station metadata only. A node never
> transmits a third-party credential to another node.** A collector that uploads on behalf of the
> fleet selects which origins it covers and uses its own credentials.

Rationale: the current `RemoteStationConfig` ships `aprs_password`, `wu_api_key`,
`aeris_client_secret` and `pws_password` on every registration, and `INSERT OR REPLACE`s them in
cleartext into the collector's `remote_stations` table — where **nothing reads them**. The intent
(the collector is the fleet's egress point) is sound; welding it to registration is what made the
exposure unavoidable. As a decision this is a one-line test a reviewer can apply to any future
inter-node message.

### A-W7-3 — addition: D-45, admission precedes the pipeline

D-29 specifies the data flow and a policy for a slow sink. It does not say where authorisation sits in
that flow, and the Go receiver puts it after the store.

> **D-45. Admission control for an inbound reading happens before the reading reaches the stream, and
> an unauthorised sender is a counted rejection.** There is no path by which a reading is stored and
> then found to be unauthorised.

Rationale: `grpcreceiver/station.go:175` pushes into the distributor and `:182` consults the registry,
so an unregistered peer's readings are stored under whatever station name they claim and the only
trace is a `Debugf`. W2a Decision 46 already implements this for stations; stating it in `DESIGN.md`
is what makes it apply to a direction nobody has invented yet. **Note:** this is close enough to W2a
Decision 46 that W6 may reasonably choose to record it only there. It is proposed here because the
decision is about the *flow* (D-29's ground) rather than about identity resolution.

### A-W7-4 — extension of D-30, flagged

**D-30** says "durable sinks must not lose data". §3.5 adopts W3 §2.7's decision that a spool at its
ceiling drops at the edge, which is a bounded, counted, loudly-reported exception to that sentence.
W3 already decided it; this records that the exception is load-bearing and has a stated invariant
behind it.

> **Extension to D-30:** a durable sink must not lose data *while the spool has room*. The spool is
> bounded, and at the bound the policy is to refuse at the edge — counted, logged and visible as a
> component failure — rather than to stall ingest or grow without limit. **`max_lag` must be reachable
> before the ceiling at the node's observed rate**; a configuration in which it is not is a warning at
> startup, because it inverts which of the two protections fires first.

Rationale: W3 §2.7 established that loss is certain either way at the ceiling and the only choice is
whether shutdown still works and whether the operator is told. What was missing is why the ceiling is
not reached in practice — which is the `max_lag` invariant — and that invariant is checkable, so it
belongs with the decision rather than in a section a reader may not reach.
