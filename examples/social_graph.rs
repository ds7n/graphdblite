//! Social graph example.
//!
//! Builds a small friendship network and runs a few traversal queries:
//!   - Direct friends of a user.
//!   - Friends-of-friends, excluding the user and their direct friends.
//!   - Shortest-path style reach via variable-length traversal.
//!
//! Run with: `cargo run --example social_graph`

use graphdblite::{Database, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join("graphdblite-social-graph.db");
    // Start clean each run.
    let _ = std::fs::remove_file(&path);
    let mut db = Database::open(&path)?;

    // Seed a tiny social graph.
    //   alice — bob — carol
    //     \           /
    //      dave — eve
    let tx = db.write_tx()?;
    tx.query(
        "CREATE (alice:Person {name: 'Alice', city: 'Portland'}),
                (bob:Person   {name: 'Bob',   city: 'Seattle'}),
                (carol:Person {name: 'Carol', city: 'Seattle'}),
                (dave:Person  {name: 'Dave',  city: 'Portland'}),
                (eve:Person   {name: 'Eve',   city: 'Boise'}),
                (alice)-[:FRIEND]->(bob),
                (bob)-[:FRIEND]->(carol),
                (alice)-[:FRIEND]->(dave),
                (dave)-[:FRIEND]->(eve),
                (eve)-[:FRIEND]->(carol)",
    )?;
    tx.commit()?;

    let tx = db.read_tx()?;

    println!("== Direct friends of Alice ==");
    for row in
        tx.query("MATCH (a:Person {name: 'Alice'})-[:FRIEND]->(f) RETURN f.name AS friend")?
    {
        println!("  {}", value_str(row.get("friend")));
    }

    println!("\n== Friends-of-friends of Alice (2 hops, excluding Alice & direct friends) ==");
    for row in tx.query(
        "MATCH (a:Person {name: 'Alice'})-[:FRIEND*2]->(fof)
         WHERE fof.name <> 'Alice'
           AND NOT (a)-[:FRIEND]->(fof)
         RETURN DISTINCT fof.name AS friend_of_friend",
    )? {
        println!("  {}", value_str(row.get("friend_of_friend")));
    }

    println!("\n== Reachable within 1..3 hops from Alice ==");
    for row in tx.query(
        "MATCH p = (a:Person {name: 'Alice'})-[:FRIEND*1..3]->(target)
         RETURN target.name AS name, length(p) AS hops
         ORDER BY hops, name",
    )? {
        println!(
            "  {} (hops={})",
            value_str(row.get("name")),
            value_str(row.get("hops"))
        );
    }

    Ok(())
}

fn value_str(v: Option<&Value>) -> String {
    v.map(|v| v.to_string())
        .unwrap_or_else(|| "NULL".to_string())
}
