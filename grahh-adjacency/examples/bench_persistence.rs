//! Persistence benchmark: what the byte format costs, and what it buys.
//!
//! Checks three Phase-4 claims on the same graph `bench_traversal` uses:
//! persisting is a *copy* (chunk payloads carry through unchanged), the framing
//! overhead on top of the compressed payload is small, and a validating load is
//! cheaper than rebuilding the CSR from raw edges.
//!
//! Run: `cargo run --release --example bench_persistence`

use std::time::{Duration, Instant};

use grahh_adjacency::{ChunkedCsr, CsrBuilder, NodeId};

/// Same deterministic synthetic graph as `bench_traversal`, so the numbers line
/// up: source-biased toward low ids, targets uniform across the id space.
fn synth_graph(node_count: u64, edge_count: usize) -> Vec<(NodeId, NodeId)> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let mut edges = Vec::with_capacity(edge_count);
    for _ in 0..edge_count {
        let src = next() % node_count;
        let tgt = next() % node_count;
        if src != tgt {
            edges.push((src, tgt));
        }
    }
    edges
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn throughput(bytes: usize, elapsed: Duration) -> f64 {
    mib(bytes) / elapsed.as_secs_f64().max(1e-9)
}

fn main() {
    let node_count: u64 = 100_000;
    let edge_count: usize = 400_000;

    let edges = synth_graph(node_count, edge_count);
    println!(
        "graph: {} nodes, {} edges not self-loop",
        node_count,
        edges.len()
    );

    // ---- Build, serialize, load ----
    let t: Instant = Instant::now();
    let mut b: CsrBuilder = CsrBuilder::new();
    b.extend_edges(edges.iter().copied());
    let csr: ChunkedCsr = b.build();
    let t_build: Duration = t.elapsed();

    let mut bytes: Vec<u8> = Vec::new();
    let t: Instant = Instant::now();
    csr.serialize(&mut bytes).unwrap();
    let t_serialize: Duration = t.elapsed();

    let t: Instant = Instant::now();
    let loaded: ChunkedCsr = ChunkedCsr::deserialize(&bytes).unwrap();
    let t_load: Duration = t.elapsed();

    let image: usize = bytes.len();
    let payload: usize = csr.encoded_size();
    let naive: usize = edges.len() * 8;

    println!("\n-- size --");
    println!("naive edge payload  {:6.2} MiB   (8 B/edge)", mib(naive));
    println!(
        "csr payload         {:6.2} MiB   (offsets + compressed chunks)",
        mib(payload)
    );
    println!(
        "serialized image    {:6.2} MiB   ({:+.2}% vs payload)",
        mib(image),
        (image as f64 / payload as f64 - 1.0) * 100.0
    );
    println!(
        "framing overhead    {:6} bytes (header, counts, per-chunk directory)",
        image - payload
    );

    println!("\n-- time --");
    println!("build csr  {t_build:?}   (sort + chunk + compress every chunk)");
    println!(
        "serialize  {t_serialize:?}   ({:.0} MiB/s)",
        throughput(image, t_serialize)
    );
    println!(
        "load       {t_load:?}   ({:.0} MiB/s, validates every chunk payload)",
        throughput(image, t_load)
    );
    println!(
        "load is {:.2}x faster than rebuilding from edges",
        t_build.as_secs_f64() / t_load.as_secs_f64().max(1e-9)
    );

    // ---- Persisting is a copy, never a recompression ----
    assert_eq!(loaded.chunk_slots(), csr.chunk_slots());
    assert_eq!(loaded.format_usage(), csr.format_usage());
    println!(
        "\nformats survive a load unchanged: {:?}",
        loaded.format_usage()
    );

    // ---- A loaded store must expand identically, and decoding the whole
    // column is the pass validation already paid for ----
    fn expansion_sum(csr: &ChunkedCsr, node_count: u64) -> usize {
        (0..node_count).map(|n| csr.neighbors(n).len()).sum()
    }

    let t: Instant = Instant::now();
    let built_ids: usize = expansion_sum(&csr, node_count);
    let t_expand: Duration = t.elapsed();

    let t: Instant = Instant::now();
    let loaded_ids: usize = expansion_sum(&loaded, node_count);
    let t_expand_loaded: Duration = t.elapsed();

    assert_eq!(built_ids, loaded_ids);
    println!("\n-- expanding every node --");
    println!("built  {t_expand:?}");
    println!("loaded {t_expand_loaded:?}   (identical result: {loaded_ids} ids)");
}
