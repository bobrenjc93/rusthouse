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

## Batch CLI

The command reads UTF-8 SQL from stdin, one statement per nonempty line. The
lines share one in-memory catalog for the life of the process:

```bash
printf '%s\n' \
  'CREATE TABLE events (id Int64, active Bool)' \
  'INSERT INTO events VALUES (1, true), (2, false)' \
  'SELECT active, id FROM events WHERE id >= 2' \
  | cargo run --quiet -- --format csv
```

`CREATE TABLE` and `INSERT INTO ... VALUES` are silent on success. Each
`SELECT` writes its projected header and selected rows to stdout as
CSVWithNames. `--format csv` is accepted for ClickHouse-style invocations and
is also the default. Run `cargo run -- --help` for the input limits and stable
exit-code contract.

One table can be carried across invocations with a bounded snapshot. The load
happens before stdin is processed, and the atomic save happens only after every
statement and output write in the batch succeeds:

```bash
printf '%s\n' \
  'CREATE TABLE events (id Int64)' \
  'INSERT INTO events VALUES (1), (2)' \
  | cargo run --quiet -- --save-table events=events.snapshot

printf '%s\n' 'SELECT id FROM restored ORDER BY id' \
  | cargo run --quiet -- --load-table restored=events.snapshot
```

Each option may be supplied at most once. The snapshot payload limit is 64 MiB;
these options intentionally do not persist manifests, discovery metadata, a
whole catalog, or a write-ahead log. Snapshot file names matching `.*.tmp` or
`.*.lock` are reserved for atomic-writer sidecars.

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.

<!-- burner-progress:start -->
## Burner evaluation progress

![Burner evaluation progress](docs/burner-evaluation-progress.svg)

_Updated automatically on every Burner merge. [Raw history](docs/burner-evaluation-history.json)._
<!-- burner-progress:end -->
