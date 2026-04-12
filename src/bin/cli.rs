use std::io::{self, BufRead, Write};

use graphdblite::{Database, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let (db_path, query) = parse_args(&args);

    let mut db = match &db_path {
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

    match query {
        Some(q) => {
            // Single query mode.
            run_query(&mut db, &q);
        }
        None => {
            // Interactive REPL.
            run_repl(&mut db);
        }
    }
}

fn parse_args(args: &[String]) -> (Option<String>, Option<String>) {
    let mut db_path = None;
    let mut query = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "-q" | "--query" => {
                i += 1;
                if i < args.len() {
                    query = Some(args[i].clone());
                } else {
                    eprintln!("error: -q requires a query string");
                    std::process::exit(1);
                }
            }
            arg if arg.starts_with('-') => {
                eprintln!("error: unknown flag: {arg}");
                std::process::exit(1);
            }
            _ => {
                if db_path.is_none() {
                    db_path = Some(args[i].clone());
                } else {
                    eprintln!("error: unexpected argument: {}", args[i]);
                    std::process::exit(1);
                }
            }
        }
        i += 1;
    }

    (db_path, query)
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
    eprintln!("  -q, --query    Execute a single Cypher query and exit");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  graphdblite my.db");
    eprintln!("  graphdblite my.db -q \"MATCH (n:Person) RETURN n.name\"");
    eprintln!("  graphdblite my.db -q \"CREATE (n:Person {{name: 'Alice'}})\"");
}

fn run_query(db: &mut Database, cypher: &str) {
    let is_read_only = is_read_query(cypher);

    if is_read_only {
        let tx = db.begin_read().unwrap_or_else(|e| {
            eprintln!("error: {e}");
            std::process::exit(1);
        });
        match tx.query(cypher) {
            Ok(records) => print_records(&records),
            Err(e) => eprintln!("error: {e}"),
        }
        let _ = tx.commit();
    } else {
        let tx = db.begin_write().unwrap_or_else(|e| {
            eprintln!("error: {e}");
            std::process::exit(1);
        });
        match tx.query(cypher) {
            Ok(records) => {
                print_records(&records);
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
}

fn run_repl(db: &mut Database) {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    eprintln!("graphdblite v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("Type Cypher queries. Press Ctrl-D to exit.");
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
            eprintln!("  .help    Show this help");
            eprintln!("  .quit    Exit the REPL");
            eprintln!("  .exit    Exit the REPL");
            eprintln!();
            eprintln!("Enter any Cypher query to execute it.");
            continue;
        }

        run_query(db, trimmed);
    }
}

fn is_read_query(cypher: &str) -> bool {
    let upper = cypher.trim().to_uppercase();
    upper.starts_with("MATCH")
        && !upper.contains("DELETE")
        && !upper.contains("SET ")
        && !upper.contains("CREATE")
}

fn print_records(records: &[graphdblite::Record]) {
    if records.is_empty() {
        println!("(no results)");
        return;
    }

    // Collect column names from the first record, hiding internal fields.
    let columns: Vec<&String> = {
        let mut cols: Vec<&String> = records[0]
            .fields
            .keys()
            .filter(|k| !k.contains(".__"))
            .collect();
        cols.sort();
        cols
    };

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
    }
}
