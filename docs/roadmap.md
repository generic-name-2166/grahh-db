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
| 4 | Persistence: serializable byte format | 🔄 4a done (byte format + validation); 4b/4c later |
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
grahh-adjacency/      topology: chunked CSR + per-kind registry
libgrahh/             (empty stub — future umbrella/engine crate)
docs/roadmap.md       this file
```

Build/test:
```
cargo test -p grahh-codecs
cargo test -p grahh-adjacency
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
  payload (decodes, produces the declared id count, targets in order). There is
  no unchecked load path; after a load, decoding has no failure path left.
- Cost and size of all this, plus the copy-vs-recompress claim, are measured:
  `cargo run --release --example bench_persistence`.

**4b — file + mmap (deferred).** This std exposes no `std::fs`/`File`/`mmap`
anywhere yet; real disk I/O and memory-mapped reads land once we confirm the
primitive exists. The byte format is the file format, so 4b is a thin layer on
top of 4a.

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