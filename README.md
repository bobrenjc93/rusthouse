# RustHouse

RustHouse is a from-scratch analytical database in Rust, inspired by the useful core of ClickHouse: typed columnar data, fast scans and aggregations, a practical SQL surface, and an operational interface that is easy to embed and understand.

This is intentionally not a wire-compatible ClickHouse clone. The goal is to grow a credible small competitor through measured, reviewed iterations while keeping the implementation approachable.

## Product target

The first useful release should support:

- typed tables with `Int64`, `Float64`, `Bool`, and `String` columns;
- a genuinely columnar in-memory representation;
- `CREATE TABLE`, `INSERT INTO ... VALUES`, and `SELECT`;
- projections, `WHERE` comparisons, `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`, `GROUP BY`, `ORDER BY`, and `LIMIT`;
- a batch/interactive CLI with readable table, CSV, and JSON output;
- durable local snapshots with an explicit, documented file format;
- an HTTP endpoint for executing SQL;
- deterministic tests and a small benchmark that demonstrate analytical behavior.

The early implementation should favor Rust's standard library and a small dependency surface. Correctness, clear errors, bounded resource use, and a modular path toward vectorized execution matter more than superficial feature count.

## Current storage API

The crate provides an in-memory `Table` backed by distinct typed column vectors. A validated `Schema` fixes column order and types, and `Table::insert_rows` atomically validates and inserts batches of up to 65,536 rows. Invalid row widths, type mismatches, non-finite floats, and oversized batches return structured errors without changing the table.

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

```bash
cargo test
cargo run -- --help
```

## Quality gate

The repository pins its Rust toolchain in `rust-toolchain.toml`. Run the same
checks enforced by CI with:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo test --doc --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps --locked
```

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.

## Current SQL surface

The in-memory database executes bounded `CREATE TABLE`, `INSERT INTO ...
VALUES`, and single-table projection `SELECT` statements. A batch is parsed in
full before its statements execute in source order:

```rust
use rusthouse::{Database, Value};

let mut database = Database::new();
database.execute_batch(
    "CREATE TABLE events (id Int64, score Float64, active Bool, label String);
     INSERT INTO events VALUES (1, 2.5, true, 'first');",
)?;

let result = database.execute("SELECT label, id FROM events")?;
let rows = result.query().expect("SELECT returns rows").rows();
assert_eq!(rows, [vec![Value::String("first".to_owned()), Value::Int64(1)]]);
# Ok::<(), rusthouse::Error>(())
```

`SELECT *` and explicit bare column lists are supported. Predicates,
expressions, aggregation, grouping, sorting, and joins are not yet part of the
grammar. SQL keywords, identifiers, and the four type names are
case-insensitive. By default, an input may contain at most 1 MiB and a table
or projection may contain at most 1,024 columns. One materialized query and
all query results retained by `execute_batch` are each limited to 1,048,576
cells. These limits can be changed with `DatabaseConfig` and
`DatabaseConfig::with_result_limits`.

The CLI reads one complete valid semicolon-separated batch from stdin and
rejects oversized input as soon as it crosses the byte limit. It streams each
completed `SELECT` as CSV with a header row, without retaining earlier query
results, and reports argument, input, SQL, storage, and output failures on
stderr with a nonzero status:

```bash
printf "%s\n" \
  "CREATE TABLE events (id Int64, label String);" \
  "INSERT INTO events VALUES (1, 'one, quoted');" \
  "SELECT * FROM events;" \
  | cargo run -- --format csv
```

<!-- burner-progress:start -->
## Burner evaluation progress

![Burner evaluation progress](docs/burner-evaluation-progress.svg)

_Updated automatically on every Burner merge. [Raw history](docs/burner-evaluation-history.json)._
<!-- burner-progress:end -->
