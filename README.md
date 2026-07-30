# RustHouse

RustHouse is a small analytical SQL engine written in Rust. It keeps tables in memory and stores each field in a contiguous, typed column (Vec<i64>, Vec<f64>, Vec<bool>, or Vec<String>).

## What works

- CREATE TABLE with Int64, Float64, Bool, and String columns
- multi-row INSERT INTO ... VALUES with row-width and exact type validation
- streaming COPY FROM Parquet with bounded batches and exact type validation
- SELECT * and named projections, with optional AS aliases
- WHERE comparisons using =, !=, <>, <, <=, >, and >=
- AND, OR, and parentheses in predicates (AND binds more tightly)
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

## Parquet input

Load a local Parquet file into an existing table:

~~~sql
COPY events FROM '/data/events.parquet' FORMAT PARQUET;
COPY events (label, id, score, active) FROM '/data/reordered.parquet' FORMAT PARQUET;
~~~

The optional target-column list maps file columns by position and must name every table column exactly once. COPY accepts flat required or optional scalar fields with these exact mappings: Parquet `INT64` (unannotated or signed 64-bit annotated) to `Int64`, unannotated `DOUBLE` to `Float64`, unannotated `BOOLEAN` to `Bool`, and UTF-8 annotated `BYTE_ARRAY` to `String`. Uncompressed, Snappy, and Zstandard files are supported. Nested, repeated, widening, and other logical types are rejected before rows are appended.

RustHouse columns are non-nullable. Optional Parquet fields are accepted when their values are present, but an encountered null fails COPY. The reader processes row groups in batches of at most 1,024 rows. Each decoded batch is validated and appended atomically; if a later batch or row group fails, earlier completed batches remain in the table.

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

RustHouse has no SQL NULL storage, joins, arithmetic expressions, updates, deletes, quoted identifiers, persistence, transactions spanning multiple SQL statements, HTTP API, or network protocol. Data exists only for the lifetime of the Database value or CLI process. A multi-row INSERT is validated in full before any of its rows are appended.

To keep recursive predicate processing bounded, each WHERE expression is limited to 64 levels of parenthesis nesting and 256 total comparison/boolean AST nodes. Queries over either limit return a SQL error before execution.

On empty input, COUNT and SUM return numeric zero. MIN, MAX, and AVG return an actionable error because the current type system has no nullable result.

## Development

Run the complete checks with:

~~~bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
~~~
