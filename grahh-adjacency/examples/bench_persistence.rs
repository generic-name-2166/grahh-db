//! Persistence benchmark: the LSM tablet engine vs the old CSR image.
//!
//! CSR is now the in-memory query structure; durability goes through
//! `grahh-lsm`. This example measures both paths on the same graph:
//!
//! 1. **CSR image (legacy baseline).** Serialize one whole `ChunkedCsr` to a
//!    `Vec<u8>` and deserialize it back. This was the database's persistence
//!    mechanism before the LSM migration and is kept for comparison.
//! 2. **LSM tablets (current path).** Append every edge through `Engine`
//!    (a durable WAL append per edge), flush to an L0 tablet, reopen the
//!    engine, scan the kind back, and rebuild the CSR.
//!
//! The LSM write path is deliberately per-edge durable, so it is much slower
//! than taking an in-memory image snapshot. That cost buys crash recovery and
//! replace semantics. The graph size is smaller than `bench_traversal` for
//! that reason; override it with `GRAHH_BENCH_NODES` / `GRAHH_BENCH_EDGES`.
//!
//! Run: `cargo run --release --example bench_persistence`

use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use grahh_adjacency::{ChunkedCsr, CsrBuilder, NodeId};
use grahh_lsm::{Engine, Key};

/// Edge kind used for the LSM rows.
const KIND: u32 = 0;

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

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

/// Total bytes and tablet-only bytes in a directory.
fn dir_usage(dir: &Path) -> (usize, usize) {
    let mut total = 0;
    let mut tablets = 0;
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let len = metadata.len() as usize;
        total += len;
        if entry.file_name().to_string_lossy().ends_with(".tablet") {
            tablets += len;
        }
    }
    (total, tablets)
}

fn expansion_sum(csr: &ChunkedCsr, node_count: u64) -> usize {
    (0..node_count).map(|node| csr.neighbors(node).len()).sum()
}

fn main() {
    let node_count = env_usize("GRAHH_BENCH_NODES", 10_000) as u64;
    let edge_count = env_usize("GRAHH_BENCH_EDGES", 40_000);

    let edges = synth_graph(node_count, edge_count);
    println!(
        "graph: {} nodes, {} edges not self-loop",
        node_count,
        edges.len()
    );

    // ---- The in-memory query structure ----
    let t = Instant::now();
    let csr: ChunkedCsr = CsrBuilder::from_edges(edges.clone()).build();
    let t_build: Duration = t.elapsed();

    // ---- Legacy baseline: one whole-CSR image ----
    let mut image: Vec<u8> = Vec::new();
    let t = Instant::now();
    csr.serialize(&mut image).unwrap();
    let t_serialize: Duration = t.elapsed();

    let t = Instant::now();
    let from_image: ChunkedCsr = ChunkedCsr::deserialize(&image).unwrap();
    let t_image_load: Duration = t.elapsed();

    // ---- Current path: WAL + LSM tablet ----
    let dir = std::env::temp_dir().join(format!("grahh-bench-lsm-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);

    let t = Instant::now();
    let mut engine = Engine::open(&dir).unwrap();
    let t_lsm_open: Duration = t.elapsed();

    let t = Instant::now();
    let mut next_edge = vec![0_u64; node_count as usize];
    for (source, target) in csr.edges() {
        let edge_id = next_edge[source as usize];
        next_edge[source as usize] += 1;
        engine.add(Key::new(source, KIND), target, edge_id).unwrap();
    }
    let t_lsm_write: Duration = t.elapsed();

    let t = Instant::now();
    engine.flush().unwrap();
    let t_lsm_flush: Duration = t.elapsed();
    drop(engine);

    let t = Instant::now();
    let engine = Engine::open(&dir).unwrap();
    let t_lsm_reopen: Duration = t.elapsed();

    let t = Instant::now();
    let rows = engine.scan_kind(KIND).unwrap();
    let mut builder = CsrBuilder::new();
    builder.extend_edges(
        rows.iter()
            .map(|(source, value)| (*source, value.target_id)),
    );
    let from_lsm: ChunkedCsr = builder.build();
    let t_lsm_load: Duration = t.elapsed();

    let (lsm_bytes, tablet_bytes) = dir_usage(&dir);
    let payload: usize = csr.encoded_size();
    let naive: usize = edges.len() * 8;

    println!("\n-- size --");
    println!("naive edge payload  {:6.2} MiB   (8 B/edge)", mib(naive));
    println!(
        "csr payload         {:6.2} MiB   (offsets + compressed chunks)",
        mib(payload)
    );
    println!(
        "csr image           {:6.2} MiB   ({:+.2}% vs payload)",
        mib(image.len()),
        (image.len() as f64 / payload as f64 - 1.0) * 100.0
    );
    println!(
        "lsm tablet          {:6.2} MiB   ({:+.2}% vs payload)",
        mib(tablet_bytes),
        (tablet_bytes as f64 / payload as f64 - 1.0) * 100.0
    );
    println!(
        "lsm directory       {:6.2} MiB   (tablet + manifest + WAL header)",
        mib(lsm_bytes)
    );

    println!("\n-- time --");
    println!("build csr     {t_build:?}   (sort + chunk + compress every chunk)");
    println!(
        "csr serialize {t_serialize:?}   ({:.0} MiB/s)",
        throughput(image.len(), t_serialize)
    );
    println!(
        "csr load      {t_image_load:?}   ({:.0} MiB/s, validates every chunk)",
        throughput(image.len(), t_image_load)
    );
    println!("lsm open      {t_lsm_open:?}   (empty database)");
    println!(
        "lsm write     {t_lsm_write:?}   ({:.1} us/edge, durable WAL append)",
        t_lsm_write.as_micros() as f64 / edges.len().max(1) as f64
    );
    println!("lsm flush     {t_lsm_flush:?}   (memtable -> tablet -> manifest)");
    println!("lsm reopen    {t_lsm_reopen:?}   (manifest replay + tablet load)");
    println!("lsm -> csr    {t_lsm_load:?}   (scan kind + rebuild compressed CSR)");
    println!(
        "image load is {:.1}x faster than lsm reopen + rebuild",
        (t_lsm_reopen + t_lsm_load).as_secs_f64() / t_image_load.as_secs_f64().max(1e-9)
    );

    // ---- Both persistence paths must reproduce the same topology ----
    assert_eq!(from_image.node_count(), csr.node_count());
    assert_eq!(from_image.edge_count(), csr.edge_count());
    assert_eq!(from_lsm.node_count(), csr.node_count());
    assert_eq!(from_lsm.edge_count(), csr.edge_count());

    let built_ids = expansion_sum(&csr, node_count);
    assert_eq!(expansion_sum(&from_image, node_count), built_ids);
    assert_eq!(expansion_sum(&from_lsm, node_count), built_ids);
    println!("\nall paths expand identically: {built_ids} neighbor ids across {node_count} nodes");

    let _ = fs::remove_dir_all(&dir);
}
