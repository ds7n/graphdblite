//! Recommendation example.
//!
//! Builds a tiny User-PURCHASED-Product graph and runs collaborative-filtering
//! style queries:
//!   - "People who bought X also bought ..."
//!   - Top-N recommendations for a given user, ranked by co-purchase count.
//!
//! Run with: `cargo run --example recommendation`

use graphdblite::{Database, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join("graphdblite-recommendation.db");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::open(&path)?;

    let tx = db.write_tx()?;
    tx.query(
        "CREATE (alice:User {name: 'Alice'}),
                (bob:User   {name: 'Bob'}),
                (carol:User {name: 'Carol'}),
                (dave:User  {name: 'Dave'}),
                (book:Product   {sku: 'BOOK-1',   title: 'Graph Algorithms'}),
                (cup:Product    {sku: 'CUP-1',    title: 'Coffee Mug'}),
                (laptop:Product {sku: 'LAPTOP-1', title: 'Laptop Stand'}),
                (mouse:Product  {sku: 'MOUSE-1',  title: 'Wireless Mouse'}),
                (kbd:Product    {sku: 'KBD-1',    title: 'Mechanical Keyboard'}),
                (alice)-[:PURCHASED]->(book),
                (alice)-[:PURCHASED]->(cup),
                (alice)-[:PURCHASED]->(laptop),
                (bob)-[:PURCHASED]->(book),
                (bob)-[:PURCHASED]->(mouse),
                (bob)-[:PURCHASED]->(kbd),
                (carol)-[:PURCHASED]->(cup),
                (carol)-[:PURCHASED]->(laptop),
                (carol)-[:PURCHASED]->(mouse),
                (dave)-[:PURCHASED]->(kbd)",
    )?;
    tx.commit()?;

    let tx = db.read_tx()?;

    println!("== People who bought 'Graph Algorithms' also bought ==");
    for row in tx.query(
        "MATCH (p:Product {sku: 'BOOK-1'})<-[:PURCHASED]-(other:User)-[:PURCHASED]->(rec:Product)
         WHERE rec.sku <> 'BOOK-1'
         RETURN rec.title AS title, count(*) AS co_purchases
         ORDER BY co_purchases DESC, title",
    )? {
        println!(
            "  {:30} (co_purchases={})",
            value_str(row.get("title")),
            value_str(row.get("co_purchases"))
        );
    }

    println!("\n== Top recommendations for Alice (products she hasn't bought) ==");
    for row in tx.query(
        "MATCH (alice:User {name: 'Alice'})-[:PURCHASED]->(seed:Product)
              <-[:PURCHASED]-(peer:User)-[:PURCHASED]->(rec:Product)
         WHERE peer.name <> 'Alice'
           AND NOT (alice)-[:PURCHASED]->(rec)
         RETURN rec.title AS title, count(DISTINCT peer) AS peer_count
         ORDER BY peer_count DESC, title",
    )? {
        println!(
            "  {:30} (recommended by {} peer(s))",
            value_str(row.get("title")),
            value_str(row.get("peer_count"))
        );
    }

    Ok(())
}

fn value_str(v: Option<&Value>) -> String {
    v.map(|v| v.to_string())
        .unwrap_or_else(|| "NULL".to_string())
}
