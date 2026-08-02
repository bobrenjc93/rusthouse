# RustHouse

[![Rust quality](https://github.com/bobrenjc93/rusthouse/actions/workflows/quality.yml/badge.svg)](https://github.com/bobrenjc93/rusthouse/actions/workflows/quality.yml)

RustHouse is a from-scratch analytical database in Rust, inspired by the useful core of ClickHouse: typed columnar data, fast scans and aggregations, a practical SQL surface, and an operational interface that is easy to embed and understand.

This is intentionally not a wire-compatible ClickHouse clone. The goal is to grow a credible small competitor through measured, reviewed iterations while keeping the implementation approachable.

The current SQL foundation includes a bounded lexer with byte-positioned tokens and errors, plus a typed parser for one `CREATE TABLE` definition. Callers explicitly provide input-byte, token-count, and statement-count limits. A bounded CSV formatter streams named result rows to any `std::io::Write` destination with typed validation and writer errors.

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

The binary currently supports help (`-h`, `--help`) and version (`-V`,
`--version`) output. It does not execute SQL yet. The supported operations are
library APIs: bounded SQL lexing and `CREATE TABLE` parsing, validated columnar
table construction and transactional row-batch insertion, and bounded
streaming CSV formatting.

RustHouse uses Rust 1.85.0, the minimum toolchain supporting edition 2024.
`rustup` reads the checked-in toolchain file and installs rustfmt and Clippy.
Run the same quality checks as CI from the repository root with:

```bash
cargo fmt --all --check && \
  cargo build --all-targets --all-features --locked && \
  cargo clippy --all-targets --all-features --locked -- -D warnings && \
  cargo test --all-targets --all-features --locked && \
  cargo test --doc --all-features --locked && \
  RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps --locked
```

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.

<!-- burner-progress:start -->
## Burner evaluation progress

![Burner evaluation progress](docs/burner-evaluation-progress.svg)

_Updated automatically on every Burner merge. [Raw history](docs/burner-evaluation-history.json)._
<!-- burner-progress:end -->
