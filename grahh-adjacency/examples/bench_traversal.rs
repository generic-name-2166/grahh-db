//! Traversal benchmark: chunked CSR vs a naive adjacency list.
//!
//! Validates the "index-free adjacency" premise: does a compressed, chunked
//! CSR give fast neighbor expansion and nested traversal versus the naive
//! in-RAM `Vec<Vec<NodeId>>` representation?
//!
//! Run: `cargo run --release --example bench_traversal`
//!
//! Deliberately dependency-free: timing is raw `std::time::Instant`, and the
//! naive and CSR stores are both built from the same synthetic graph. The CSR
//! compresses each chunk with the library's codec picker (best of
//! FOR/bitpack/RLE/plain), and we report the real encoded size + which format
//! won per chunk.

use std::time::{Duration, Instant};

use grahh_adjacency::{ChunkedCsr, CsrBuilder, NodeId};

/// Synthetic power-law-ish graph: `node_count` nodes, `edge_count` directed
/// edges. Lower node ids are denser (hub-like), so there's a mix of few
/// high-degree and many low-degree nodes, plus a wide sparse id space — a
/// plausible shape for a social/followers graph. Deterministic.
fn synth_graph(node_count: u64, edge_count: usize) -> Vec<(NodeId, NodeId)> {
    // Prefix-mixing PRNG: xorshift fixed seed, so the graph is reproducible.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let mut edges = Vec::with_capacity(edge_count);
    for _ in 0..edge_count {
        // Source biased toward low ids (hubs appear disproportionately often).
        let src = next() % node_count;
        // Target uniformly across the full id space, so targets skew high and
        // produce a sparse, wide distribution (good for compression deltas).
        let tgt = next() % node_count;
        // Self-loops are legal but skew the graph shape; skip them for a
        // cleaner comparison.
        if src != tgt {
            edges.push((src, tgt));
        }
    }
    edges
}

/// Naive baseline: one `Vec` of neighbor ids per node.
fn build_naive(edges: &[(NodeId, NodeId)]) -> Vec<Vec<NodeId>> {
    let mut adj = Vec::new();
    for &(src, tgt) in edges {
        let needed = src as usize + 1;
        if adj.len() <= needed {
            adj.resize(needed, Vec::new());
        }
        adj[src as usize].push(tgt);
    }
    adj
}

fn main() {
    // Tune: 100k nodes, 400k edges — fast enough to run repeatedly, big enough
    // to smooth out `Instant` noise. Bump if you want heavier graphs.
    let node_count: u64 = 100_000;
    let edge_count: usize = 400_000;

    let edges = synth_graph(node_count, edge_count);
    println!(
        "graph: {} nodes, {} edges not self-loop",
        node_count,
        edges.len()
    );

    // ---- Build both stores from the same edges ----
    let t0: Instant = Instant::now();
    let naive: Vec<Vec<u64>> = build_naive(&edges);
    let t_naive: Duration = t0.elapsed();

    let t1: Instant = Instant::now();
    let mut b: CsrBuilder = CsrBuilder::new();
    b.extend_edges(edges.iter().copied());
    let csr: ChunkedCsr = b.build();
    let t_csr: Duration = t1.elapsed();

    println!("build naive: {t_naive:?} | build csr: {t_csr:?}");

    // ---- Footprint comparison (measured) ----
    // naive: 8 bytes per neighbor id.
    let naive_bytes: usize = naive.iter().map(|v| v.len() * 8).sum::<usize>();
    // CSR: real encoded size from the store itself (offsets + compressed chunks).
    let csr_bytes: usize = csr.encoded_size();
    println!(
        "footprint naive   ~ {naive_bytes} B ~ {:.1} MiB",
        naive_bytes as f64 / (1024.0 * 1024.0)
    );
    println!(
        "footprint csr     ~ {csr_bytes} B ~ {:.1} MiB",
        csr_bytes as f64 / (1024.0 * 1024.0)
    );
    println!(
        "csr compression   ~ {:.2}x smaller than naive",
        naive_bytes as f64 / csr_bytes.max(1) as f64
    );
    println!("csr format usage : {:?}", csr.format_usage());

    // ---- Measurement helpers ----
    // Single-hop expansion from a node (sum neighbor ids so work is real).
    let mut sink: usize = 0;

    fn expand_sum(csr: &ChunkedCsr, node: NodeId) -> usize {
        csr.neighbors(node)
            .iter()
            .copied()
            .map(|n| n as usize)
            .sum()
    }
    fn expand_sum_naive(naive: &[Vec<NodeId>], node: NodeId) -> usize {
        naive
            .get(node as usize)
            .map_or_default(|v: &Vec<u64>| v.iter().copied().map(|n: u64| n as usize).sum())
    }

    // Friends-of-friends: 2-hop reachability from a node (excluding self).
    fn fof_csr(csr: &ChunkedCsr, node: NodeId) -> usize {
        let mut seen: usize = 0;
        for &a in &csr.neighbors(node) {
            for &b in &csr.neighbors(a) {
                if b != node {
                    seen += b as usize;
                }
            }
        }
        seen
    }
    fn fof_naive(naive: &[Vec<NodeId>], node: NodeId) -> usize {
        let mut seen: usize = 0;
        if let Some(v) = naive.get(node as usize) {
            for &a in v {
                if let Some(av) = naive.get(a as usize) {
                    for &b in av {
                        if b != node {
                            seen += b as usize;
                        }
                    }
                }
            }
        }
        seen
    }

    // ---- Measure single-hop: expand each of `reps` nodes once ----
    let reps: i32 = 2000;
    let hops = |src: u64, csr: &ChunkedCsr, naive: &[Vec<NodeId>]| {
        let mut acc: usize = 0;
        for i in 0..reps {
            let node = (src + i as u64) % node_count;
            acc += expand_sum(csr, node);
            acc += expand_sum_naive(naive, node);
        }
        acc
    };
    let t: Instant = Instant::now();
    let acc_csr: usize = hops(7, &csr, &naive);
    let t_h_csr: Duration = t.elapsed();
    let t: Instant = Instant::now();
    let acc_naive: usize = hops(7, &csr, &naive);
    let t_h_naive: Duration = t.elapsed();
    sink += acc_csr + acc_naive;

    // ---- Measure friends-of-friends ----
    let reps_fof: u64 = 300;
    let t: Instant = Instant::now();
    let mut s2: usize = 0;
    for r in 0..reps_fof {
        s2 += fof_csr(&csr, (r * 13) % node_count);
    }
    let t_fof_csr: Duration = t.elapsed();
    let t: Instant = Instant::now();
    let mut s2n: usize = 0;
    for r in 0..reps_fof {
        s2n += fof_naive(&naive, (r * 13) % node_count);
    }
    let t_fof_naive: Duration = t.elapsed();
    sink += s2 + s2n;

    println!("\n-- single-hop expand (avg over {reps} nodes) --");
    println!(
        "csr : {:.1} ns/op   | naive : {:.1} ns/op",
        t_h_csr.as_nanos() as f64 / reps as f64,
        t_h_naive.as_nanos() as f64 / reps as f64
    );
    println!("\n-- friends-of-friends 2-hop (avg over {reps_fof} roots) --");
    println!(
        "csr : {:.1} ns/op   | naive : {:.1} ns/op",
        t_fof_csr.as_nanos() as f64 / reps_fof as f64,
        t_fof_naive.as_nanos() as f64 / reps_fof as f64
    );
    println!("\n(sink={sink})");
}
