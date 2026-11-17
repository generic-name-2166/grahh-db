# GRAHH-DB Roadmap

GRAHH-DB is a graph database engine in Rust. It stores directed edges and typed
properties using an LSM-tree tablet (LSMT) structure, resolves user keys to
dense internal ids, and expands adjacency with index-free lookups.

## Crates

| Crate | Purpose |
|-------|---------|
| grahh-codecs | Columnar compression: FOR, bit-pack, RLE, plain; self-describing 1-byte tag |
| grahh-lsm | LSMT engine: WAL, memtable (`Cell::{One,ZeroOrOne,Many}`), tablets, manifest, levels, compaction; shared wire/persist primitives |
| libgrahh | Database front door: identity, adjacency store, and properties store backed by LSMT (`save`/`load` through `grahh-lsm`) |

## Phase 1 - Codecs (done)

`grahh-codecs` provides a single `CoreCodec` that probes FOR (sorted input),
bit-packing, RLE, and plain encoding with a counting sink, picks the smallest,
and writes a self-describing 1-byte format tag. Round-trip is exact for any
`Vec<u64>`.

## Phase 2 - Persist primitives (done, integrated)

Wire-format primitives (fixed-width little-endian fields, length-prefixed
regions, `MAGIC`/version headers, `Reader`/`Writer`, `PersistenceError`) are
owned by `grahh-lsm::persist` and used by the storage layer.

## Phase 3 - Adjacency store (done, LSMT-backed)

Adjacency is implemented in `libgrahh` on top of the LSMT structure:
- Durable storage: each edge is a row under `(source_node_id, edge_kind_id)`.
  Adjacency values are encoded as raw blobs (`[target_id][edge_id]`, 16 bytes)
  and use `Many` cardinality / replace semantics per key generation.
- Query view: `AdjacencyStore`/`ChunkedCsr` provide index-free expansion
  (compressed chunked neighbor columns) rebuilt from LSMT data.
- `DatabaseBuilder`/`Database` manage label mapping, edge kinds, inverse
  declarations, and save/load through `grahh-lsm::Engine`.

## Phase 4 - LSM tablet engine (done, MVP+)

Key: `(node_id: u64, kind_id: u32)`.
Value rows: raw byte blobs. Adjacency uses fixed 16-byte `[target_id][edge_id]`;
properties use typed variable-length encodings. The same LSMT list/cell
structure supports `One | ZeroOrOne | Many`.

- WAL: append-only, CRC32, replay with crash-tail tolerance, `max_seq` boundary.
- Memtable: `BTreeMap<Key, Cell>`, byte-budget flush.
- Tablet (v1): bloom + exact key/length directory, per-kind chunks, node-delta
  column + blob value column, full validation on load.
- Manifest/levels: append-only membership, L0 newest-first, basic L0+L1->L1
  major compaction.
- Engine: `add`/`remove` (adjacency), `add_raw`/`remove_raw`/`get_raw`
  (properties/raw), flush, open/recovery, compaction.

## Phase 5 - Properties / type model (in progress)

`libgrahh::properties` implements an LSMT-backed property store:
- Typed values (`Bool/U64/I64/F64/String/Bytes/NodeRef`) with schema-guided
  encoding.
- `PropertyKindId`, `PropertyType`, `Cardinality`, `PropertyDef`.
- `PropertyStore` reads/writes through `Engine` raw LSMT list storage, sharing
  the same `Cell::Many`/replace semantics as adjacency.

Remaining: schema catalog/registry, node/edge kind assignment, namespace
management between edge kinds and property kinds, derived/triggered properties,
and higher-level `Database` property APIs.

## Phase 6 - mmap / file I/O (later)

The MVP engine owns I/O with `std::fs`. A later adapter can separate file
access from encoding (read/mmap tablet regions) while keeping on-disk bytes
stable.

## Non-goals

- No tombstones: a removal is a new generation that omits the row.
- No prefix compression: fixed 12-byte keys make it unnecessary.
- No inverse kinds at engine level: `follows` and `followed_by` are separate
  user-declared kinds.
- No value handles / indirection: columnar position is the pointer.
