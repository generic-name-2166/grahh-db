# GRAHH-DB Roadmap

GRAHH-DB is a graph database engine in Rust. It stores directed edges grouped by
edge kind, resolves user keys to dense internal ids, and expands adjacency with
index-free CSR lookups. Persistence now goes through an LSM-tree key-value layer
instead of a single serialized image.

## Crates

| Crate | Purpose |
|-------|---------|
| grahh-codecs | Columnar compression: FOR, bit-pack, RLE, plain; self-describing 1-byte tag |
| grahh-persist | Wire format: Writer/Reader, fixed-width LE fields, length-prefixed regions |
| grahh-index | LabelIndex: user u64 key <-> dense internal node id |
| grahh-adjacency | ChunkedCsr + AdjacencyStore: per-kind CSR topology, index-free expansion |
| grahh-lsm | LSM-tree KV engine: WAL, memtable, tablet, manifest, levels, engine |
| grahh-properties | Typed property store: NodeKind/EdgeKind/Enum types, property catalog |
| libgrahh | Database front door: identity + topology, save/load through grahh-lsm |

## Phase 1 - Codecs (done)

`grahh-codecs` provides a single `CoreCodec` that probes FOR (sorted input),
bit-packing, RLE, and plain encoding with a counting sink, picks the smallest,
and writes a self-describing 1-byte format tag. The public API is closed;
formats are registered internally in `format.rs`. Round-trip is exact for any
`Vec<u64>`.

## Phase 2 - Persist primitives (done)

`grahh-persist` owns the byte-level rules shared by every store:
- Fixed-width little-endian everywhere (`u64`, never `usize`).
- Length-prefixed regions; unknown regions are skippable.
- Self-describing regions: `MAGIC` + `VERSION` header, codec tag on columns.
- Payloads copied verbatim; "on disk == in RAM".
- Validate at load, then trust; no unchecked load paths.

`PersistenceError` covers truncation, bad magic, unsupported version, corrupt
structure, chunk decode failures, length mismatches, duplicate kinds, trailing
bytes, unknown region tags.

## Phase 3 - Adjacency store (done)

`grahh-adjacency` provides:
- `ChunkedCsr`: one CSR per edge kind. Flat offset array + flat neighbor-id
  column split into fixed-size codec-compressed chunks. Immutable after build.
  Reading a node's neighbors decodes only the chunk(s) covering that node's
  slice.
- `AdjacencyStore`: registry of one directional CSR per edge kind (out always,
  in optional via `set_inverse`). `neighbors(node, kind, dir)` is the expansion
  primitive. `id_span()` reports the highest referenced id for cross-region
  validation.
- Both levels serialize/deserialize to `Vec<u8>` through `grahh-persist`.
  Full validation on load: magic, version, geometry, monotonic offsets, every
  chunk payload decodes and produces the declared id count, targets ordered
  within each node's slice.

## Phase 4 - LSM tablet engine (done, MVP)

Replaced the single serialized image with a durable LSM key-value layer.

Key: `(node_id: u64, kind_id: u32)` - 12 bytes LE.
Value: `[target_id: u64][edge_id: u64]` for adjacency deltas. Variable-length
property rows reuse the same key space later.

### WAL (`grahh-lsm/src/wal.rs`)
Append-only binary log of delta records (Add/Remove of a `(target_id, edge_id)`
under a `(node_id, kind_id)` key). File header with magic/version. Records are
self-delimiting via keylen/vallen. CRC32 per record. Replayed on restart into a
fresh memtable; a partial final record is treated as a crash tail. Records at or
below the manifest's flushed `max_seq` are skipped, so a crash between the
manifest append and WAL truncation cannot double-apply a batch.

### Memtable (`grahh-lsm/src/memtable.rs`)
`BTreeMap<(node_id, kind_id), Cell>` where Cell is `One | ZeroOrOne | Many`.
Flush trigger by byte budget (e.g. 4 MiB). On flush, cells are serialized into
tablet rows; `Many` emits one row per item; `One`/`ZeroOrOne` emit one row (or
zero for None).

### Tablet (`grahh-lsm/src/tablet.rs`)
Immutable columnar run file partitioned by `kind_id` into chunks. Rows sorted by
`node_id`, then `target_id` within each chunk. Bloom filter over
`(node_id, kind_id)` keys, followed by an exact per-key length directory (the
local stand-in for the future schema length property). The directory makes bloom
false positives harmless and lets an emptied key authoritatively shadow older
rows. Chunk directory with `kind_id`, `row_count`, `data_offset`, `data_length`
per chunk. Footer back-pointers to header start.

### Manifest (`grahh-lsm/src/manifest.rs`)
Append-only text log of `add <level> <filename> <max_seq>` / `remove <level>
<filename>`, replayed on open. `max_seq` is the flush commit point. A trailing
partial line is ignored.

### Levels (`grahh-lsm/src/levels.rs`)
L0 = memtable flushes (overlapping key ranges, checked newest-first). L1, L2, ...
= each ~10x previous size; non-overlapping key ranges within a level.

### Engine (`grahh-lsm/src/engine.rs`)
Top-level KV engine on the LSM tablet. `add`/`remove` append WAL deltas and
materialize the key's full current value into the memtable; `flush` writes a
columnar tablet; `get` checks memtable then tablets newest-first; `scan_kind`
rebuilds one kind with replace semantics. For a given `(node_id, kind_id)`, the
newest tablet containing the key fully determines the result (replace semantics,
not merge). On open the engine removes stale `.tablet.tmp` files and unreferenced
tablets before replaying the WAL.

`libgrahh` keeps its query structures in memory: `Database::save(&mut Engine)`
exports labels, inverse declarations, and forward edges; `Database::load(&Engine)`
rebuilds and validates them. Reserved kind ids `u32::MAX` (labels) and
`u32::MAX - 1` (inverse flags) keep metadata out of the user kind space.

Compaction, mmap, and renumbering remain deferred (post-MVP).

## Phase 5 - Codegen / type model (later)

Formal `NodeKind`/`EdgeKind`/`Enum` types, property catalog, label index. The
typed property store (`grahh-properties`) defines schema-level types and
cardinality constraints (`One`, `ZeroOrOne`, `Many`). Dependent properties
(e.g. synced follower counts) are computed by triggers, not stored directly.

## Phase 6 - mmap / file I/O (later)

Serialize/deserialize stay pure functions over bytes, but the MVP `grahh-lsm`
engine currently owns its I/O directly with `std::fs` (WAL, manifest, tablet
create/fsync/rename). A later adapter can separate file access from encoding:
read or mmap a tablet, then borrow its regions, while keeping the on-disk bytes
unchanged. `std::fs` exists; `mmap` requires a dependency or platform FFI.

## Non-goals

- No tombstones: a removal is a new generation that omits the row.
- No prefix compression: fixed 12-byte keys make it unnecessary.
- No inverse kinds at engine level: `follows` and `followed_by` are separate
  user-declared kinds.
- No value handles / indirection: columnar position is the pointer.