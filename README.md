# RustHouse

RustHouse is a small analytical SQL engine written in Rust. It keeps tables in memory and stores each field in a contiguous, typed column (Vec<i64>, Vec<f64>, Vec<bool>, or Vec<String>).

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
- a bounded HTTP service with shared state and JSON/CSV responses

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

## HTTP service

Start a server on an explicit loopback address. Non-loopback and wildcard listeners are rejected because the mutable endpoint does not implement authentication:

~~~bash
cargo run -- serve --listen 127.0.0.1:8080
~~~

`POST /query` accepts a UTF-8 SQL batch as its request body. It defaults to JSON and negotiates `application/json` or `text/csv` through the `Accept` header. State is retained for the lifetime of the server process:

~~~bash
curl -H 'Content-Type: application/sql' --data-binary \
  "CREATE TABLE events (id Int64); INSERT INTO events VALUES (1), (2)" \
  http://127.0.0.1:8080/query

curl -H 'Content-Type: application/sql' -H 'Accept: text/csv' --data-binary \
  "SELECT * FROM events ORDER BY id" \
  http://127.0.0.1:8080/query

curl http://127.0.0.1:8080/health
~~~

The query endpoint requires `Content-Type: application/sql` and rejects requests containing an `Origin` header. This non-simple media type plus the absence of CORS preflight support prevents browser cross-origin form and fetch requests from reaching mutable SQL execution.

The server parses a batch before locking the database. Batches containing only `SELECT` statements share a read lock; any batch containing `CREATE` or `INSERT` holds the exclusive write lock through the complete batch. The service caps bodies at 1 MiB, headers at 16 KiB, SQL tokens at 65,536, accepted connections at 128, request workers at 8, statements per batch at 32, scanned rows at 100,000, intermediate groups and result rows at 10,000, result cells at 100,000, materialized result values at 2 MiB, and encoded responses at 4 MiB.

Retained state is capped across requests at 64 tables, 100,000 total rows, 1,000,000 cells, and 32 MiB of stored value data. CREATE and INSERT check retained growth before mutation, so a limit error does not partially change state. Queueing, request reads, lock acquisition, execution (including sorting), rendering, and successful response writes share an absolute 10-second deadline; timeout errors have a bounded 250 ms write grace. SIGINT and SIGTERM stop accepting connections and drain bounded accepted work before exit.

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

RustHouse has no NULL, joins, arithmetic expressions, updates, deletes, quoted identifiers, persistence, transactions spanning multiple SQL statements, or external network protocol beyond HTTP. Data exists only for the lifetime of the Database value, CLI process, or HTTP server process. A multi-row INSERT is validated in full before any of its rows are appended.

To keep recursive predicate processing bounded, each WHERE expression is limited to 64 levels of parenthesis nesting and 256 total comparison/boolean AST nodes. Queries over either limit return a SQL error before execution.

On empty input, COUNT and SUM return numeric zero. MIN, MAX, and AVG return an actionable error because the current type system has no nullable result.

## Development

The crate uses `ctrlc` for portable SIGINT and SIGTERM handling. Run the complete checks with:

~~~bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
~~~
