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

## Quick start

The bundled example parses `CREATE TABLE`, `INSERT INTO ... VALUES`, and
`SELECT *`, applies them to a catalog, and streams the selected table as CSV:

```rust
use std::error::Error;

use rusthouse::Catalog;
use rusthouse::csv::write_csv;
use rusthouse::sql::{parse_create_table, parse_insert, parse_select};

fn main() -> Result<(), Box<dyn Error>> {
    let mut catalog = Catalog::default();
    catalog.create_table(parse_create_table(
        "CREATE TABLE events (id Int64, label String)",
    )?)?;
    catalog.insert(parse_insert(
        "INSERT INTO events VALUES (1, 'first'), (2, 'with,comma')",
    )?)?;

    let mut output = Vec::new();
    write_csv(catalog.select(parse_select("SELECT * FROM events")?)?, &mut output)?;
    print!("{}", String::from_utf8(output)?);
    Ok(())
}
```

Run it directly from the repository:

```bash
cargo run --quiet --example catalog_csv
```

```csv
id,label
1,first
2,"with,comma"
```

## Command line

The `rusthouse` binary executes a bounded SQL script in one in-memory catalog.
It accepts `CREATE TABLE`, `INSERT INTO ... VALUES`, and `SELECT * FROM ...`;
each SELECT result is streamed to standard output as CSV. Pass a file path, or
omit it (or use `-`) to read from standard input:

```bash
cargo run --quiet -- tests/fixtures/cli_workflow.sql
printf 'CREATE TABLE t (id Int64); INSERT INTO t VALUES (1); SELECT * FROM t;' \
  | cargo run --quiet
```

Input is UTF-8 and bounded to 8 MiB per invocation. Each statement also uses
the parser and table limits documented by the library API. Run
`cargo run --quiet -- --help` for the complete command contract.

## Benchmarks

The dependency-free analytical scan benchmark builds a deterministic,
four-column table and repeatedly scans every typed column while computing
numeric sums, a Boolean count, and String bytes. Its output includes the fixed
seed, workload size, elapsed time, throughput, and a result checksum:

```bash
cargo bench --bench analytical_scan
RUSTHOUSE_BENCH_ROWS=1000000 RUSTHOUSE_BENCH_ITERATIONS=100 \
  cargo bench --bench analytical_scan
```

The environment variables make workload scaling reproducible; compare timing
results only on the same machine and toolchain.

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

RustHouse's minimum supported Rust version (MSRV) is 1.85.0. The checked-in
toolchain file makes `rustup` use that exact release with rustfmt and Clippy.
All RustHouse targets forbid unsafe code.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- --deny warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS="--deny warnings" cargo doc --workspace --all-features --no-deps --locked
```

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.

<!-- burner-progress:start -->
## Burner evaluation progress

![Burner evaluation progress](docs/burner-evaluation-progress.svg)

_Updated automatically on every Burner merge. [Raw history](docs/burner-evaluation-history.json)._
<!-- burner-progress:end -->
