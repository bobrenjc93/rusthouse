# RustHouse

RustHouse is a small in-memory analytical database written in Rust. It stores
each field in a typed column vector and provides both an embedding API and a
stdin/stdout SQL CLI.

## SQL surface

RustHouse supports:

- `CREATE TABLE` with `Int64`, `Float64`, `Bool`, and `String` columns;
- multi-row `INSERT INTO ... VALUES`, including explicit column order;
- `SELECT` projections, aliases, `*`, arithmetic, and comparisons;
- `WHERE` expressions with parentheses, `AND`, `OR`, and `NOT`;
- `COUNT`, `SUM`, `MIN`, `MAX`, and `AVG` with `GROUP BY`;
- multi-key `ORDER BY` with `ASC`/`DESC`; and
- `LIMIT`, `LIMIT ... OFFSET ...`, and `LIMIT offset, count`.

Identifiers can be unquoted, double quoted, or backtick quoted. Unquoted SQL
keywords and identifiers are case-insensitive; identifier matching uses full
non-Turkic Unicode case folding. Quoted identifiers preserve exact case, so
`"CaseName"` and `"casename"` are distinct. The four storage types are non-nullable;
`NULL` is rejected instead of being silently coerced. `COUNT` over an empty
input returns zero. `SUM`, `MIN`, `MAX`, and `AVG` over an empty input return a
typed `EmptyAggregate` error because the engine cannot represent SQL `NULL`.

## CLI

Pass one or more semicolon-delimited statements on standard input. DDL and
inserts do not print status text. Every query result is emitted as CSV with a
header row.

```bash
printf '%s\n' \
  'CREATE TABLE readings (site String, value Float64);' \
  "INSERT INTO readings VALUES ('west', 2.5), ('east', 4.0);" \
  'SELECT site, AVG(value) AS mean FROM readings GROUP BY site ORDER BY site;' \
  | cargo run --quiet -- --format csv
```

Use `cargo run -- --help` for the complete CLI syntax. A downstream consumer
closing stdout early is treated as a normal broken-pipe termination.

## Embedding

[`Database::execute`](src/database.rs) parses and executes multiple statements
in order, returning one typed `ExecutionResult` per statement.

```rust
use rusthouse::{Database, ExecutionResult, Value};

let mut database = Database::new();
database.execute("CREATE TABLE facts (id Int64, active Bool);")?;
database.execute("INSERT INTO facts VALUES (1, true), (2, false);")?;
let result = database.execute_one("SELECT COUNT(*) AS n FROM facts")?;

let ExecutionResult::Query(result) = result else { unreachable!() };
assert_eq!(result.rows[0][0], Value::Int64(2));
# Ok::<(), rusthouse::DatabaseError>(())
```

`Database::with_limits` configures input bytes, request tokens and statements,
expression depth and node count, rows per insert, rows per table, per-statement
and cumulative request result rows/bytes, intermediate rows/bytes, columns per
table, and bytes per string value (including query literals). Limit failures,
parse failures, catalog errors, type errors, and
arithmetic failures are distinct `DatabaseError` variants. An insert batch is
fully evaluated and validated before any column is changed. Unordered queries
apply `OFFSET`/`LIMIT` before projection; ordered queries retain a bounded top-k
working set. Projection and ordering budgets are charged after each scalar, so
a single wide row cannot bypass byte limits. Expression depth is hard-capped at 64 to keep parser and evaluator
stack use safe even when a caller supplies a larger configured value. The
separate node-count bound limits expression memory without treating balanced
trees as deeply nested. It is hard-capped at 256 nodes so caller overrides
cannot construct an AST whose recursive destruction would exhaust the stack.
The column limit applies to physical table schemas, not scalar `SELECT`
results.

Public catalog helpers mirror SQL identifier rules. `schema` and
`table_row_count` reject names whose exact-quoted and folded-unquoted meanings
select different tables; `_quoted` and `_unquoted` variants choose explicitly.
`Schema` provides the same behavior through `resolve_column_index`,
`column_index_quoted`, and `column_index_unquoted`.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

The implementation is currently single-process and in-memory: persistence,
server protocols, joins, windows, and nullable types are not implemented. Its
only runtime dependency is a focused Unicode case-folding table.
