# GRAHH-DB — Roadmap & Architecture Notes

An embedded, single-file, read-biased **graph database engine** built in Rust.
The central bet: *deeply nested traversal stays fast by pointer-chasing
through compressed chunks, and compression keeps the working set small enough
to live in RAM*.

This document is the single source of truth for the plan, the architecture,
and the decisions made so far. Update it as we go.

---

## 1. The premise (what we're trying to prove)

> For read-heavy workloads, a graph DB can answer *"user A's friends, and those
> friends' specific profile properties"* by expanding nodes through an
> **index-free-adjacency**-style structure (one array-index hop, no per-hop
> index seek) while keeping the whole topology **compressed** so it can stay
> resident in RAM.

Three sub-claims to validate with numbers, in order:

1. Traversal genuinely avoids per-hop index seeks (structure test).
2. Sorted neighbor lists compress dramatically with delta/bit-pack, so the
   topology footprint collapses (compression test).
3. Decompressing just the needed chunk stays fast enough that traversal
   through compressed data ≈ traversal through uncompressed data (RAM test).

---

## 2. Where we are (2026)

| Phase | Scope | Status |
|---|---|---|
| 0 | Type model + format spec | Decisions made (see §4); captured in this doc |
| 1 | Codec layer | ✅ `grahh-codecs` — trait, formats, picker |
| 2 | Chunked CSR adjacency | ✅ `grahh-adjacency` — `ChunkedCsr` + registry |
| 3 | Traversal/compression benchmark | ✅ `bench_traversal` |
| 4 | Persistence: byte format + database image | 🔄 4a/4d done; 4b (file/mmap) + 4c (WAL) later |
| 5 | Codegen / type-model crate | ⏳ later (unlocks typed property stores) |
| 6 | Query surface (primitives + language) | ⏳ later |

MVP gate: **Persistence (Phase 4)**, so the adjacency store round-trips
through a stable byte format. The headline nested query also needs the
property store, which was **removed pending codegen** (see §7): a
self-contained heterogeneous column store forces type erasure (rejected at the
API), and without codegen a "store" degenerates to a bare `Vec` wrapper.

---

## 3. Workspace layout

```
Cargo.toml            workspace (edition 2024), members:
grahh-codecs/         dependency-free compression codecs
grahh-persist/        wire format: header, errors, Reader/Writer primitives
grahh-index/          identity: label index (user key <-> dense internal id)
grahh-adjacency/      topology: chunked CSR + per-kind registry
libgrahh/             engine: database image, Database/DatabaseBuilder
docs/roadmap.md       this file
```

Build/test:
```
cargo test --workspace
cargo run --example mre -p libgrahh
cargo run --release --example bench_traversal -p grahh-adjacency
cargo run --release --example bench_persistence -p grahh-adjacency
cargo clippy --workspace --all-targets
```

---

## 4. Data model decisions (locked)

**Primitive type set** (closed; `PropertyType`): `U64 I64 F64 Bool Str Date
Bytes Null List` + future `Enum` (first complex type). Explicitly deferred to
TODO: `U32`, `UUID`.

**Two sorts, stratified** (property-graph model):
- `NodeKind` — primitive props + outgoing connections. May **not** directly
  hold another `NodeKind`.
- `EdgeKind` — optional primitive props + **exactly one** target `NodeKind`.
  May **not** hold another `EdgeKind`.
- `List<primitive>` (a column cell) ≢ `Vec<EdgeKind>` (a connection →
  adjacency store). They look similar in syntax; storage is entirely different.

**Edge semantics**:
- Directed by default; a **reverse (in) adjacency list is optional**, built
  only if a kind opts in (`set_inverse`). No automatic `followedBy`.
- Stateful vs pure edges = `EdgeKind.props` empty or not. Stateful edges get a
  columnar payload in the edge store; topology stays separate from payload.
- Self-loops allowed; multiple/parallel edges allowed; each is a distinct
  record.
- Edge→edge forbidden for now; escape hatch = reification (model the edge as a
  node) later if needed.

**Identity & ids**:
- Internal node ids are **dense `u64`**, renumbered at compaction — ephemeral
  handles, never stable user references.
- The **label index** (external key → internal id) is the only stable
  reference users hold; the authoritative node count belongs to a future node
  store, not the adjacency store.
- User-level tombstones are the user's concern; engine tombstones exist only in
  the compaction window.

**Codec contract** (`grahh-codecs`):
- Public API is closed & opaque: a `Codec<T>` trait (per column type) + a
  singular `CoreCodec` implementing it. Callers never see formats.
- Packed form is **self-describing**: `[ 1-byte format tag ][ format payload ]`.
  Decoding needs no prior knowledge.
- The **format set is open internally**: FOR, bitpack, RLE, plain today;
  adding a format = new module + tag registration, and old data still decodes
  (no migration).
- Encoder **probes each candidate with a counting sink**, picks the smallest,
  then re-encodes the winner into the real sink.

**Adjacency** (`grahh-adjacency`):
- One `ChunkedCsr` per edge kind (kind is a property of the store; direction is
  a query-layer concern). Neighbors are a flat, sorted node-id column, chunked
  (64 ids/chunk), each chunk compressed by the codec picker.
- `AdjacencyStore` = registry keyed by `EdgeKindId`, with optional in-CSRs.

---

## 5. Benchmark findings (Phase 3)

`cargo run --release --example bench_traversal` — 100k nodes / ~400k edges,
same synthetic graph for both stores.

```
footprint naive   ~ 3.1 MiB      (edge payload, 8 B/edge)
footprint csr     ~ 1.6 MiB      (offsets + all compressed chunks)
csr compression   ~ 1.9x smaller
csr format usage : [("bitpack", 6250 chunks)]       ← FOR never fired

single-hop expand : csr ~603 ns | naive ~471 ns     (+28%)
friends-of-friends: csr ~3.0 µs | naive ~0.31 µs    (~10x slower)
```

**Interpretation:**
- Structure premise holds: expansion is a sub-µs pointer-chase.
- **FOR never fires** on this synthetic graph: targets are *uniform* over the
  id space and chunks straddle node boundaries, so the `is_sorted()` gate
  rejects FOR. A social-graph shape (sorted, low-id lists) with node-aligned
  chunks is where FOR should shine — this benchmark's 1.9x **undersells** the
  architecture.
- The 10x 2-hop gap is the deliberate eager-`neighbors()` tradeoff: each
  expansion allocates + decodes a fresh `Vec`. Accepted for now (do not
  optimize pre-MVP); see the optimization backlog.

---

## 6. Next step — Phase 4: persistence (serialization first)

The storage layer is done (codecs + chunked CSR registry); before it can be
called a database it must survive process restarts. Persistence is staged:

**4a — serializable byte format (done).** Both `ChunkedCsr` and
`AdjacencyStore` round-trip losslessly through a self-describing, fixed-width,
little-endian byte stream (`Vec<u8>` in and out). Rules:
- **Fixed-width integers everywhere** — offsets/counts are `u64` LE, never
  `usize` (architecture-dependent), so files are portable.
- **Length-prefixed, self-delimiting** regions, so a reader never guesses
  where a chunk or blob ends and unknown regions are skippable.
- **The data describes itself**, chunk geometry included: the region records
  how many flat positions a chunk spans and the reader uses that, rather than
  assuming a compiled-in constant.
- Chunk payloads are written **verbatim** (they already carry their codec tag),
  so persisting is a copy, never a recompression — the first real claim on
  "on disk == in RAM".
- The header's **version field is data, not behaviour**: it says which layout
  the bytes are in. It is `0` (pre-1.0 — unstable, nothing promised). A layout
  change is handled by a separate migration tool that reads an old structure
  from scratch, not by branching inside this reader.
- **Validate at connect time, then trust**: structural checks (magic, version,
  geometry, monotonic offsets, counts, no trailing bytes) *and* every chunk
  payload (decodes, produces the declared id count, targets ordered *within each
  node's slice* — a chunk boundary can fall inside a slice, so node boundaries
  are not chunk boundaries). There is no unchecked load path; after a load,
  decoding has no failure path left.
- Cost and size of all this, plus the copy-vs-recompress claim, are measured:
  `cargo run --release --example bench_persistence`.

**Measured** (100k nodes / 400k edges, `bench_persistence`):
```
naive edge payload   3.05 MiB
csr payload          1.61 MiB   (800 KB offsets + 887 KB compressed chunks)
serialized image     1.70 MiB   (framing 100 KB = 16 B/chunk directory)
build csr            650   ms
serialize            9.2   ms   (186 MiB/s — a pure copy)
load                 42.9  ms   (40 MiB/s — validates every chunk payload)
load vs rebuild      15.2x faster
```
Framing is cheap and payloads are genuinely copied (format usage identical after
a load, expansion results identical). Load cost is dominated by decoding every
chunk for validation plus one allocation per chunk — the price of "validate at
connect", and the number to watch as images grow (~25 ms/MiB).

**4d — database image (done).** A registry alone was not a database: it had no
identity layer and no file-level envelope. Both now exist.
- `grahh-index` — `LabelIndex`: user `u64` key ↔ dense internal id, ids issued on
  first sight, persisted as two coded columns (sorted keys + the id permutation).
  The reverse direction is rebuilt on load, so it costs nothing on the wire.
- `libgrahh` — the envelope: `header`, region count, a fixed-size directory
  (`tag u32 | offset u64 | length u64`), then the regions. Offsets are absolute
  and regions contiguous: a gap, overlap, entry past the end or trailing byte is
  rejected. Unknown tags are *refused* — the format could skip them by length,
  but at version 0 there is no compatibility promise, so refusing beats
  half-reading a file we do not understand.
- `Database`/`DatabaseBuilder`: declare edges by key, ids are assigned on first
  sight, and `lookup → expand → key_of` is the first end-to-end query path.
- Cross-region check: every id the topology references — sources and targets,
  both directions — must be covered by the label index, so a dangling reference
  fails the load rather than surfacing later as a missing key. A region's
  `id_span` is derived (never stored), so this costs no format change. Known
  limit: an id with no edges is not yet addressable, because a store's span
  comes from the ids it actually stores.

**4b — file + mmap (deferred).** The engine performs no I/O of its own: every
`serialize`/`deserialize` is a pure function over bytes (sans-io), which is why
there is nothing to change when files arrive. 4b is therefore an *adapter*: read
the image in and `deserialize`, or map it and borrow the regions, in a separate
crate that the core does not depend on. What std lacks is `mmap` (a dependency or
platform FFI); `std::fs` itself exists, to be confirmed when 4b starts.

**4c — WAL / compaction / renumbering (even later).** Write-path concerns,
not needed until there are in-place mutations.

---

## 7. Later phases

- **Phase 5 — codegen / type-model crate:** formal `NodeKind`/`EdgeKind`/
  `Enum` types, property catalog, label index. The **typed property store was
  removed** (see §2): a self-contained heterogeneous column store forces type
  erasure, and without codegen it collapses to a bare `Vec` wrapper. Typed
  per-kind stores return **only via codegen** — a `#[derive]` that generates
  the store struct — so the schema/type-model crate feeds that path.
- **Phase 6 — query surface:** materialized physical primitives first
  (`lookup`, `project`, `expand`, edge props, scan), then syntax on top.
- **Phase 7 — I/O adapters (later):** the core is sans-io by construction, so
  `std::fs`, `mmap` and async runtimes belong in separate crates that depend on
  the engine, never the reverse. `no-std`/`no-alloc` is aspirational and
  currently blocked by the stores themselves: offsets and chunks are `Vec`/`Box`
  shaped, `PersistenceError` carries a `String`, and the builders use maps.
  A borrowed/arena representation is the prerequisite, and it is the same work
  `mmap` wants.

---

## 8. Optimization backlog (do NOT do before MVP)

Deliberate deferrals; revisit after the MVP query works end-to-end.

1. **Reuse decode buffers across expansions** — one `Vec` per call today; a
   shared window would cut the ~10x 2-hop gap. Revisit when traversal depth is
   a measured cost.
2. **Node-aligned chunks** so FOR engages on real sorted neighbor lists
   (chunks currently straddle node boundaries). Should dramatically beat the
   synthetic 1.9x.
3. **Codec-choice tuning** — today it's purely "smallest size"; add a decode
   complexity weight / tiebreak when decode cost matters.
4. **`take(node, n)` partial expansion** — for queries that only need the
   first few neighbors (the lazy path we deliberately dropped).
5. **Zero-copy decode / decoder iterators** — memory-mapped chunk reads that
   yield values instead of materializing `Vec`s.
6. **`Box<[_]>` → plain `Vec`s / arena** decisions were consciously kept
   boring pre-MVP; revisit ownership/layout with real memory numbers.
7. **Bit-packing SIMD / word-at-a-time paths** — once pack/unpack shows up in
   profiles.
8. **Dependent properties** (e.g. follower count maintained engine-side) —
   discussed, intentionally out of scope for the adjacency store.
9. **Offsets are the uncompressed column** — measured at ~45% of the store on
   `bench_persistence` (800 KB of a 1.70 MiB image). Its share is `8 / (8 + c·D)`
   for `c` bytes per compressed id and average degree `D`, so it *grows* as the
   codec does better and as average degree drops: at D=4 with FOR-style 0.8 B/id
   the offsets would be ~71% of the working set, i.e. the column eats the win the
   compression architecture exists to deliver. Ideas to benchmark against each
   other, cheapest first:
   (a) `u32` offsets when `edge_count < 2^32` — 2x cut, still O(1), no new
   concepts; (b) fixed-width bitpacked offsets, width = `bits(edge_count)` —
   ~2.4 B at 400k edges and grows only logarithmically, O(1) shift+mask but needs
   a random-access bit reader; (c) coarse block table + bitpacked per-node
   degrees — ~1.5 B, ~8 adds per expansion; (d) sparse/rank form for kinds that
   touch few ids. Every one of these changes RAM *and* file layout together, so
   it is a format change (migration tool), and the author is against
   varied-width offsets on principle — benchmark the rest before proposing it.
10. **One allocation per chunk on load** — 6,250 `Box<[u8]>` for the benchmark
    image, visible in the 42.9 ms load. A single arena (one buffer + ranges)
    removes it and is the shape a borrowed/mapped reader will want anyway.
11. **Cross-region check is exact for referenced ids** — the adjacency region
    derives an `id_span` (highest id stored, targets included) and the image
    refuses one that outruns the label index, so a dangling reference — a
    source *or* a target — fails the load instead of surfacing later as a
    missing key. Remaining gap: an id with no edges at all is invisible to
    `expand` because a store's span comes from the ids it stores; closing that
    means chunking sized by the identity layer rather than by stored ids.