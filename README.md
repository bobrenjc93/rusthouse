# RustHouse

RustHouse is a from-scratch analytical database in Rust, inspired by the useful core of ClickHouse: typed columnar data, fast scans and aggregations, a practical SQL surface, and an operational interface that is easy to embed and understand.

This is intentionally not a wire-compatible ClickHouse clone. The current vertical slice is an in-memory engine intended for analytical SQL batches and embedding.

## Quick start

RustHouse reads a complete SQL batch from standard input. DDL and inserts are silent; every `SELECT` emits its own header and rows.

```bash
cargo run -- --format csv <<'SQL'
CREATE TABLE events (
    category String,
    value Float64,
    active Bool,
    note Nullable(String)
);
INSERT INTO events VALUES
    ('hardware', 12.5, true, NULL),
    ('software', 7.0, false, 'trial'),
    ('hardware', 3.5, true, 'repeat');
SELECT category, COUNT(*) AS rows, SUM(value) AS total
FROM events
WHERE active
GROUP BY category
ORDER BY total DESC
LIMIT 10;
SQL
```

Output:

```csv
category,rows,total
hardware,2,16
```

`--format` accepts `table`, `csv`, or `json`. CSV output follows conventional quoting and represents `NULL` as an empty field. JSON output is one document containing a `{ "columns": [...], "rows": [[...]] }` object per `SELECT`, preserving duplicate projection names. Run `cargo run -- --help` for resource-limit flags.

## SQL and storage

- Columnar in-memory tables with `Int64`, `Float64`, `Bool`, `String`, and `Nullable(...)` columns.
- `CREATE TABLE` and atomic batched `INSERT INTO ... VALUES`, including explicit insert column lists.
- `SELECT` projections and aliases, arithmetic, comparisons, SQL three-valued boolean predicates, `IN`, `BETWEEN`, `LIKE`, casts, and a small scalar-function set.
- `COUNT`, `SUM`, `MIN`, `MAX`, and `AVG`, with `DISTINCT`, `GROUP BY`, and `HAVING`.
- Multi-column `ORDER BY` with aliases, ordinals, and explicit null placement, plus `LIMIT`.

The engine deliberately rejects unsupported syntax with typed errors. Joins, subqueries, windows, persistence, transactions spanning statements, and concurrent service access are not implemented.

Embedding starts with `rusthouse::Engine`. `EngineConfig` bounds SQL bytes, rows per insert, rows per table, emitted rows per query, and retained or intermediate query bytes. Before building an AST, parsing also rejects batches over 500,000 tokens, 1,024 statements, or 1,024 projections per `SELECT`, recursive SQL shapes over 1,024 tokens along an expression path, and more than 128 nested delimiters. LIKE uses linear matching for literal segments and a fixed work bound for segments containing `_`. The byte budget covers expression results, filtering, grouping, aggregate and `DISTINCT` state, projected rows, and sort keys. `LIMIT 0` returns after binding without scanning, and plain unordered queries stop retaining scan candidates at their limit; ordered queries fail at the byte limit if their sort candidates cannot be retained safely. `execute_iter` yields one statement result at a time; the CLI uses it so multi-statement batches do not retain prior results. The collecting `execute` API gives each query only its remaining cumulative byte budget and preflights command-result capacity before `CREATE` or `INSERT` mutations. Each insert batch is validated before any column is changed.

## Product target

Longer-term product targets include:

- durable local snapshots with an explicit, documented file format;
- an HTTP endpoint for executing SQL;
- larger-scale execution with explicit memory accounting.

The early implementation should favor Rust's standard library and a small dependency surface. Correctness, clear errors, bounded resource use, and a modular path toward vectorized execution matter more than superficial feature count.

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

```bash
cargo test
cargo run -- --help
```

The deterministic boundary tests and randomized semantic tests run with `cargo test`.

The `analytical` Criterion benchmark generates 50,000 deterministic rows and measures the supported grouped aggregate query shape. Save a local baseline, then compare later changes against it:

```bash
cargo bench --bench analytical -- --save-baseline main
cargo bench --bench analytical -- --baseline main
```
