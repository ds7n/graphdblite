use std::io::{self, BufRead, Write};

use graphdblite::{Database, Edge, Node, PathValue, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Table,
    Json,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let opts = parse_args(&args);

    let mut db = match &opts.db_path {
        Some(path) => Database::open(path).unwrap_or_else(|e| {
            eprintln!("error: failed to open {path}: {e}");
            std::process::exit(1);
        }),
        None => {
            eprintln!("error: database path required");
            eprintln!();
            print_usage();
            std::process::exit(1);
        }
    };

    match opts.query {
        Some(q) => run_query(&mut db, &q, opts.mode),
        None => run_repl(&mut db, opts.mode),
    }
}

struct Opts {
    db_path: Option<String>,
    query: Option<String>,
    mode: OutputMode,
}

fn parse_args(args: &[String]) -> Opts {
    let mut opts = Opts {
        db_path: None,
        query: None,
        mode: OutputMode::Table,
    };
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("graphdblite {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-q" | "--query" => {
                i += 1;
                if i < args.len() {
                    opts.query = Some(args[i].clone());
                } else {
                    eprintln!("error: -q requires a query string");
                    std::process::exit(1);
                }
            }
            "-j" | "--json" => {
                opts.mode = OutputMode::Json;
            }
            arg if arg.starts_with('-') => {
                eprintln!("error: unknown flag: {arg}");
                std::process::exit(1);
            }
            _ => {
                if opts.db_path.is_none() {
                    opts.db_path = Some(args[i].clone());
                } else {
                    eprintln!("error: unexpected argument: {}", args[i]);
                    std::process::exit(1);
                }
            }
        }
        i += 1;
    }

    opts
}

fn print_usage() {
    eprintln!("graphdblite — embedded graph database CLI");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  graphdblite <db-path>                  Interactive REPL");
    eprintln!("  graphdblite <db-path> -q <cypher>      Run a single query");
    eprintln!();
    eprintln!("Flags:");
    eprintln!("  -h, --help     Show this help");
    eprintln!("  -V, --version  Print version and exit");
    eprintln!("  -q, --query    Execute a single Cypher query and exit");
    eprintln!("  -j, --json     Output NDJSON (one JSON object per row)");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  graphdblite my.db");
    eprintln!("  graphdblite my.db -q \"MATCH (n:Person) RETURN n.name\"");
    eprintln!("  graphdblite my.db -j -q \"MATCH (n:Person) RETURN n.name\"");
    eprintln!();
    eprintln!("REPL commands:");
    eprintln!("  .help              Show REPL help");
    eprintln!("  .mode table|json   Switch output mode");
    eprintln!("  .quit / .exit      Exit");
}

fn run_query(db: &mut Database, cypher: &str, mode: OutputMode) {
    // Always use a write transaction. The CLI is single-threaded and runs one
    // query per invocation, so any "read-only" optimization to acquire a
    // deferred txn is not worth dragging the parser into this binary.
    let tx = db.write_tx().unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });
    match tx.query(cypher) {
        Ok(records) => {
            print_records(&records, mode);
            tx.commit().unwrap_or_else(|e| {
                eprintln!("error committing: {e}");
                std::process::exit(1);
            });
        }
        Err(e) => {
            eprintln!("error: {e}");
            let _ = tx.rollback();
        }
    }
}

fn run_repl(db: &mut Database, initial_mode: OutputMode) {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut mode = initial_mode;

    eprintln!("graphdblite v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("Type Cypher queries. Press Ctrl-D to exit. Type .help for commands.");
    eprintln!();

    loop {
        print!("cypher> ");
        stdout.flush().unwrap();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("read error: {e}");
                break;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed == ".quit" || trimmed == ".exit" {
            break;
        }

        if trimmed == ".help" {
            eprintln!("Commands:");
            eprintln!("  .help              Show this help");
            eprintln!(
                "  .mode table|json   Switch output mode (current: {})",
                mode_name(mode)
            );
            eprintln!("  .quit / .exit      Exit the REPL");
            eprintln!();
            eprintln!("Enter any Cypher query to execute it.");
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix(".mode") {
            match rest.trim() {
                "table" => {
                    mode = OutputMode::Table;
                    eprintln!("output mode: table");
                }
                "json" => {
                    mode = OutputMode::Json;
                    eprintln!("output mode: json");
                }
                "" => eprintln!(
                    "current mode: {} (use .mode table | .mode json)",
                    mode_name(mode)
                ),
                other => eprintln!("error: unknown mode '{other}' (expected: table, json)"),
            }
            continue;
        }

        if trimmed.starts_with('.') {
            eprintln!("error: unknown command '{trimmed}' (try .help)");
            continue;
        }

        run_query(db, trimmed, mode);
    }
}

fn mode_name(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::Table => "table",
        OutputMode::Json => "json",
    }
}

fn visible_columns(rec: &graphdblite::Record) -> Vec<&String> {
    let mut cols: Vec<&String> = rec.fields.keys().filter(|k| !k.contains(".__")).collect();
    cols.sort();
    cols
}

fn print_records(records: &[graphdblite::Record], mode: OutputMode) {
    match mode {
        OutputMode::Table => print_records_table(records),
        OutputMode::Json => print_records_json(records),
    }
}

fn print_records_json(records: &[graphdblite::Record]) {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for rec in records {
        let cols = visible_columns(rec);
        let mut s = String::from("{");
        for (i, col) in cols.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            json_escape_into(col, &mut s);
            s.push(':');
            value_to_json(rec.fields.get(*col).unwrap_or(&Value::Null), &mut s);
        }
        s.push('}');
        let _ = writeln!(out, "{s}");
    }
}

fn print_records_table(records: &[graphdblite::Record]) {
    if records.is_empty() {
        println!("(no results)");
        return;
    }

    let columns = visible_columns(&records[0]);

    if columns.is_empty() {
        println!("(empty record)");
        return;
    }

    // Compute column widths.
    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    for rec in records {
        for (i, col) in columns.iter().enumerate() {
            let val_str = format_value(rec.fields.get(*col));
            widths[i] = widths[i].max(val_str.len());
        }
    }

    // Print header.
    let header: String = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{:width$}", c, width = widths[i]))
        .collect::<Vec<_>>()
        .join(" | ");
    println!("{header}");

    // Separator.
    let sep: String = widths
        .iter()
        .map(|w| "-".repeat(*w))
        .collect::<Vec<_>>()
        .join("-+-");
    println!("{sep}");

    // Print rows.
    for rec in records {
        let row: String = columns
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let val_str = format_value(rec.fields.get(*col));
                format!("{:width$}", val_str, width = widths[i])
            })
            .collect::<Vec<_>>()
            .join(" | ");
        println!("{row}");
    }

    println!();
    println!("{} row(s)", records.len());
}

fn format_value(val: Option<&Value>) -> String {
    match val {
        None | Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::I64(n)) => n.to_string(),
        Some(Value::F64(n)) => format!("{n:.6}"),
        Some(Value::String(s)) => s.clone(),
        Some(Value::List(items)) => format!("{}", Value::List(items.clone())),
        Some(p @ Value::Path(_)) => format!("{p}"),
        Some(m @ Value::Map(_)) => format!("{m}"),
        Some(n @ Value::Node(_)) => format!("{n}"),
        Some(e @ Value::Edge(_)) => format!("{e}"),
        Some(other) => format!("{other}"),
    }
}

// --- minimal JSON serialization (avoids pulling serde_json into the lib) ---

fn value_to_json(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::I64(n) => out.push_str(&n.to_string()),
        Value::F64(n) => {
            // JSON has no NaN/Infinity — coerce to null per common convention.
            if n.is_finite() {
                out.push_str(&format!("{n}"));
            } else {
                out.push_str("null");
            }
        }
        Value::String(s) => json_escape_into(s, out),
        Value::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                value_to_json(item, out);
            }
            out.push(']');
        }
        Value::Map(map) => {
            out.push('{');
            for (i, (k, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_escape_into(k, out);
                out.push(':');
                value_to_json(val, out);
            }
            out.push('}');
        }
        Value::Node(n) => node_to_json(n, out),
        Value::Edge(e) => edge_to_json(e, out),
        Value::Path(p) => path_to_json(p, out),
        // Temporal types: ISO-8601 strings via Display.
        Value::Date(d) => json_escape_into(&d.to_string(), out),
        Value::LocalTime(t) => json_escape_into(&t.to_string(), out),
        Value::Time(t) => json_escape_into(&t.to_string(), out),
        Value::LocalDateTime(dt) => json_escape_into(&dt.to_string(), out),
        Value::DateTime(dt) => json_escape_into(&dt.to_string(), out),
        Value::Duration(d) => json_escape_into(&d.to_string(), out),
    }
}

fn node_to_json(n: &Node, out: &mut String) {
    out.push_str("{\"__type\":\"node\",\"id\":");
    out.push_str(&n.id.0.to_string());
    out.push_str(",\"labels\":[");
    for (i, l) in n.labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_escape_into(l, out);
    }
    out.push_str("],\"properties\":");
    properties_to_json(&n.properties, out);
    out.push('}');
}

fn edge_to_json(e: &Edge, out: &mut String) {
    out.push_str("{\"__type\":\"edge\",\"src\":");
    out.push_str(&e.src.0.to_string());
    out.push_str(",\"dst\":");
    out.push_str(&e.dst.0.to_string());
    out.push_str(",\"label\":");
    json_escape_into(&e.label, out);
    out.push_str(",\"properties\":");
    properties_to_json(&e.properties, out);
    out.push('}');
}

fn path_to_json(p: &PathValue, out: &mut String) {
    out.push_str("{\"__type\":\"path\",\"nodes\":[");
    for (i, n) in p.nodes.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        node_to_json(n, out);
    }
    out.push_str("],\"edges\":[");
    for (i, e) in p.edges.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        edge_to_json(e, out);
    }
    out.push_str("]}");
}

fn properties_to_json(props: &graphdblite::Properties, out: &mut String) {
    let mut keys: Vec<&String> = props.keys().collect();
    keys.sort();
    out.push('{');
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_escape_into(k, out);
        out.push(':');
        value_to_json(&props[*k], out);
    }
    out.push('}');
}

fn json_escape_into(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
