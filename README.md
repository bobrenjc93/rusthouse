# RustHouse

RustHouse is a from-scratch, in-memory analytical database in Rust, inspired by the useful core of ClickHouse: typed columnar data, scans and aggregations, a practical SQL surface, and an interface that is easy to embed and understand.

This is intentionally not a wire-compatible ClickHouse clone. The goal is to grow a credible small competitor through measured, reviewed iterations while keeping the implementation approachable.

## Current SQL surface

The engine stores every table as typed column vectors and supports nullable and non-nullable `Int64`, `Float64`, `Bool`, and `String` columns. A session accepts semicolon-separated:

- `CREATE TABLE`, including `Nullable(T)`, `IF NOT EXISTS`, and `ENGINE = Memory`;
- atomic multi-row `INSERT INTO ... [(columns)] VALUES ...`;
- projections, `*`, aliases, numeric arithmetic, and `DISTINCT`;
- `WHERE` comparisons combined with `AND`, `OR`, `NOT`, and parentheses;
- SQL null comparisons through `IS NULL` and `IS NOT NULL`;
- `COUNT`, `SUM`, `MIN`, `MAX`, and `AVG`, with `GROUP BY` and `HAVING`;
- multi-column `ORDER BY` with `ASC`/`DESC`, aliases, or ordinals;
- `LIMIT n`, `LIMIT offset,n`, and `LIMIT n OFFSET offset`.

The engine has no third-party dependencies. It returns typed lexing, parsing, catalog, type, execution, resource-limit, and I/O errors. Tables are session-local: persistence, transactions across statements, joins, window functions, and a network server are not implemented.

## CLI

Pass a SQL script on standard input. `CREATE` and `INSERT` do not print rows; every `SELECT` emits a CSVWithNames-compatible header and result in statement order.

```bash
cat <<'SQL' | cargo run --quiet -- --format csv
CREATE TABLE readings (sensor String, value Float64, ok Bool);
INSERT INTO readings VALUES ('a', 2.5, true), ('a', 3.5, true), ('b', 9, false);
SELECT sensor, avg(value) AS mean
FROM readings WHERE ok = true
GROUP BY sensor ORDER BY mean DESC;
SQL
```

CSV string fields and names use RFC 4180 quoting, and nulls are written as `\N`. Input is limited to 128 MiB, a catalog to 128 tables, a table to 256 columns and 5,000,000 rows, materialized results and output to 256 MiB, and each expression to 256 nesting levels and 1,024 AST nodes.

## Library

`rusthouse::Database::execute` exposes the same stateful SQL boundary. It returns one `QueryResult` for each `SELECT`; `rusthouse::write_csv` and `rusthouse::CsvWriter` render results.

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo run -- --help
```

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.
