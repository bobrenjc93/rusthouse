# RustHouse

RustHouse is a small, dependency-free analytical SQL engine written in Rust. It keeps tables in memory and stores each field in a contiguous, typed column (`Vec<i64>`, `Vec<f64>`, `Vec<bool>`, or `Vec<String>`). Nullable columns retain that layout and add a compact one-bit-per-row validity map.

## What works

- CREATE TABLE with Int64, Float64, Bool, String, and Nullable(T) columns
- multi-row INSERT INTO ... VALUES with row-width and exact type validation
- SELECT * and named projections, with optional AS aliases
- NULL literals, IS NULL, and IS NOT NULL
- three-valued WHERE comparisons, AND, and OR (AND binds more tightly)
- NULL-skipping COUNT, SUM, MIN, MAX, and AVG
- GROUP BY, output-column or alias ORDER BY with ASC/DESC, and LIMIT
- semicolon-separated SQL batches
- table, CSV, and JSON output from the CLI
- SQL input from --execute or standard input

Identifiers are unquoted and case-insensitive; TRUE, FALSE, and NULL are reserved literals and cannot be column names. String literals use single quotes; write a quote inside one as ''.

Comparisons involving NULL evaluate to unknown and are not selected by WHERE. NULL values form one group and sort after non-NULL values for both ascending and descending order. `COUNT(*)` counts rows, `COUNT(column)` skips NULL, and every other aggregate skips NULL and returns NULL for empty or all-NULL input. Table output writes `NULL`, CSV writes `\N`, and JSON writes `null`.

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

# Ok::<(), rusthouse::Error>(())
~~~

## Current boundaries

RustHouse has no joins, arithmetic expressions, updates, deletes, quoted identifiers, persistence, transactions spanning multiple SQL statements, HTTP API, or network protocol. Data exists only for the lifetime of the Database value or CLI process. A multi-row INSERT is validated in full before any of its rows are appended.

To keep recursive predicate processing bounded, each WHERE expression is limited to 64 levels of parenthesis nesting and 256 total comparison/boolean AST nodes. Queries over either limit return a SQL error before execution.

On empty input, COUNT returns numeric zero. SUM, MIN, MAX, and AVG return NULL.

## Development

The crate has no third-party dependencies. Run the complete checks with:

~~~bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
~~~
