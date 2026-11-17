//! Minimum end-to-end use of the embedded database API.
//!
//! ```text
//! open:   Database::open(path)  (SQLite/DuckDB-style connection)
//! write:  ensure nodes, declare edges/properties; durable via LSMT (WAL)
//! query:  key -> id (lookup), id -> ids (expand), id -> key (key_of)
//! reopen: data survives close/reopen
//! ```
//!
//! Run: `cargo run --example mre -p libgrahh`

use std::fs;

use libgrahh::{
    Database, Direction, EdgeKindId, NodeId, PropertyKindId, PropertyType, PropertyValue,
};

const FOLLOWS: EdgeKindId = EdgeKindId(0);
const LIKES: EdgeKindId = EdgeKindId(1);
const NAME: PropertyKindId = PropertyKindId(10_000);

fn expand_keys(db: &Database, node: NodeId, kind: EdgeKindId, dir: Direction) -> Vec<u64> {
    db.expand(node, kind, dir)
        .into_iter()
        .filter_map(|id| db.key_of(id))
        .collect()
}

fn main() {
    let dir = std::env::temp_dir().join(format!("grahh-mre-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);

    {
        let mut db = Database::open(&dir).unwrap();
        db.set_inverse(FOLLOWS).unwrap();
        db.add_edge(FOLLOWS, 1, 2).unwrap();
        db.add_edge(FOLLOWS, 1, 3).unwrap();
        db.add_edge(FOLLOWS, 2, 3).unwrap();
        db.add_edge(FOLLOWS, 4, 3).unwrap();
        db.add_edge(LIKES, 3, 7).unwrap();

        let alice = db.lookup(1).unwrap();
        db.set_property(
            alice,
            NAME,
            &PropertyValue::String("alice".into()),
            &PropertyType::String,
        )
        .unwrap();

        println!(
            "live: {} nodes, {} edge rows",
            db.node_count(),
            db.edge_count()
        );
        db.flush().unwrap();
    }

    let db = Database::open(&dir).unwrap();
    println!(
        "reopened: {} nodes, {} edge rows\n",
        db.node_count(),
        db.edge_count()
    );

    let alice: NodeId = db.lookup(1).expect("key 1 exists");
    println!("lookup {:<3} -> node {alice}", 1);
    println!(
        "name   {:<3} -> {:?}",
        1,
        db.get_property_one(alice, NAME, &PropertyType::String).unwrap()
    );

    println!("\nfollows         {:?}", expand_keys(&db, alice, FOLLOWS, Direction::Outgoing));

    let node3 = db.lookup(3).unwrap();
    println!(
        "followed by     {:?} (inverse declared)",
        expand_keys(&db, node3, FOLLOWS, Direction::Incoming)
    );
    println!(
        "liked by        {:?} (no inverse -> empty)",
        expand_keys(&db, node3, LIKES, Direction::Incoming)
    );

    let _ = fs::remove_dir_all(&dir);
}
