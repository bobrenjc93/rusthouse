# RustHouse

RustHouse is a from-scratch analytical database in Rust, inspired by the useful core of ClickHouse: typed columnar data, fast scans and aggregations, a practical SQL surface, and an operational interface that is easy to embed and understand.

This is intentionally not a wire-compatible ClickHouse clone. The goal is to grow a credible small competitor through measured, reviewed iterations while keeping the implementation approachable.

## Product target

The current in-memory vertical slice supports:

- typed tables with `Int64`, `Float64`, `Bool`, `String`, and `Nullable(T)` columns;
- a genuinely columnar in-memory representation;
- `CREATE TABLE`, `INSERT INTO ... VALUES`, and `SELECT`;
- atomic multi-row inserts, including optional insert column lists;
- projections and aliases, arithmetic, comparisons, three-valued Boolean predicates, and `IS NULL`;
- `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`, `GROUP BY`, `HAVING`, and `DISTINCT`;
- multi-key `ORDER BY`, explicit NULL placement, `LIMIT`, and `OFFSET`;
- a bounded stdin CLI with CSV and JSON output;

Durable snapshots, an HTTP service, parallel execution, and joins remain future work:

- durable local snapshots with an explicit, documented file format;
- an HTTP endpoint for executing SQL;
- parallel scans and aggregation.

## Command line

Build or run the binary, then pass a semicolon-delimited script on standard input. DDL and inserts do not emit output; every `SELECT` emits a result in statement order.

```bash
printf '%s\n' "\
CREATE TABLE events (id Int64, category String, amount Nullable(Float64));
INSERT INTO events VALUES (1, 'a', 3.5), (2, 'a', NULL), (3, 'b', 2.0);
SELECT category, count(*) AS n, sum(amount) AS total
FROM events GROUP BY category ORDER BY total DESC;" \
  | cargo run --quiet -- --format csv
```

`--format csv` is the default and emits headerless RFC-style records. NULL is `\N`. `--format json` emits one JSON array of typed row objects per query and requires unique output names. Input, materialized query results, and encoded output are each capped at 64 MiB; token and statement counts, expression depth, table cells, and table storage also have explicit limits so compact adversarial input fails cleanly.

```bash
cargo run -- --help
```

## SQL notes

Unquoted names and SQL keywords are case-insensitive. Strings use single quotes and escape a quote by doubling it (`'it''s'`). Aggregates ignore NULL values except `COUNT(*)`; a comparison involving NULL is unknown and therefore does not pass `WHERE` or `HAVING`. INSERT validates a complete batch before changing any column.

The SQL surface is intentionally focused. It does not currently include joins, subqueries, casts, UPDATE/DELETE, persistent tables, or server protocols.

## Development

The crate has no third-party dependencies. Run the complete validation suite with:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## Product direction

Longer-term goals include:

- a readable table output mode;
- durable local snapshots with an explicit, documented file format;
- an HTTP endpoint for executing SQL;
- deterministic tests and a small benchmark that demonstrate analytical behavior.

The early implementation should favor Rust's standard library and a small dependency surface. Correctness, clear errors, bounded resource use, and a modular path toward vectorized execution matter more than superficial feature count.

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.
