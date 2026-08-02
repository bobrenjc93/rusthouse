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

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

```bash
cargo test
cargo run -- --help
```

## Constant-query CLI

The current executable implements a deliberately bounded first SQL surface. It
accepts semicolon-separated `SELECT` statements whose projections are `Int64`,
`Float64`, `Bool`, or single-quoted String literals. Projections may use
`AS identifier` aliases. Output is CSVWithNames-compatible CSV; tables, other
clauses, expressions, aggregation, and DDL are rejected.

```bash
cargo run -- --execute "SELECT 42 AS answer, 'it''s ready' AS message" --format csv
printf "SELECT true AS ready; SELECT 1.5 AS ratio;" | cargo run -- --format csv
```

Without `--execute`, the command reads standard input through EOF. SQL input is
limited to 1 MiB. Invalid arguments, malformed SQL, invalid UTF-8, and oversized
input produce an error on standard error and a nonzero exit status.

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.
