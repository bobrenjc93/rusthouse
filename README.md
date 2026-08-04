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

## SQL execution

The semicolon-delimited batch engine in `rusthouse::batch` supports typed,
multi-column `Int64`, `Float64`, `Bool`, and `String` tables. It executes
multi-row `INSERT INTO ... VALUES`, typed projections and comparisons,
`COUNT`, `SUM`, `MIN`, `MAX`, and `AVG`, plus `GROUP BY`, multi-column
`ORDER BY`, and `LIMIT`. String literals escape a quote by doubling it, so
semicolons and line breaks inside literals do not split a batch.
Empty aggregate inputs produce one row: `COUNT` is zero and `SUM`, `MIN`,
`MAX`, and `AVG` are typed `NULL` values.

RustHouse's bounded in-memory `Catalog` parses and executes a one-column `Int64`
subset covering `CREATE TABLE`, single-row `INSERT INTO ... VALUES`, and
`SELECT` projections across multiple named tables. `SELECT` supports nullable
`Int64` predicates through `WHERE column operator literal`, where `operator` is
`=`, `!=`, `<>`, `<`, `<=`, `>`, or `>=`, as well as `WHERE column IS NULL` and
`WHERE column IS NOT NULL`. Both forms accept an optional nonnegative `LIMIT`.
Scalar `SELECT COUNT(*)`, `SELECT COUNT(column)`, and `SELECT SUM(column)`
support the same comparison filters with explicit scan and aggregate row
bounds while preserving SQL `NULL` semantics. `SELECT MIN(column) FROM table`
provides the bounded unfiltered minimum and returns `NULL` for empty or
all-`NULL` input.
The explicit
`ORDER BY column ASC|DESC NULLS FIRST|LAST LIMIT n` form uses a bounded top-k
operator and materializes rows in stable order. Plain projections borrow a
prefix of the table's column storage;
filtered projections return matching values in source order through bounded
comparison and nullness scans. `SELECT DISTINCT column FROM table` uses
explicit input-row and distinct-value limits and returns deterministic
`NULL`-first, ascending values.

## Command-line session

`rusthouse --format csv` reads one complete SQL batch from standard input
through EOF, with explicit limits of 64 MiB and 4,096 statements. Parsing is
lazy and bounds all `INSERT` ASTs in a batch to 100,000 rows and 1,000,000
scalar values. A separate cumulative 100,000-item limit covers `CREATE`
columns plus `SELECT`, `GROUP BY`, and `ORDER BY` lists, so compact input cannot
expand into an unbounded retained token or AST graph.
Every statement shares one in-memory catalog. Successful `CREATE` and `INSERT`
statements are silent, and each `SELECT` is executed and emitted before the
next statement, using a CSVWithNames-compatible header followed by typed rows;
commas, quotes, and newlines in strings are CSV-escaped. A query result is
checked before cloning against limits of 10,000 rows, 250,000 values, and an
estimated 16 MiB. Grouped queries additionally allow 100,000 groups and bound
aggregate working state to 500,000 cells and an estimated 32 MiB, including
cloned string extrema. The collecting library API separately caps all retained
query results at an estimated 64 MiB.

Running `rusthouse` without options retains the legacy line-oriented `Int64`
session. It reads one statement from each nonempty input line and prints a row
list such as `[7, NULL, -2]` for each projection. That session allows 65,536
input bytes, 1,024 statements, 64 tables, and 1,024 rows per table. In either
mode, malformed or failed SQL is reported on standard error and exits nonzero.

```bash
printf '%s\n' \
  "CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);" \
  "INSERT INTO metrics VALUES (1, 2.5, true, 'alpha'), (2, 4.0, false, 'beta');" \
  "SELECT COUNT(*) AS rows, AVG(score) AS mean FROM metrics;" |
  cargo run -- --format csv
```

For concurrent in-process access, `SharedCatalog` wraps a catalog in an
`Arc<RwLock<Catalog>>`. Cloned handles serialize `CREATE`, `INSERT`, and CSV
ingestion with a write lock, allow `SELECT` operations through read locks, and
return owned projection rows. Existing catalog failures remain typed, and lock
poisoning is reported separately.

## Snapshot envelope

`SnapshotCodec` encodes and validates bounded byte payloads using an explicit
magic value, format version, declared length, and CRC-32 checksum.
`NullableI64PayloadCodec` provides the first deterministic storage payload: a
bounded row count and tagged nullable `Int64` values.
`restore_int64_table_from_file` reopens one of these files with a hard envelope
read bound and restores a table only after the envelope, payload, schema, and
row cap have all been validated. These define the current persistence
corruption boundary without yet choosing catalog serialization or filesystem
replacement. The exact layouts are documented in
[docs/snapshot-format.md](docs/snapshot-format.md).

## CSV ingestion

`ingest_csv_with_names` atomically appends a bounded one-column
`CSVWithNames` subset to an `Int64Table`. The header must exactly match the
schema column, and each LF- or CRLF-delimited record must be an unquoted decimal
`Int64` or `NULL`. Callers supply byte and row limits; format, limit,
nullability, or table-cap failures leave the table unchanged.
`Catalog::ingest_csv_with_names` exposes the same transactional ingestion by
exact table name without requiring direct access to catalog-owned tables;
`SharedCatalog::ingest_csv_with_names` provides the synchronized equivalent.

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
