# RustHouse

RustHouse is a small, dependency-free analytical SQL engine written in Rust. It keeps tables in memory and stores each field in a contiguous, typed column (`Vec<i64>`, `Vec<f64>`, `Vec<bool>`, or `Vec<String>`). Nullable columns add a packed validity bitmap without wrapping every value in `Option`.

## What works

- CREATE TABLE with Int64, Float64, Bool, String, and Nullable(T) columns
- multi-row INSERT INTO ... VALUES with row-width and exact type validation
- SELECT * and named projections, with optional AS aliases
- NULL literals, IS NULL, IS NOT NULL, and three-valued WHERE predicates
- WHERE comparisons using =, !=, <>, <, <=, >, and >=
- AND, OR, and parentheses in predicates (AND binds more tightly)
- COUNT, SUM, MIN, MAX, and AVG as grouped or bounded ROWS window aggregates
- ROW_NUMBER, RANK, and DENSE_RANK ranking windows
- GROUP BY, output-column or alias ORDER BY with ASC/DESC, and LIMIT
- semicolon-separated SQL batches
- table, CSV, and JSON output from the CLI
- SQL input from --execute or standard input

Identifiers are unquoted and case-insensitive; TRUE, FALSE, and NULL are reserved literals and cannot be column names. String literals use single quotes; write a quote inside one as ''.

`NULL` can only be inserted into a `Nullable(T)` column. Comparisons involving NULL evaluate to UNKNOWN; WHERE keeps only TRUE rows. Aggregates ignore NULL inputs, while `COUNT(*)` counts every row. `SUM`, `MIN`, `MAX`, and `AVG` return NULL when no non-NULL input exists; `COUNT` returns zero. GROUP BY collects all NULL keys into one group. ORDER BY places NULL last for ascending order and first for descending order.

Windows use `OVER (PARTITION BY ... ORDER BY ...)`. Ranking windows do not accept a frame. Aggregate windows require either `ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW` for a cumulative result or `ROWS BETWEEN n PRECEDING AND CURRENT ROW` for a sliding result. The shorthand forms `ROWS UNBOUNDED PRECEDING` and `ROWS n PRECEDING` are also accepted. Fixed preceding bounds are limited to 1,000,000 rows. Windows run after `WHERE` and before the final output `ORDER BY` and `LIMIT`; tied window-order values retain insertion order. Window aggregates ignore NULL using the same rules as grouped aggregates.

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

Windows require both `PARTITION BY` and `ORDER BY` and cannot be combined with `GROUP BY` or non-window aggregates in the same SELECT. `RANGE`, `GROUPS`, following-row frames, and frames ending anywhere other than the current row are not supported.

To keep recursive predicate processing bounded, each WHERE expression is limited to 64 levels of parenthesis nesting and 256 total comparison/boolean AST nodes. Queries over either limit return a SQL error before execution.

On empty input, COUNT returns numeric zero. SUM, MIN, MAX, and AVG return NULL.

## Development

The crate has no third-party dependencies. Run the complete checks with:

~~~bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
~~~
