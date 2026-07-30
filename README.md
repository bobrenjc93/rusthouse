# RustHouse

RustHouse is a small, dependency-free analytical SQL engine written in Rust. It keeps tables in memory and stores each field in a contiguous, typed column (Vec<i64>, Vec<f64>, Vec<bool>, or Vec<String>).

## What works

- CREATE TABLE with Int64, Float64, Bool, and String columns
- multi-row INSERT INTO ... VALUES with row-width and exact type validation
- SELECT * and named projections, with optional AS aliases
- WHERE comparisons using =, !=, <>, <, <=, >, and >=
- AND, OR, and parentheses in predicates (AND binds more tightly)
- append-maintained 1,024-row zone maps for conservative WHERE scan pruning
- COUNT, SUM, MIN, MAX, and AVG
- GROUP BY, output-column or alias ORDER BY with ASC/DESC, and LIMIT
- semicolon-separated SQL batches
- table, CSV, and JSON output from the CLI
- SQL input from --execute or standard input

Identifiers are unquoted and case-insensitive; TRUE and FALSE are reserved Boolean literals and cannot be column names. String literals use single quotes; write a quote inside one as ''.

## CLI

Run a batch directly:

~~~bash
cargo run -- --execute "
  CREATE TABLE sales (region String, amount Int64, online Bool);
  INSERT INTO sales VALUES
    ('west', 10, true),
    ('east', 4, false),
    ('west', 7, true);
  SELECT region, COUNT(*) AS orders, SUM(amount) AS total, AVG(amount) AS mean
  FROM sales
  WHERE online = true
  GROUP BY region
  ORDER BY total DESC
  LIMIT 10;
"
~~~

Choose table (the default), csv, or json:

~~~bash
cargo run -- --format json --execute \
  "CREATE TABLE t (id Int64); INSERT INTO t VALUES (2), (1); SELECT * FROM t ORDER BY id"
~~~

Or pipe a batch through standard input:

~~~bash
printf '%s\n' \
  "CREATE TABLE flags (name String, enabled Bool);" \
  "INSERT INTO flags VALUES ('search', true);" \
  "SELECT * FROM flags;" |
  cargo run -- --format csv
~~~

Command acknowledgements go to stderr so CSV and JSON query data on stdout remain usable in pipelines.
JSON output is always one document with a top-level results array. Each SELECT result contains explicit column name/type metadata and positional row arrays, so multiple SELECT statements and duplicate aliases preserve every value.

## Library API

Database retains an in-memory catalog across calls and returns structured results:

Database parses a complete SQL batch before execution: any syntax error leaves the catalog unchanged. After parsing succeeds, statements execute in order; if a later execution error occurs, earlier successful statements remain applied.

~~~rust
use rusthouse::{Database, StatementResult};

let mut database = Database::new();
database.execute("CREATE TABLE events (id Int64, name String)")?;
database.execute("INSERT INTO events VALUES (1, 'launch')")?;

let results = database.execute("SELECT * FROM events WHERE id = 1")?;
let StatementResult::Query(result) = &results[0] else {
    unreachable!();
};
assert_eq!(result.rows.len(), 1);

let scan = database.last_scan_stats();
assert!(scan.row_groups_scanned <= scan.row_groups_total);

# Ok::<(), rusthouse::Error>(())
~~~

## Current boundaries

RustHouse has no NULL, joins, arithmetic expressions, updates, deletes, quoted identifiers, persistence, transactions spanning multiple SQL statements, HTTP API, or network protocol. Data exists only for the lifetime of the Database value or CLI process. A multi-row INSERT is validated in full before any of its rows are appended.

To keep recursive predicate processing bounded, each WHERE expression is limited to 64 levels of parenthesis nesting and 256 total comparison/boolean AST nodes. Queries over either limit return a SQL error before execution.

On empty input, COUNT and SUM return numeric zero. MIN, MAX, and AVG return an actionable error because the current type system has no nullable result.

## Development

The crate has no third-party dependencies. Run the complete checks with:

~~~bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
~~~

The standalone zone-map benchmark measures point, compound-selective, and full scans over one million rows without participating in ClickHouse parity scoring:

~~~bash
cargo bench --bench zone_map
~~~

On 2026-07-29, an arm64 macOS release run measured 9.622 us/query for the point filter (1/977 groups and 1,024 rows examined), 32.489 us/query for the compound filter (2/977 groups and 1,600 rows examined), and 2,628.785 us/query for the unfiltered baseline (all 1,000,000 rows examined). Query parsing is included in every iteration. These measurements are informational only; the parity benchmark's modes, workloads, weights, and scoring are unchanged.
