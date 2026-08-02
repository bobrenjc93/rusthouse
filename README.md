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

## Current CLI

The CLI executes a bounded, semicolon-delimited SQL script from standard input.
It supports the current `CREATE TABLE`, multi-row `INSERT INTO ... VALUES`, scalar
`SELECT`, table projection `SELECT` with one optional typed
`WHERE column = literal` filter, and table `COUNT(*)` shapes. Each SELECT is
emitted in source order as ClickHouse-style `CSVWithNames`; command statements
emit nothing.

```bash
printf "CREATE TABLE t (id Int64); INSERT INTO t VALUES (42); SELECT id FROM t;\n" \
  | cargo run -- --format csv
```

## Library DDL

The library can create in-memory catalog tables with
`execute_create_table(&mut catalog, sql)`. The supported DDL shape is exactly
`CREATE TABLE name (column type [, column type ...])`, using `Int64`,
`Float64`, `Bool`, or `String`, with an optional trailing semicolon.

## Library DML

The library can insert one or more typed rows into an existing catalog table with
`execute_insert_values(&mut catalog, sql)`. The supported DML shape is exactly
`INSERT INTO name VALUES (literal [, literal ...]) [, (literal [, literal ...]) ...]`,
using `Int64`, `Float64`, `Bool`, or `String` literals, with an optional trailing
semicolon. All rows in one statement are validated and inserted atomically.

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

## Local quality gates

The checked-in `rust-toolchain.toml` selects Rust 1.85.0, the package's minimum
supported Rust version, and installs its matching `rustfmt` and Clippy
components. With [rustup](https://rustup.rs/) installed, run the same checks as
CI from the repository root:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo test --locked --doc --all-features
```

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.

<!-- burner-progress:start -->
## Burner evaluation progress

![Burner evaluation progress](docs/burner-evaluation-progress.svg)

_Updated automatically on every Burner merge. [Raw history](docs/burner-evaluation-history.json)._
<!-- burner-progress:end -->
