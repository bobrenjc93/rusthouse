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

## Current storage foundation

The library provides validated schemas and bounded in-memory tables for
`Int64`, `Float64`, `Bool`, and `String`. Rows are validated atomically and
transposed into type-specific columns on insert.

```rust
use rusthouse::{DataType, Field, ScalarValue, Schema, Table};

let schema = Schema::new(vec![
    Field::new("id", DataType::Int64),
    Field::new("name", DataType::String),
])?;
let mut table = Table::new(schema, 10_000);
table.insert([
    ScalarValue::Int64(1),
    ScalarValue::String("Ada".to_owned()),
])?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

Development uses the Rust 1.85.0 toolchain declared in `rust-toolchain.toml`. Rustup installs the matching `rustfmt` and Clippy components automatically. Run the same quality gate as CI with:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo build --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --document-private-items --locked
RUSTDOCFLAGS="-D warnings" cargo test --workspace --doc --all-features --locked
```

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.

RustHouse is licensed under the [MIT License](LICENSE).

<!-- burner-progress:start -->
## Burner evaluation progress

![Burner evaluation progress](docs/burner-evaluation-progress.svg)

_Updated automatically on every Burner merge. [Raw history](docs/burner-evaluation-history.json)._
<!-- burner-progress:end -->

Validate the history contract and checked-in chart with:

```bash
python3 -m unittest discover -s scripts -p 'test_*.py' -v
python3 scripts/render_burner_evaluation_history.py --check
```
