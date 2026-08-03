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

RustHouse's bounded in-memory `Catalog` parses and executes a one-column `Int64`
subset covering `CREATE TABLE`, atomic multi-row `INSERT INTO ... VALUES`, and
`SELECT` projections across multiple named tables. `SELECT` supports nullable
`Int64` equality predicates through `WHERE column = literal` and an optional
nonnegative `LIMIT`. The explicit
`ORDER BY column ASC|DESC NULLS FIRST|LAST LIMIT n` form uses a bounded top-k
operator and materializes rows in stable order. The exact grouped aggregate
form `SELECT column, COUNT(*) FROM table GROUP BY column` requires explicit
input-row and distinct-group limits and returns `NULL` first, followed by
non-`NULL` keys in ascending order. Plain projections borrow a prefix of the
table's column storage; filtered projections return matching non-`NULL` values
in source order through the bounded comparison scan.

## Snapshot envelope

`SnapshotCodec` encodes and validates bounded byte payloads using an explicit
magic value, format version, declared length, and CRC-32 checksum.
`NullableI64PayloadCodec` provides the first deterministic storage payload: a
bounded row count and tagged nullable `Int64` values. These define the current
persistence corruption boundary without yet choosing catalog serialization or
filesystem replacement. The exact layouts are documented in
[docs/snapshot-format.md](docs/snapshot-format.md).

## CSV ingestion

`ingest_csv_with_names` atomically appends a bounded one-column
`CSVWithNames` subset to an `Int64Table`. The header must exactly match the
schema column, and each LF- or CRLF-delimited record must be an unquoted decimal
`Int64` or `NULL`. Callers supply byte and row limits; format, limit,
nullability, or table-cap failures leave the table unchanged.

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
