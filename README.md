# RustHouse

RustHouse is a small analytical SQL engine written in Rust. It stores each field in a contiguous, typed column (`Vec<i64>`, `Vec<f64>`, `Vec<bool>`, or `Vec<String>`) and can persist the catalog as an atomic snapshot.

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
- versioned, checksummed database snapshots with atomic mutation checkpoints

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

On Linux and macOS, persist data across invocations with `--database` (or `-d`):

~~~bash
cargo run -- --database rusthouse.db --execute \
  "CREATE TABLE events (id Int64, name String); INSERT INTO events VALUES (1, 'launch')"
cargo run -- --database rusthouse.db --execute \
  "SELECT * FROM events ORDER BY id"
~~~

The database destination is resolved once before SQL execution: relative paths remain anchored to the opening directory, and a symlink continues to target its canonical file rather than being replaced. Each successful `CREATE` or `INSERT` is written to a private same-directory temporary file, synced, atomically renamed over the previous snapshot, and followed by a parent-directory sync. Existing mode, owner, group, ACLs, and extended attributes are reproduced and verified before rename. Snapshots include a format version, declared length, and CRC-32 checksum; truncated, corrupt, and unsupported snapshots are rejected rather than partially loaded. Persistent databases are rejected on other platforms until an equivalent durable replacement and metadata protocol is available.

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

Use `Database::open("rusthouse.db")` for the same automatically checkpointed behavior as the CLI. A syntax error still applies nothing. If execution fails after earlier statements succeeded, their checkpoints remain. A checkpoint failure before rename rolls back that mutation in memory. Rename is the commit boundary: if the subsequent parent-directory sync fails, the error states that crash durability is uncertain, while the live snapshot and in-memory catalog both retain the committed mutation.

## Current boundaries

RustHouse has no NULL, joins, arithmetic expressions, updates, deletes, quoted identifiers, transactions spanning multiple SQL statements, HTTP API, or network protocol. Snapshot files coordinate no concurrent writers, so use one process at a time for a given database path. A multi-row INSERT is validated in full before any of its rows are appended.

To keep recursive predicate processing bounded, each WHERE expression is limited to 64 levels of parenthesis nesting and 256 total comparison/boolean AST nodes. Queries over either limit return a SQL error before execution.

On empty input, COUNT and SUM return numeric zero. MIN, MAX, and AVG return an actionable error because the current type system has no nullable result.

## Development

The crate has no third-party dependencies. Run the complete checks with:

~~~bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
~~~
