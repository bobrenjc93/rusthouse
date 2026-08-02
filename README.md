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

## Current SQL surface

The CLI and `Database` API execute batches containing scalar `SELECT`,
`SELECT COUNT(*) [AS alias] FROM table`, `CREATE TABLE name (field type, ...)`,
and schema-ordered `INSERT INTO name VALUES (...), (...)` statements. Scalar
expressions support literals and same-type `=`, `<>`, `<`, `<=`, `>`, or `>=`
comparisons, with SQL NULL propagation. `COUNT(*)` returns the row count of a
stored table, including rows inserted earlier in the same batch.
Table fields support `Int64`, `Float64`, `Bool`, and `String`. Plain types are
non-nullable, while `Nullable(Int64)`, `Nullable(Float64)`, `Nullable(Bool)`,
and `Nullable(String)` accept `NULL`. DDL and inserts produce no CSV output,
and a failing statement rolls back the complete SQL batch.

Catalog tables are in-memory and each is capped at 1,000,000 rows. The public
catalog API exposes immutable table lookup, and persistence is not implemented.
SQL inserts use the lower-level `Table` API's atomic positional batch append.

Storage resource limits are measured in UTF-8 bytes: a schema may contain at
most 1,024 fields, each field identifier may contain at most 256 bytes, and one
stored `String` value may contain at most 1,048,576 bytes. `Schema::new` and the
atomic table append APIs report typed errors when these limits are exceeded.
Each table also has a 256 MiB aggregate column-data budget; callers using the
public storage API can select a lower bound with `Table::with_data_limit`.

Each SQL batch is limited to 32 MiB (33,554,432 UTF-8 bytes) and 10,000
statements, whether submitted through the CLI or directly through
`Database::execute`.

## Usage

Execute SQL through the CLI:

```bash
printf '%s\n' \
  'CREATE TABLE events (id Int64, note Nullable(String));' \
  "INSERT INTO events VALUES (1, NULL), (2, 'ready');" \
  'SELECT COUNT(*) AS event_count FROM events;' \
| cargo run --locked --quiet -- --format csv
```

Run the equivalent embedded API example:

```bash
cargo run --locked --example database
```

Both commands print:

```csv
event_count
2
```

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

```bash
cargo test
cargo run -- --help
```

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.

<!-- burner-progress:start -->
## Burner evaluation progress

![Burner evaluation progress](docs/burner-evaluation-progress.svg)

_Updated automatically on every Burner merge. [Raw history](docs/burner-evaluation-history.json)._
<!-- burner-progress:end -->
