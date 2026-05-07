//! Temporal example.
//!
//! Builds a small calendar of events and runs queries that exercise Cypher's
//! temporal types (Date, DateTime, Duration):
//!   - Events on a specific date.
//!   - Events in a date range.
//!   - Computing durations between events.
//!
//! Run with: `cargo run --example temporal_events`

use graphdblite::{Database, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join("graphdblite-temporal-events.db");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::open(&path)?;

    let tx = db.write_tx()?;
    tx.query(
        "CREATE (:Event {name: 'Kickoff',     starts_at: datetime('2026-04-01T09:00:00Z'), duration: duration('PT1H')}),
                (:Event {name: 'Standup',     starts_at: datetime('2026-04-02T09:30:00Z'), duration: duration('PT15M')}),
                (:Event {name: 'Design review', starts_at: datetime('2026-04-02T14:00:00Z'), duration: duration('PT45M')}),
                (:Event {name: 'Demo',        starts_at: datetime('2026-04-15T16:00:00Z'), duration: duration('PT30M')}),
                (:Event {name: 'Retro',       starts_at: datetime('2026-04-30T15:00:00Z'), duration: duration('PT1H')})",
    )?;
    tx.commit()?;

    let tx = db.read_tx()?;

    println!("== Events on 2026-04-02 ==");
    for row in tx.query(
        "MATCH (e:Event)
         WHERE date(e.starts_at) = date('2026-04-02')
         RETURN e.name AS name, e.starts_at AS starts_at
         ORDER BY starts_at",
    )? {
        println!(
            "  {:20} @ {}",
            value_str(row.get("name")),
            value_str(row.get("starts_at"))
        );
    }

    println!("\n== Events in the first half of April 2026 ==");
    for row in tx.query(
        "MATCH (e:Event)
         WHERE date(e.starts_at) >= date('2026-04-01')
           AND date(e.starts_at) <= date('2026-04-15')
         RETURN e.name AS name, date(e.starts_at) AS day
         ORDER BY day, name",
    )? {
        println!(
            "  {:20} on {}",
            value_str(row.get("name")),
            value_str(row.get("day"))
        );
    }

    println!("\n== Time from each event to the Retro ==");
    for row in tx.query(
        "MATCH (e:Event), (retro:Event {name: 'Retro'})
         WHERE e.name <> 'Retro'
         RETURN e.name AS name,
                duration.between(e.starts_at, retro.starts_at) AS until_retro
         ORDER BY name",
    )? {
        println!(
            "  {:20} -> {} until Retro",
            value_str(row.get("name")),
            value_str(row.get("until_retro"))
        );
    }

    Ok(())
}

fn value_str(v: Option<&Value>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "NULL".to_string())
}
