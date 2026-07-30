# RustHouse

RustHouse is a small, dependency-free analytical SQL engine written in Rust. It keeps tables in memory and stores each field in a contiguous, typed column (Vec<i64>, Vec<f64>, Vec<bool>, or Vec<String>).

## What works

- CREATE TABLE with Int64, Float64, Bool, and String columns
- multi-row INSERT INTO ... VALUES with row-width and exact type validation
- SELECT * and named projections, with optional AS aliases
- WHERE comparisons using =, !=, <>, <, <=, >, and >=
- AND, OR, and parentheses in predicates (AND binds more tightly)
- COUNT, SUM, MIN, MAX, and AVG
- GROUP BY, output-column or alias ORDER BY with ASC/DESC, and LIMIT
- semicolon-separated SQL batches
- table, CSV, and JSON output from the CLI
- SQL input from --execute or standard input
- cooperative scan-row, output-row, deadline, and cancellation controls

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

Bound a batch's resource use with row limits and a wall-clock timeout:

~~~bash
cargo run -- --max-scan-rows 100000 --max-output-rows 1000 --timeout-ms 5000 \
  --execute "SELECT * FROM events ORDER BY id LIMIT 1000"
~~~

Limits count cumulatively across all SELECT statements in the input batch. Row
maximums are inclusive, so a limit of 100 permits exactly 100 rows. Exceeding a
limit, reaching the deadline, or cancelling execution returns a structured
error and a nonzero CLI status.

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

# Ok::<(), rusthouse::Error>(())
~~~

Use `execute_with_options` for bounded or externally cancellable work. The
cancellation token is cloneable and can be signalled from another thread:

~~~rust
use std::time::{Duration, Instant};
use rusthouse::{CancellationToken, Database, ExecutionLimits, ExecutionOptions};

let token = CancellationToken::new();
let canceller = token.clone();
let options = ExecutionOptions::new(
    ExecutionLimits {
        max_scan_rows: Some(1_000_000),
        max_output_rows: Some(10_000),
        deadline: Instant::now().checked_add(Duration::from_secs(2)),
    },
    token,
);

// Another thread may call `canceller.cancel()` while execution is in progress.
let mut database = Database::new();
let _ = canceller;
database.execute_with_options("CREATE TABLE events (id Int64)", &options)?;

# Ok::<(), rusthouse::Error>(())
~~~

`Database::execute` remains the unlimited API, retains the optimized sort path,
and does not perform row accounting or atomic/deadline checks. An aborted SELECT
does not poison the database; later calls can reuse it normally. As with
unlimited batches, commands completed before a later execution error remain
applied.

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
