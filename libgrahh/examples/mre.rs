//! Minimum end-to-end use of the database API: build by key, persist, load, then
//! run the query the engine exists for.
//!
//! ```text
//! build:  edges are declared by user key; ids are assigned on first sight
//! image:  header + directory                                 (grahh-persist)
//!         region 1: label index      user key <-> dense id   (grahh-index)
//!         region 2: adjacency        topology per edge kind  (grahh-adjacency)
//! query:  key -> id  (lookup)
//!         id -> ids  (expand through one edge kind, one direction)
//!         id -> key  (key_of), or repeat for a nested traversal
//! ```
//!
//! Run: `cargo run --example mre -p libgrahh`

use libgrahh::{Database, DatabaseBuilder, Direction, EdgeKindId, NodeId};

/// `follows` is directional and we want "who follows me", so it declares an
/// inverse. `likes` does not.
const FOLLOWS: EdgeKindId = EdgeKindId(0);
const LIKES: EdgeKindId = EdgeKindId(1);

fn build() -> Database {
    let mut b = DatabaseBuilder::new();
    b.set_inverse(FOLLOWS);
    b.edge(FOLLOWS, 1, 2);
    b.edge(FOLLOWS, 1, 3);
    b.edge(FOLLOWS, 2, 3);
    b.edge(FOLLOWS, 4, 3);
    b.edge(LIKES, 3, 7);
    b.build()
}

/// Expand `node` and translate the result back into user keys.
fn expand_keys(db: &Database, node: NodeId, kind: EdgeKindId, dir: Direction) -> Vec<u64> {
    db.expand(node, kind, dir)
        .into_iter()
        .filter_map(|id| db.key_of(id))
        .collect()
}

fn main() {
    let built: Database = build();
    println!(
        "built : {} nodes, {} edges (forward + reverse for inverse kinds)",
        built.node_count(),
        built.edge_count()
    );

    // ---- Persist the whole database as one image ----
    let mut image: Vec<u8> = Vec::new();
    built.serialize(&mut image).unwrap();
    println!(
        "image : {} bytes for {} nodes",
        image.len(),
        built.node_count()
    );

    // ---- Load it back and query only the loaded copy ----
    let db: Database = Database::deserialize(&image).unwrap();
    println!(
        "loaded: {} nodes, {} edges\n",
        db.node_count(),
        db.edge_count()
    );

    let alice: NodeId = db.lookup(1).expect("key 1 is in the index");
    println!("lookup {:<3} -> node {alice}", 1);
    println!("lookup {:<3} -> {:?}", 999, db.lookup(999));
    println!("key_of {:<3} -> {:?}", alice, db.key_of(alice));

    let follows: Vec<u64> = expand_keys(&db, alice, FOLLOWS, Direction::Outgoing);
    println!("\nfollows         {:?}", follows);

    let node: NodeId = db.lookup(3).expect("key 3 is in the index");
    let followed_by: Vec<u64> = expand_keys(&db, node, FOLLOWS, Direction::Incoming);
    println!("followed by     {followed_by:?} (inverse was declared)");

    let liked_by: Vec<u64> = expand_keys(&db, node, LIKES, Direction::Incoming);
    println!("liked by        {liked_by:?} (no inverse declared -> nothing stored)");

    // ---- Two hops: the nested traversal, ids throughout ----
    let mut friends_of_friends: Vec<u64> = Vec::new();
    for key in &follows {
        let friend: NodeId = db.lookup(*key).expect("follow targets are known keys");
        friends_of_friends.extend(expand_keys(&db, friend, FOLLOWS, Direction::Outgoing));
    }
    friends_of_friends.sort_unstable();
    friends_of_friends.dedup();
    friends_of_friends.retain(|key| *key != 1);
    println!("\n2-hop from 1    {friends_of_friends:?}");
}
