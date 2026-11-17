# GRAHH-DB — LSM Tablet Spec

**Status:** Implemented (MVP) — durable key-value layer replacing image-based persistence.

## 1. Goal

An LSM-tree key-value store where:

- **Key:** `(node_id: u64, kind_id: u32)` — 12 bytes, little-endian, fixed width.
- **Value:** schema-defined serialized bytes, variable length.
- **Multimap on disk:** a tablet may hold multiple rows with the same `(node_id, kind_id)` key (a 1-to-many list is stored as one row per `(target_id, edge_id)` pair). In memory, each key maps to a single cell value.
- **Sort order:** lexicographic on raw LE bytes of the 12-byte key; within a key, rows are sorted by `target_id`.
- **No tombstones.** A removal is recorded as a new tablet generation that simply omits the removed row. Older tablet rows for the same key are replaced, not unioned (see §6 read semantics).
- **No inverse kinds at engine level.** `follows` and `followed_by` are separate user-declared kinds; sync is a schema-level trigger, not engine behavior.
- **No prefix compression in MVP.** Fixed 12-byte keys make it unnecessary.
- **Mandatory bloom filters** on every tablet.
- **Bloom false positives are harmless.** Each tablet also carries an exact per-key length directory. A bloom hit is confirmed against that directory; a key missing from it is a false positive and is skipped. The directory records keys whose list is empty, so a tablet can authoritatively shadow older rows without tombstones. This is the tablet-local "length property" used by LSMT reads.

## 2. Data model

```
Key   = [node_id: u64 LE][kind_id: u32 LE]     // 12 bytes
Value = [target_id: u64 LE][edge_id: u64 LE]   // 16 bytes for adjacency delta rows
                              or serialized bytes for property rows
```

> **Current implementation:** the engine supports adjacency values (`add`/`remove`/
> `get`/`scan_kind` over 16-byte `[target_id, edge_id]`) and raw variable-length
> values (`add_raw`/`get_raw`/`scan_kind_raw`) for properties. Both share the
> same LSMT `(node_id, kind_id)` key space and `Cell` list storage. Adjacency and
> properties stores are implemented in `libgrahh`.

In memory, a key maps to a cell:

```rust
enum Cell {
    One(Vec<u8>),             // exactly 1 — cardinality enforced at read
    ZeroOrOne(Option<Vec<u8>>), // 0 or 1
    Many(Vec<Vec<u8>>),         // 0..N — one row per (target_id, edge_id)
}
```

## 3. WAL (write-ahead log)

Append-only binary log. File header once at start; records appended contiguously. Each record is a **delta** — one Add or Remove of a single `(target_id, edge_id)` item under a `(node_id, kind_id)` key — never a full list replacement.

### File header

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | magic `b"GWAL"` |
| 4 | 4 | version `u32 LE` (= 0 for pre-1.0; anything can change) |

### Record

| Offset | Size | Field |
|--------|------|-------|
| 0 | 8 | `seq` u64 LE — global write sequence |
| 8 | 1 | `rtype` u8 — 1 = Add item, 2 = Remove item |
| 9 | 4 | `keylen` u32 LE — key length (12 in MVP) |
| 13 | 4 | `vallen` u32 LE — value length (16 for adjacency deltas) |
| 17 | `keylen` | `key` raw bytes — `[node_id][kind_id]` |
| 17+K | `vallen` | `value` raw bytes — `[target_id][edge_id]` for deltas |
| ... | 4 | `crc32` u32 LE — CRC32 over `[seq .. end of value]` |

No per-record magic. No per-record file offsets. Records are self-delimiting via `keylen`/`vallen`.

### Restart semantics

- Validate header magic + version. If version is unrecognized, error (no silent data loss).
- Replay records in `seq` order into a fresh memtable, applying Add/Remove deltas to the appropriate cell.
- Skip any record whose `seq` is at or below the manifest's flushed watermark (`max_seq`). This makes the manifest append the commit point, so a crash between the append and WAL truncation cannot apply a flushed batch twice.
- Track `max_seq` so new writes continue the sequence.
- WAL truncated after a clean flush checkpoint.
- On open, delete stale `*.tablet.tmp` files and any `*.tablet` file the manifest does not reference. An orphan tablet's WAL records are still present, so replay rebuilds it.

### Why deltas before fetching old state

If a remove request arrives when the memtable is empty (e.g. just after compaction), the WAL record is written first. On replay, the engine fetches the current list from the oldest tablet containing the key, applies the remove, and writes the result to the new tablet. This keeps the WAL durable even when the remove depends on on-disk state.

## 4. Memtable

`BTreeMap<(node_id, kind_id), Cell>` keyed by the 12-byte key.

- **Cell enum** encodes cardinality: `One`, `ZeroOrOne`, `Many`. Not always a `Vec`.
- **Apply deltas:** `Add(item)` appends to `Many`, inserts into `One`/`ZeroOrOne`; `Remove(item)` removes from `Many`, clears `One`/`ZeroOrOne`.
- **Flush trigger:** memtable byte budget (e.g. 4 MiB).
- On flush, cells are serialized into tablet rows: `Many` emits one row per item; `One`/`ZeroOrOne` emit one row (or zero for `None`).
- No in-memory dedup by key — the map key is unique by construction; delta application handles multiplicity.

## 5. Tablet (immutable run)

Immutable run file. Rows are **columnar**, partitioned by `kind_id` into chunks. Within a chunk, rows are sorted by `node_id`, then `target_id`. Multiple rows may share the same `(node_id, kind_id)`.

### Rationale

- A query for `kind_id = follows` only touches the `follows` chunk.
- Homogeneous values in a chunk compress better with type-specific codecs.
- `node_id`s sorted within a chunk → delta encoding + bit packing is effective.
- Decompression unit is small (~32–64 KB compressed per chunk), so point reads don't pay for the whole tablet.

### Tablet header

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | magic `b"GRTB"` |
| 4 | 4 | version `u32 LE` (current: 1, blob values) |
| 8 | 8 | `entry_count` u64 LE — total rows across all chunks |
| 16 | 4 | `chunk_count` u32 LE — distinct `kind_id`s |
| 20 | 8 | `min_node` u64 LE |
| 28 | 8 | `max_node` u64 LE |
| 32 | 4 | `flags` u32 LE (reserved, = 0) |

### Bloom filter section

Immediately after the header. Serialized bloom filter over `(node_id, kind_id)` 12-byte keys. One filter per tablet.

### Exact key/length directory

Immediately after the bloom filter:

| Field | Encoding |
|-------|----------|
| `key_count` | u64 LE |
| `node_id` | codec column of deltas from the previous entry |
| `kind_id` | codec column |
| `row_count` | codec column |

Each codec column is length-prefixed and self-describing (same format as chunk columns). Entries are in ascending `(node_id, kind_id)` order. A row count of `0` means the key is present and empty; absence means the tablet does not contain the key. `sum(row_count)` must equal the header `entry_count`.

### Chunk directory

One entry per chunk (per distinct `kind_id`):

| Field | Size |
|-------|------|
| `kind_id` u32 LE | 4 |
| `row_count` u32 LE | 4 |
| `data_offset` u64 LE | 8 |
| `data_length` u64 LE | 8 |

### Chunk data

Columnar/blob layout within a chunk (tablet v1):

| Section | Encoding |
|---------|----------|
| `node_id` | length-prefixed codec column of deltas from previous row's node id |
| `values` | length-prefixed blob column: `row_count` u64, then each row as a length-prefixed raw byte blob |

Rows are ordered by `(node_id, value bytes)`. Adjacency stores 16-byte `[target_id][edge_id]` blobs; properties store typed variable-length blobs. `kind_id` is constant within a chunk (stored in the directory).

### Footer

| Offset | Size | Field |
|--------|------|-------|
| -16 | 8 | `header_offset` u64 LE — back-pointer to header start |
| -8 | 4 | magic `b"GRTB"` |
| -4 | 4 | version `u32 LE` |

### Read semantics

- Check memtable first.
- Then L0 tablets newest-first, then L1, L2, etc.
- For a given `(node_id, kind_id)`: **the newest tablet that contains the key fully determines the result.** Older tablet rows for the same key are not read and not unioned. Replace semantics, not merge semantics.
- If bloom says "absent" → return empty/None immediately.
- If bloom says "present" → confirm against the exact key/length directory. A miss is a bloom false positive and returns empty/None. A length of `0` returns a present-but-empty result. Otherwise read exactly `length` rows from the chunk.

## 6. Read path

Check memtable first. Then L0 tablets newest-first, then L1, L2, etc.
For a given `(node_id, kind_id)`: **the newest tablet that contains the key fully determines the result.** Older tablet rows for the same key are not read and not unioned. Replace semantics, not merge semantics.
- If bloom says "absent" → return empty/None immediately.
- If bloom says "present" → confirm against the exact key/length directory. A miss is a bloom false positive and returns empty/None. A length of `0` returns a present-but-empty result. Otherwise read exactly `length` rows from the chunk.

## 7. Write path

1. Validate key `(node_id, kind_id)`.
2. Append WAL delta record(s) `(seq, Add|Remove, key, [target_id, edge_id])`; `fsync`.
3. Apply delta to memtable cell.
4. Acknowledge to caller.
5. On flush trigger: freeze memtable, allocate new one.
   - Group cells by `kind_id`, sort by `node_id` within each group.
   - Compress columns (delta + codec).
   - Write tablet sections: header, bloom, chunk directory, chunk data, footer.
   - `fsync`, rename `.tmp → .tablet`.
   - Update manifest.
   - Truncate WAL up to flushed `seq`.

## 8. Levels & manifest

- **L0:** memtable flushes; files may overlap key ranges; checked newest-first.
- **L1, L2, …:** each ~10× previous size; files within a level have non-overlapping key ranges.
- **Manifest:** append-only text log replayed on open. Two line forms:
  - `add <level> <filename> <max_seq>` — `max_seq` is the highest WAL sequence contained in the tablet and doubles as the flush commit point. A trailing line without a newline is ignored as a crash artifact; older three-field `add` lines parse with `max_seq = 0`.
  - `remove <level> <filename>` — used once compaction lands.

## 9. Compaction (future, after MVP)

1. Pick overlapping tablets from adjacent levels.
2. Iterate chunks by `kind_id`. For each kind, merge sorted `node_id` streams, keeping the row from the newer tablet on key collision.
3. Re-compress into fresh columnar chunks for the output tablet(s).
4. Replace old tablets, delete old inputs, update manifest.

Natural garbage collection: if Alice's follows list changes, the old tablet simply isn't copied; old rows disappear from the new tablet.

## 10. Incoming edges (future)

With key `(node_id, kind_id)`, outgoing expansion is prefix-efficient. Incoming expansion (`target == X`) is not. A second separate tablet/LSM keyed by `(target_id, kind_id, source_id)` can be added later, populated **only** by the trigger/derived-value system — never directly mutable by the user. For MVP, incoming queries can be a full scan or deferred.

## 11. What is intentionally dropped

| Item | Reason |
|------|--------|
| Tombstones | Every `(node_id, kind_id)` slot exists by schema and holds `None`/empty when absent |
| Prefix compression | Fixed 12-byte keys; no meaningful prefix sharing |
| Per-record magic / file offsets in WAL | Records are self-delimiting |
| `seq` inside tablets | Tablets collapse to latest row per key; `seq` lives in WAL + memtable only |
| Value handles / indirection | Columnar position is the pointer; no separate handle table needed |

## 12. Resolved decisions (MVP)

1. **Value width:** raw blob values (tablet v1). Adjacency uses 16-byte `(target_id, edge_id)`; properties use typed variable-length encodings in the same `(node_id, kind_id)`/`Cell` list storage.
2. **Chunk size target:** not yet chunked by byte size; each `kind_id` is one chunk for now. Byte-sized chunk splitting is a later optimization.
3. **Random access within a chunk:** decode the whole per-kind chunk on access; chunks are small in the MVP.
4. **Bloom filter key:** the full 12-byte `(node_id, kind_id)` key.
5. **Manifest format:** text, with `max_seq` on `add` lines as the flush commit point.
6. **Length property:** provided by the tablet-local exact key/length directory (authoritative, includes empty keys). Schema/catalog layers may add metadata without changing LSMT semantics.

## 13. Files

| File | Purpose |
|------|---------|
| `grahh-lsm/src/lib.rs` | crate root, public API |
| `grahh-lsm/src/persist.rs` | wire-format primitives (`Reader`/`Writer`, header, `PersistenceError`) |
| `grahh-lsm/src/error.rs` | `LsmError` |
| `grahh-lsm/src/key.rs` | fixed-width `Key`; adjacency `Value` codec |
| `grahh-lsm/src/wal.rs` | WAL append + replay (raw value blobs) |
| `grahh-lsm/src/bloom.rs` | bloom filter |
| `grahh-lsm/src/memtable.rs` | BTreeMap memtable with `Cell` enum |
| `grahh-lsm/src/tablet.rs` | tablet writer/reader (v1 blob values) |
| `grahh-lsm/src/manifest.rs` | manifest append-only log |
| `grahh-lsm/src/levels.rs` | level management + compaction bookkeeping |
| `grahh-lsm/src/engine.rs` | top-level engine (adjacency + raw/property APIs) |
| `libgrahh/src/adjacency.rs` | adjacency query structures / LSMT-backed usage |
| `libgrahh/src/properties.rs` | typed properties on LSMT list/cell storage |
| `libgrahh/src/database.rs` | embedded connection-style database API |
