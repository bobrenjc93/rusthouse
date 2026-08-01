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

## Immutable segments

`storage::segment` provides the first durable columnar format. Version 1 files contain a checksummed fixed header, schema, and block directory followed by contiguous column blocks. Blocks are grouped by row range and carry null counts and exact min/max zone maps. Integer blocks use delta bit packing with a plain fallback for extreme deltas, booleans use validity and value bitmaps, and strings use a front-coded UTF-8 buffer.

Readers verify header metadata before allocating, enforce configurable limits for files, metadata, rows, columns, blocks, decoded buffers, and strings, and verify each block checksum when that block is read. A predicate scan consults zone maps first, so pruned blocks are neither decoded nor checksummed. `write_segment` uses create-new semantics, syncs completed contents, and never replaces an existing segment.

The format is intentionally self-contained and little-endian. Readers reject unknown versions, unknown encodings, non-canonical or overlapping block extents, invalid UTF-8, inconsistent null maps or statistics, non-zero padding, trailing data, and arithmetic overflow.

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo run -- --help
```

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.
