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

## Bulk CSV and NDJSON

The library exposes query-independent typed storage and streaming bulk formats. A `Schema` defines ordered `Int64`, `Float64`, `Bool`, and `String` fields and whether each field accepts `NULL`. `CsvBatchReader` and `NdjsonBatchReader` produce rectangular `ColumnBatch` values without retaining the complete input.

```rust
use rusthouse::formats::{CsvOptions, export_ndjson, ingest_csv};
use rusthouse::{DataType, Field, Schema, Table};
use std::io::Cursor;

fn main() -> Result<(), Box<dyn std::error::Error>> {
let schema = Schema::new(vec![
    Field::new("id", DataType::Int64, false),
    Field::new("name", DataType::String, true),
])?;
let mut table = Table::new(schema);
ingest_csv(
    Cursor::new(b"id,name\n1,Ada\n2,\\N\n"),
    &mut table,
    CsvOptions::default(),
)?;

let mut output = Vec::new();
export_ndjson(&mut output, &table)?;
Ok(())
}
```

The conversion rules are deliberately explicit:

- CSV headers, when enabled, must exactly equal the schema names in schema order. Records use RFC 4180 quoting, including doubled quotes and embedded line endings.
- The default CSV `NULL` is an exact, unquoted `\N`. A quoted `"\N"` is the string `\N`, and an empty field is an empty string rather than `NULL`. The token is configurable.
- CSV integers and floats must consume the complete field with no surrounding whitespace. Floats must be finite. Booleans are exactly lowercase `true` or `false`.
- Each nonblank NDJSON line must be one JSON object with every schema field exactly once. Field order is irrelevant, but extra, duplicate, and missing fields are errors.
- NDJSON uses JSON scalars without implicit coercion: JSON numbers feed numeric columns, JSON booleans feed `Bool`, JSON strings feed `String`, and only literal `null` produces `NULL`. Nested values are rejected by scalar schemas.
- Invalid UTF-8, non-finite floats, conversion failures, and `NULL` in non-nullable fields are typed errors. CSV and NDJSON exporters apply the inverse escaping rules and preserve schema order.

`FormatLimits` independently bounds total input bytes, rows, fields per record, decoded field bytes, JSON nesting depth, decoded string bytes, record bytes, and rows per batch. Parsing retains one bounded record and one typed batch. The `ingest_csv` and `ingest_ndjson` helpers write validated batches to a private temporary spool first, then replay them into the table; a parse, limit, staging, or replay error leaves the destination at its original row count. Applications that consume the batch iterators directly own any already-consumed batches themselves.

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo run -- --help
```

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.
