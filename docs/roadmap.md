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
| 0 | Type model + format spec | Decisions made (see §4); being written up here |
| 1 | Codec layer | ✅ `grahh-codecs` — trait, formats, picker |
| 2 | Chunked CSR adjacency | ✅ `grahh-adjacency` — `ChunkedCsr` + registry |
| 3 | Traversal/compression benchmark | ✅ `bench_traversal`|
| 4 | Schema/type-model crate | ⏳ later |
| 5 | Persistence (mmap / WAL / compaction) | ⏳ later |
| 6 | Query surface (primitives + language) | ⏳ later |

MVP gate: **Phase 4 + a join**, so the headline query works end-to-end from
one embedded file.

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

## 6. Next step — Phase 4: columnar property store

Goal: make the *other half* of the headline query real —
`expand(A, friends) -> friend ids -> project(name, age)`.

Proposed shape (subject to refinement):
- **Typed property columns:** per `NodeKind`, one column per property; each
  column is a `Vec` of typed values (or nulls) compressed by the codec picker.
  Keep the graph-sparse reality — not every node has every property.
- **Projection primitive:** `project(node_ids, [prop list]) -> typed values`,
  only decompressing requested columns (the DuckDB-style projection win).
- **Join with adjacency:** `expand` + `project` compose into the headline query.
- **Edge store:** stateful edge payloads keyed by edge id, columnar.
- **Schema descriptor** (`NodeKind`/`EdgeKind` registry) becomes first-class
  metadata — this is also the seam for future codegen.

Evaluation gate: a new/updated benchmark that measures the end-to-end
"friends + their name/age" query on a realistic graph.

---

## 7. Later phases

- **Phase 5 — schema/type-model crate:** formal `NodeKind`/`EdgeKind`/`Enum`
  types, property catalog, label index. Feeds codegen later.
- **Phase 6 — persistence:** serializable chunk format, mmap the compressed
  base ("on disk == in RAM"), WAL + memtable + eager compaction with dense
  renumbering.
- **Phase 7 — query surface:** materialized physical primitives first
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