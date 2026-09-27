//! Chunked Compressed Sparse Row (CSR) adjacency store — **one per edge kind**.
//!
//! This is the heart of "index-free adjacency": a node's neighbors are found
//! by one array-index hop into a flat offset array, then reading a slice of a
//! flat neighbor-id column — no index *seek* between friends.
//!
//! The store is **scoped to a single edge kind**, so each entry is a bare
//! target id (not `(target, kind, direction)`); the kind is a property of the
//! store and `direction` is a query-layer concern. This keeps the neighbor
//! column a clean, compressible `NodeId` column.
//!
//! Internally the flat neighbor column is split into fixed-size **chunks**,
//! each compressed with a codec from [`grahh_codecs`]. Reading a node's
//! neighbors decodes only the chunk(s) covering that node's slice.
//!
//! The store is **immutable**: fields are stored in [`Box<[_]>`] (no spare
//! capacity). Construction happens in a [`CsrBuilder`], which consumes its
//! scratch `Vec`s when converting to the boxed form via [`CsrBuilder::build`].
//!
//! ```text
//! offsets[i]    : start of node i's slice in the flat neighbor column
//! node i's ids  : neighbors[ offsets[i] .. offsets[i+1] ]
//! ```
//! Equal consecutive offsets mean a node has no neighbors — no special case.

use std::{collections::BTreeMap, range::Range};

use grahh_codecs::{Codec, CoreCodec};

use crate::persist::{PersistenceError, Reader, Writer};

/// A dense internal node id: an index into the node store, also used to index
/// into this CSR's offset array. Dense (no gaps) by renumbering, so the offset
/// array stays compact and directly indexable.
pub type NodeId = u64;

/// Upper bound on the chunk geometry a region may declare, so a corrupt count
/// cannot drive an absurd allocation.
const MAX_CHUNK_SLOTS: usize = 1 << 16;

/// One fixed-size slice of the neighbor column, compressed.
///
/// `len` is how many target ids this chunk holds; `packed` is its codec
/// encoded bytes. Because every chunk but the last is full, the *position* of
/// a chunk is derived from its index; only length and bytes are stored.
#[derive(Debug)]
struct Chunk {
    /// Number of target ids stored in this chunk.
    len: usize,
    /// Codec-encoded bytes for exactly these `len` ids.
    packed: Box<[u8]>,
}

/// Build a [`ChunkedCsr`] from raw, directed edges.
///
/// Accumulate edges with `add_edge`/`extend_edges`, then `build` to sort,
/// chunk, and compress into the immutable store. This is deliberately the
/// same shape a future compaction pass would use, so this builder doubles as
/// the prototype for the write/compaction half of the engine.
#[derive(Debug)]
pub struct CsrBuilder {
    edges: Vec<(NodeId, NodeId)>,
    /// Flat positions per chunk for stores this builder produces.
    chunk_slots: usize,
}

impl Default for CsrBuilder {
    fn default() -> Self {
        Self {
            edges: Vec::new(),
            chunk_slots: 64,
        }
    }
}

impl CsrBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a builder that already owns a set of `(source, target)` edges.
    ///
    /// This takes ownership with **no copying** — useful when a consumer
    /// already has the edge list (e.g. during a grouped build) and wants to
    /// hand it to the CSR without an extra allocation.
    pub fn from_edges(edges: Vec<(NodeId, NodeId)>) -> Self {
        Self {
            edges,
            ..Self::default()
        }
    }

    /// Record a directed edge `source -> target` (internal ids).
    pub fn add_edge(&mut self, source: NodeId, target: NodeId) {
        self.edges.push((source, target));
    }

    /// Record many directed edges at once.
    pub fn extend_edges<I: IntoIterator<Item = (NodeId, NodeId)>>(&mut self, iter: I) {
        self.edges.extend(iter);
    }

    /// Set how many flat positions one chunk spans (64 by default).
    ///
    /// Panics on zero: a zero-width chunk could never advance.
    pub fn set_chunk_slots(&mut self, chunk_slots: usize) {
        assert!(chunk_slots > 0, "chunk geometry must be at least 1");
        self.chunk_slots = chunk_slots;
    }

    /// Consume the builder, producing the immutable, chunked, compressed CSR.
    ///
    /// Neighbors are grouped by source and sorted by target id: this makes the
    /// offset array dense and compact, makes the neighbor column easy to
    /// compress (and, later, binary-searchable), and gives deterministic
    /// output. Equal `(source, target)` pairs keep their relative order
    /// (stable sort).
    pub fn build(self) -> ChunkedCsr {
        let chunk_slots: usize = self.chunk_slots;
        let mut edges: Vec<(u64, u64)> = self.edges;
        edges.sort_by_key(|&(s, t)| (s, t));

        // Upper bound on source ids referenced. Valid source ids are 0..=max.
        let node_count = match edges.iter().map(|&(s, _)| s).max() {
            Some(max_source) => max_source as usize + 1,
            None => 0,
        };

        // Count neighbors per source, then build exclusive-prefix offsets.
        let mut counts: Vec<usize> = vec![0; node_count];
        for &(s, _) in &edges {
            counts[s as usize] += 1;
        }
        let mut offsets: Box<[usize]> = (0..=node_count).map(|_| 0).collect();
        for i in 0..node_count {
            offsets[i + 1] = offsets[i] + counts[i];
        }

        // The flat neighbor column. Edges are sorted by (source, target), so
        // pushing target ids in sorted order groups them correctly by source.
        let neighbors: Vec<NodeId> = edges.into_iter().map(|(_, t)| t).collect();

        ChunkedCsr::from_parts(chunk_slots, offsets, neighbors)
    }
}

/// An immutable, chunked CSR neighbor store scoped to a single edge kind.
///
/// `offsets[i] .. offsets[i+1]` indexes the flat compressed neighbor column
/// and yields node `i`'s outgoing neighbors for this edge kind. Fields are
/// boxed slices because the store is immutable — there is never spare capacity.
#[derive(Debug)]
pub struct ChunkedCsr {
    /// Flat positions one chunk spans: the builder's policy when built here,
    /// whatever the region declared when loaded.
    chunk_slots: usize,
    /// `offsets[i]` = start of node `i`'s slice in the flat neighbor column.
    /// Length is `node_count + 1`. Equal consecutive offsets mean a node has
    /// no neighbors (handled with no special case).
    offsets: Box<[usize]>,
    /// The flat neighbor column, split into fixed-size compressed chunks.
    /// `neighbors[ci]` covers flat positions
    /// `[ ci*chunk_slots .. ci*chunk_slots + neighbors[ci].len )`.
    neighbors: Box<[Chunk]>,
}

impl ChunkedCsr {
    /// Build a store directly from a precomputed offset array and a flat,
    /// already-sorted neighbor column. Internal; used by `CsrBuilder::build`.
    fn from_parts(chunk_slots: usize, offsets: Box<[usize]>, neighbors: Vec<NodeId>) -> Self {
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut pos: usize = 0;
        while pos < neighbors.len() {
            let end: usize = (pos + chunk_slots).min(neighbors.len());
            let slice: &[u64] = &neighbors[pos..end];

            let mut packed: Vec<u8> = Vec::new();
            CoreCodec.encode(slice, &mut packed).unwrap();

            chunks.push(Chunk {
                len: slice.len(),
                packed: packed.into_boxed_slice(),
            });
            pos = end;
        }

        Self {
            chunk_slots,
            offsets,
            neighbors: chunks.into_boxed_slice(),
        }
    }

    /// Flat positions one chunk spans in this store.
    pub fn chunk_slots(&self) -> usize {
        self.chunk_slots
    }

    /// Number of distinct node ids indexed by this store.
    pub fn node_count(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Total number of stored neighbor entries (edges) in this store.
    pub fn edge_count(&self) -> usize {
        self.offsets.last().copied().unwrap_or_default()
    }

    /// Total encoded size of this store in bytes: the offsets array plus every
    /// compressed chunk payload (each chunk includes its 1-byte format tag).
    pub fn encoded_size(&self) -> usize {
        let offsets_bytes = self.offsets.len() * size_of::<usize>();
        let chunks_bytes: usize = self.neighbors.iter().map(|c| c.packed.len()).sum();
        offsets_bytes + chunks_bytes
    }

    /// Per-format usage: how many chunks were compressed by each format and
    /// how many bytes they occupy. Reads the tag byte from each chunk.
    pub fn format_usage(&self) -> Vec<(&'static str, usize, usize)> {
        let mut usage: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
        for c in self.neighbors.iter() {
            let name = grahh_codecs::format_name(&c.packed).unwrap_or("?");
            let e = usage.entry(name).or_insert((0, 0));
            e.0 += 1;
            e.1 += c.packed.len();
        }
        usage.into_iter().map(|(k, (n, b))| (k, n, b)).collect()
    }

    /// The slice range `[start, end)` of `node`'s neighbors in the flat
    /// column, or `None` if `node` is not indexed by this store.
    fn offset_range(&self, node: NodeId) -> Option<Range<usize>> {
        let idx = node as usize;
        if idx >= self.node_count() {
            return None;
        }
        Some(Range {
            start: self.offsets[idx],
            end: self.offsets[idx + 1],
        })
    }

    /// Number of outgoing neighbors of `node` for this edge kind (`O(1)`, no
    /// decode).
    pub fn degree(&self, node: NodeId) -> usize {
        self.offset_range(node)
            .map_or(0, |r: Range<usize>| r.end - r.start)
    }

    /// Outgoing neighbors of `node` for this edge kind, eager.
    ///
    /// Decodes only the chunk(s) covering the node's slice, appending into a
    /// single pre-sized buffer (no per-chunk temporary `Vec`s). Returns an
    /// empty vec for a node with no neighbors or an out-of-range id.
    pub fn neighbors(&self, node: NodeId) -> Vec<NodeId> {
        let Some(range) = self.offset_range(node) else {
            return Vec::new();
        };
        if range.is_empty() {
            return Vec::new();
        }

        let first_chunk: usize = range.start / self.chunk_slots;
        let last_chunk: usize = (range.end - 1) / self.chunk_slots;

        let mut out: Vec<NodeId> = Vec::with_capacity(range.end - range.start);
        // Reuse one decode buffer across chunks to avoid reallocating.
        let mut decoded: Vec<NodeId> = Vec::with_capacity(self.chunk_slots);

        for ci in first_chunk..=last_chunk {
            let chunk: &Chunk = &self.neighbors[ci];
            let cstart: usize = ci * self.chunk_slots;
            let cend: usize = cstart + chunk.len;
            let lo: usize = range.start.max(cstart);
            let hi: usize = range.end.min(cend);

            decoded.clear();
            CoreCodec
                .decode(&chunk.packed, &mut decoded)
                .expect("chunk payloads are validated at load");

            out.extend_from_slice(&decoded[lo - cstart..hi - cstart]);
        }
        out
    }

    /// Serialize this store into the portable adjacency byte format, appending
    /// to `out` (existing contents are kept, so regions can be nested).
    ///
    /// Layout — header, then little-endian `u64`: chunk geometry, node count,
    /// edge count, `node_count + 1` offsets, chunk count, then per chunk its id
    /// count and a length-prefixed region of **verbatim** codec output (never
    /// recompressed).
    ///
    /// # Errors
    /// Never fails for a store built by [`CsrBuilder`]; the `Result` mirrors
    /// [`ChunkedCsr::deserialize`] so nesting code has a single error type.
    pub fn serialize(&self, out: &mut Vec<u8>) -> Result<(), PersistenceError> {
        let mut w: Writer<'_> = Writer::new(out);
        w.header();
        w.u64(self.chunk_slots as u64);
        w.u64(self.node_count() as u64);
        w.u64(self.edge_count() as u64);
        for &offset in self.offsets.iter() {
            w.u64(offset as u64);
        }
        w.u64(self.neighbors.len() as u64);
        for chunk in self.neighbors.iter() {
            w.u64(chunk.len as u64);
            w.len_prefixed(&chunk.packed);
        }
        Ok(())
    }

    /// Rebuild a store written by [`ChunkedCsr::serialize`].
    ///
    /// This is the store's connect-time check: truncation, magic, version,
    /// geometry, offsets, counts, every chunk payload and trailing bytes are all
    /// validated here, so whatever comes back is one every accessor can trust.
    /// After it returns `Ok`, decoding is infallible by construction — there is
    /// deliberately no unchecked variant of this function.
    ///
    /// # Errors
    /// [`PersistenceError::UnexpectedEof`] if the buffer is short,
    /// [`PersistenceError::BadMagic`] or
    /// [`PersistenceError::UnsupportedVersion`] if it is not this format,
    /// [`PersistenceError::Corrupt`] if counts, offsets or geometry disagree,
    /// [`PersistenceError::Chunk`], [`PersistenceError::ChunkLenMismatch`] or
    /// [`PersistenceError::ChunkUnsorted`] if a payload is bad, and
    /// [`PersistenceError::TrailingBytes`] if extra bytes follow the region.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, PersistenceError> {
        let mut r: Reader<'_> = Reader::new(bytes);
        r.header()?;

        let chunk_slots: usize = r.len_u64()?;
        if chunk_slots == 0 || chunk_slots > MAX_CHUNK_SLOTS {
            return Err(PersistenceError::Corrupt {
                reason: "chunk geometry outside 1..=MAX_CHUNK_SLOTS",
            });
        }

        let node_count: usize = r.len_u64()?;
        let edge_count: usize = r.len_u64()?;

        // Guard against a bogus count becoming a huge allocation: the offset
        // array needs `node_count + 1` eight-byte entries.
        if node_count > r.remaining() / 8 {
            return Err(PersistenceError::Corrupt {
                reason: "node count exceeds the bytes remaining for offsets",
            });
        }

        let mut offsets: Vec<usize> = Vec::with_capacity(node_count + 1);
        for _ in 0..=node_count {
            offsets.push(r.len_u64()?);
        }
        Self::validate_offsets(&offsets, edge_count)?;

        let chunk_count: usize = r.len_u64()?;
        if chunk_count != edge_count.div_ceil(chunk_slots) {
            return Err(PersistenceError::Corrupt {
                reason: "chunk count does not match the edge count and geometry",
            });
        }

        // Payloads are checked here, not at query time: once this returns `Ok`,
        // every chunk decodes to exactly `len` ids in non-decreasing order, so
        // `neighbors` has no failure path to handle.
        let mut chunks: Vec<Chunk> = Vec::with_capacity(chunk_count);
        let mut scratch: Vec<NodeId> = Vec::with_capacity(chunk_slots);
        let mut total: usize = 0;
        let mut node: usize = 0;
        let mut previous: Option<NodeId> = None;
        for index in 0..chunk_count {
            let len: usize = r.len_u64()?;
            if len == 0 || len > chunk_slots {
                return Err(PersistenceError::Corrupt {
                    reason: "chunk length outside 1..=geometry",
                });
            }
            if len < chunk_slots && index + 1 != chunk_count {
                return Err(PersistenceError::Corrupt {
                    reason: "only the final chunk may be shorter than the geometry",
                });
            }
            let packed: &[u8] = r.len_prefixed()?;

            scratch.clear();
            CoreCodec
                .decode(packed, &mut scratch)
                .map_err(|source| PersistenceError::Chunk { index, source })?;
            if scratch.len() != len {
                return Err(PersistenceError::ChunkLenMismatch {
                    index,
                    declared: len,
                    decoded: scratch.len(),
                });
            }
            // Targets are sorted within each node's slice; the flat column is
            // grouped by node, so walk the node boundaries and compare only
            // values belonging to the same node.
            let chunk_start: usize = index * chunk_slots;
            let mut ordered: bool = true;
            for (at, &value) in scratch.iter().enumerate() {
                let position: usize = chunk_start + at;
                while node + 1 < offsets.len() && position >= offsets[node + 1] {
                    node += 1;
                    previous = None;
                }
                if previous.is_some_and(|prev| value < prev) {
                    ordered = false;
                    break;
                }
                previous = Some(value);
            }
            if !ordered {
                return Err(PersistenceError::ChunkUnsorted { index });
            }

            total += len;
            chunks.push(Chunk {
                len,
                packed: packed.to_vec().into_boxed_slice(),
            });
        }
        if total != edge_count {
            return Err(PersistenceError::Corrupt {
                reason: "chunk lengths do not sum to the edge count",
            });
        }

        r.finish()?;

        Ok(Self {
            chunk_slots,
            offsets: offsets.into_boxed_slice(),
            neighbors: chunks.into_boxed_slice(),
        })
    }

    /// Check the CSR offset invariants every accessor relies on.
    ///
    /// `offset_range` trusts `offsets[i+1] - offsets[i]` without bounds checks,
    /// so a decoded store must never be handed out with a broken offset array.
    fn validate_offsets(offsets: &[usize], edge_count: usize) -> Result<(), PersistenceError> {
        if offsets.first().copied() != Some(0) {
            return Err(PersistenceError::Corrupt {
                reason: "offset array must start at 0",
            });
        }
        if offsets.last().copied() != Some(edge_count) {
            return Err(PersistenceError::Corrupt {
                reason: "last offset must equal the edge count",
            });
        }
        if offsets.windows(2).any(|pair| pair[1] < pair[0]) {
            return Err(PersistenceError::Corrupt {
                reason: "offsets must be non-decreasing",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_from(edges: &[(NodeId, NodeId)]) -> ChunkedCsr {
        let mut b = CsrBuilder::new();
        b.extend_edges(edges.iter().copied());
        b.build()
    }

    #[test]
    fn basic_adjacency() {
        // 4 nodes; 0->{1,2}, 1->{2}, 3->{0}
        let csr = build_from(&[(0, 1), (0, 2), (1, 2), (3, 0)]);
        assert_eq!(csr.node_count(), 4);
        assert_eq!(csr.edge_count(), 4);
        assert_eq!(csr.degree(0), 2);
        assert_eq!(csr.neighbors(0), vec![1, 2]);
        assert_eq!(csr.neighbors(1), vec![2]);
        // node 2 has no outgoing neighbors
        assert_eq!(csr.degree(2), 0);
        assert_eq!(csr.neighbors(2), vec![]);
        assert_eq!(csr.neighbors(3), vec![0]);
    }

    #[test]
    fn out_of_range_is_empty() {
        let csr = build_from(&[]);
        assert_eq!(csr.node_count(), 0);
        assert_eq!(csr.edge_count(), 0);
        assert_eq!(csr.degree(0), 0);
        assert_eq!(csr.neighbors(0), vec![]);
        assert_eq!(csr.neighbors(999), vec![]);
    }

    #[test]
    fn chunk_boundaries_are_seamless() {
        // Small chunks so one node's slice spans several of them.
        let mut b = CsrBuilder::new();
        b.set_chunk_slots(4);
        for i in 1..=14_u64 {
            b.add_edge(0, i);
        }
        let csr = b.build();

        assert_eq!(csr.chunk_slots(), 4);
        assert!(csr.edge_count() > 4);
        let got = csr.neighbors(0);
        assert_eq!(got.len(), 14);
        assert_eq!(got.first().copied(), Some(1));
        assert_eq!(got.last().copied(), Some(14));
    }

    #[test]
    fn targets_are_sorted() {
        // unsorted input -> sorted, dedup-free output
        let csr = build_from(&[(0, 3), (0, 1), (0, 2)]);
        assert_eq!(csr.neighbors(0), vec![1, 2, 3]);
    }

    #[test]
    fn many_nodes_each_with_a_few_neighbors() {
        let mut edges = Vec::new();
        for s in 0..50_u64 {
            for t in 0..3_u64 {
                edges.push((s, t + s + 1));
            }
        }
        let csr = build_from(&edges);
        assert_eq!(csr.node_count(), 50);
        assert_eq!(csr.edge_count(), 150);
        assert_eq!(csr.neighbors(0), vec![1, 2, 3]);
        assert_eq!(csr.degree(0), 3);
        assert_eq!(csr.neighbors(49), vec![50, 51, 52]);
    }

    /// A `Vec<u8>` sink for `serialize`; keeps the tests below terse.
    fn to_bytes(csr: &ChunkedCsr) -> Vec<u8> {
        let mut bytes: Vec<u8> = Vec::new();
        csr.serialize(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn round_trips_through_bytes() {
        let csr = build_from(&[(0, 1), (0, 2), (1, 2), (3, 0)]);
        let bytes: Vec<u8> = to_bytes(&csr);
        let back: ChunkedCsr = ChunkedCsr::deserialize(&bytes).unwrap();

        assert_eq!(back.node_count(), csr.node_count());
        assert_eq!(back.edge_count(), csr.edge_count());
        for node in 0..csr.node_count() as NodeId {
            assert_eq!(back.neighbors(node), csr.neighbors(node), "node {node}");
            assert_eq!(back.degree(node), csr.degree(node), "node {node}");
        }
    }

    #[test]
    fn round_trip_is_byte_for_byte_stable() {
        // Re-serializing a decoded store must reproduce the exact same bytes,
        // because chunk payloads are carried verbatim and never recompressed.
        let mut b = CsrBuilder::new();
        b.set_chunk_slots(8);
        for source in 0..40_u64 {
            for step in 1..=7_u64 {
                b.add_edge(source, source * 10 + step);
            }
        }
        let csr = b.build();
        let first: Vec<u8> = to_bytes(&csr);

        let decoded: ChunkedCsr = ChunkedCsr::deserialize(&first).unwrap();
        let second: Vec<u8> = to_bytes(&decoded);

        assert_eq!(first, second);
        assert_eq!(decoded.edge_count(), 280);
        // Node 9's slice is flat positions 63..70, which straddles a chunk
        // boundary at this geometry (8 per chunk).
        assert_eq!(decoded.chunk_slots(), 8);
        assert_eq!(decoded.neighbors(9), csr.neighbors(9));
        assert_eq!(decoded.neighbors(9).len(), 7);
    }

    #[test]
    fn empty_store_round_trips() {
        let csr = build_from(&[]);
        let back: ChunkedCsr = ChunkedCsr::deserialize(&to_bytes(&csr)).unwrap();
        assert_eq!(back.node_count(), 0);
        assert_eq!(back.edge_count(), 0);
        assert_eq!(back.neighbors(0), Vec::<NodeId>::new());
    }

    #[test]
    fn deserialize_rejects_malformed_bytes() {
        let csr = build_from(&[(0, 1), (0, 2)]);
        let bytes: Vec<u8> = to_bytes(&csr);

        let mut bad_magic: Vec<u8> = bytes.clone();
        bad_magic[0] = b'X';
        assert!(matches!(
            ChunkedCsr::deserialize(&bad_magic),
            Err(PersistenceError::BadMagic { .. })
        ));

        let mut bad_version: Vec<u8> = bytes.clone();
        bad_version[4] = 0xFF;
        assert!(matches!(
            ChunkedCsr::deserialize(&bad_version),
            Err(PersistenceError::UnsupportedVersion { .. })
        ));

        let truncated: &[u8] = &bytes[..bytes.len() - 1];
        assert!(matches!(
            ChunkedCsr::deserialize(truncated),
            Err(PersistenceError::UnexpectedEof { .. })
        ));

        let mut extra: Vec<u8> = bytes.clone();
        extra.push(0);
        assert!(matches!(
            ChunkedCsr::deserialize(&extra),
            Err(PersistenceError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn deserialize_rejects_inconsistent_structure() {
        // Hand-built region: 1 node, 1 edge, offsets [0, 1] — but two chunks
        // where the edge count permits exactly one.
        let mut buf: Vec<u8> = Vec::new();
        let mut w: Writer<'_> = Writer::new(&mut buf);
        w.header();
        w.u64(64);
        w.u64(1);
        w.u64(1);
        w.u64(0);
        w.u64(1);
        w.u64(2);
        drop(w);

        assert!(matches!(
            ChunkedCsr::deserialize(&buf),
            Err(PersistenceError::Corrupt { .. })
        ));
    }

    /// Bytes of region framing before the offsets: header, geometry, node
    /// count, edge count, chunk count.
    const FRAMING: usize = 8 + 4 * 8;

    /// Where the first chunk's `len` field starts, for `node_count` nodes.
    fn chunk_len_offset(node_count: usize) -> usize {
        FRAMING + (node_count + 1) * 8
    }

    #[test]
    fn deserialize_rejects_corrupt_chunk_payload() {
        let mut b = CsrBuilder::new();
        b.set_chunk_slots(4);
        b.add_edge(0, 1);
        b.add_edge(0, 2);
        let mut bytes: Vec<u8> = to_bytes(&b.build());

        // The first payload byte is the codec tag; no registered tag survives
        // this flip, so the payload can no longer be decoded.
        let payload_at: usize = chunk_len_offset(1) + 2 * 8;
        bytes[payload_at] ^= 0xFF;

        assert!(matches!(
            ChunkedCsr::deserialize(&bytes),
            Err(PersistenceError::Chunk { index: 0, .. })
        ));
    }

    #[test]
    fn deserialize_rejects_len_mismatch() {
        let mut b = CsrBuilder::new();
        b.set_chunk_slots(4);
        b.add_edge(0, 1);
        b.add_edge(0, 2);
        let mut bytes: Vec<u8> = to_bytes(&b.build());

        let len_at: usize = chunk_len_offset(1);
        bytes[len_at..len_at + 8].copy_from_slice(&1_u64.to_le_bytes());

        assert!(matches!(
            ChunkedCsr::deserialize(&bytes),
            Err(PersistenceError::ChunkLenMismatch {
                index: 0,
                declared: 1,
                decoded: 2,
            })
        ));
    }

    #[test]
    fn deserialize_rejects_unsorted_chunk() {
        let mut packed: Vec<u8> = Vec::new();
        CoreCodec.encode(&[3, 1], &mut packed).unwrap();

        let mut buf: Vec<u8> = Vec::new();
        let mut w: Writer<'_> = Writer::new(&mut buf);
        w.header();
        w.u64(4);
        w.u64(1);
        w.u64(2);
        w.u64(0);
        w.u64(2);
        w.u64(1);
        w.u64(2);
        w.len_prefixed(&packed);
        drop(w);

        assert!(matches!(
            ChunkedCsr::deserialize(&buf),
            Err(PersistenceError::ChunkUnsorted { index: 0 })
        ));
    }

    #[test]
    fn deserialize_rejects_short_nonfinal_chunk() {
        // Geometry 4 over 6 edges needs two chunks, and the first must be full:
        // every position after a short chunk would be derived differently.
        let mut buf: Vec<u8> = Vec::new();
        let mut w: Writer<'_> = Writer::new(&mut buf);
        w.header();
        w.u64(4);
        w.u64(1);
        w.u64(6);
        w.u64(0);
        w.u64(6);
        w.u64(2);
        w.u64(2);
        drop(w);

        assert!(matches!(
            ChunkedCsr::deserialize(&buf),
            Err(PersistenceError::Corrupt { .. })
        ));
    }

    #[test]
    fn deserialize_honours_file_geometry() {
        // Geometry 2 is not the builder's default: the reader must use what the
        // region declares, not a compiled-in value.
        let mut b = CsrBuilder::new();
        b.set_chunk_slots(2);
        for i in 1..=5_u64 {
            b.add_edge(0, i);
        }
        let csr = b.build();
        let bytes: Vec<u8> = to_bytes(&csr);

        let back: ChunkedCsr = ChunkedCsr::deserialize(&bytes).unwrap();
        assert_eq!(back.chunk_slots(), 2);
        assert_eq!(back.edge_count(), 5);
        assert_eq!(back.neighbors(0), vec![1, 2, 3, 4, 5]);
    }
}
