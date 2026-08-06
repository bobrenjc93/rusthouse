# RustHouse

RustHouse is a from-scratch analytical database in Rust, inspired by the useful core of ClickHouse: typed columnar data, fast scans and aggregations, a practical SQL surface, and an operational interface that is easy to embed and understand.

This is intentionally not a wire-compatible ClickHouse clone. The goal is to grow a credible small competitor through measured, reviewed iterations while keeping the implementation approachable.

## Product target

The first useful release should support:

- typed tables with `Int64`, `Float64`, `Bool`, and `String` columns;
- a genuinely columnar in-memory representation;
- `CREATE TABLE`, `INSERT INTO ... VALUES`, and `SELECT`;
- projections, `WHERE` comparisons, `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`, `GROUP BY`, `ORDER BY`, and `LIMIT`;
- a batch/interactive CLI with readable table, CSV, TSV, and JSON output;
- durable local snapshots with an explicit, documented file format;
- an HTTP endpoint for executing SQL;
- deterministic tests and a small benchmark that demonstrate analytical behavior.

The early implementation should favor Rust's standard library and a small dependency surface. Correctness, clear errors, bounded resource use, and a modular path toward vectorized execution matter more than superficial feature count.

## SQL execution

The semicolon-delimited batch engine in `rusthouse::batch` supports typed,
multi-column `Int64`, `Float64`, `Bool`, and `String` tables. It executes
multi-row `INSERT INTO ... VALUES`, typed projections and comparisons,
`COUNT`, `SUM`, `MIN`, `MAX`, and `AVG`, plus `GROUP BY`, multi-column
`ORDER BY`, and `LIMIT`. Grouped results can be filtered by comparing a unique
projected numeric aggregate alias to a finite `Int64` or `Float64` threshold
in `HAVING`. This includes `COUNT`, numeric `SUM`, numeric `MIN` and `MAX`, and
`AVG`. Empty `SUM`, `MIN`, `MAX`, and `AVG` results are `NULL` and do not
satisfy a `HAVING` predicate.
String literals escape a quote by doubling it, so semicolons and line breaks
inside literals do not split a batch.

Literal-only queries use `SELECT <literal> [AS <alias>]` and return one typed
column with one row. `Int64` literals are optionally signed base-10 integers,
such as `-7`; `Float64` literals are optionally signed, finite decimal or
scientific forms containing a decimal point or exponent, such as `+2.5` or
`6.25e1`; and `Bool` literals are case-insensitive `TRUE` or `FALSE`. `String`
literals are single-quoted and escape a quote by doubling it, as in
`SELECT 'it''s ready' AS message`. This form accepts exactly one literal
expression and an optional `AS` alias: expression lists, `NULL`, operator
expressions, `FROM`, and other trailing clauses are not supported.

`SHOW TABLES` returns the catalog's display names in deterministic,
case-insensitive order as one `String` column.
`SHOW CREATE TABLE <name>` returns one canonical `CREATE TABLE` statement as a
bounded `String`, preserving the stored table and column display names and
schema order while normalizing type spellings.
`DESCRIBE TABLE <name>` returns the table's columns in schema order as `name`
and `type` `String` columns. It uses case-insensitive table lookup and applies
the normal result row, value, and byte limits before allocating result storage.
Two existing `SELECT` queries can be combined with `UNION ALL`. Their rows are
concatenated left-first, the left query supplies the result column names, and
both operands must return the same number and sequence of column types. Each
operand applies its own clauses; nested unions and union-level outer clauses
are not supported. The combined result remains subject to the normal query
result limits before its row vector is grown.
`SELECT * FROM left_table CROSS JOIN right_table [LIMIT n]` returns every
typed column from the left table followed by every column from the right, with
rows in deterministic left-major order. This deliberately narrow form does
not accept projections, predicates, aliases, or additional joins. The
LIMIT-reduced Cartesian row, scalar-value, and estimated byte counts are all
checked before result rows are materialized.
`SELECT DISTINCT column [, ...] FROM table [WHERE predicate] [LIMIT n]`
supports tuples of physical columns of any supported types and the same typed,
composable comparison predicates as regular `SELECT`. Rows are filtered before
unique tuples are retained in deterministic first-seen order. Distinct tuples
are collected under the grouped-query cap before `LIMIT` is applied, and the
limited output remains subject to the normal result caps.
Empty aggregate inputs produce one row: `COUNT` is zero and `SUM`, `MIN`,
`MAX`, and `AVG` are typed `NULL` values.

`SELECT` projections support `CAST(int64_column AS Float64)` and
`CAST(float64_column AS Int64)`. Float-to-integer casts truncate finite values
toward zero and report typed numeric-overflow errors outside the `Int64`
range. Add an explicit `AS alias`; otherwise, the result column is named
`CAST(<column> AS <type>)`. `WHERE`, `ORDER BY`, and `LIMIT` select rows before
checked conversion. `CAST` projections are currently limited to ungrouped
queries: they cannot be combined with aggregate projections or `GROUP BY`.
`LENGTH(string_column)` is another ungrouped scalar projection and returns the
string's UTF-8 byte length as `Int64` without allocating a transformed string.
It accepts an optional `AS alias`; otherwise, the result column is named
`LENGTH(<column>)`. `WHERE` filters source rows before evaluation, and the
unaliased expression can be ordered with `ORDER BY LENGTH(<column>)`; aliased
projections can be ordered by their alias. Both forms support `LIMIT`.
Non-`String` arguments and byte lengths outside the `Int64` range are reported
as typed errors.
`ABS(int64_column)` is an ungrouped scalar projection that returns a checked
`Int64` absolute value. It supports an optional `AS alias`, ordering by the
unaliased expression or alias, `WHERE`, and `LIMIT`. Filtering and limiting
select rows before output evaluation, so an excluded `i64::MIN` does not fail
the query; a selected `i64::MIN` reports a typed numeric-overflow error.

`ROW_NUMBER() OVER ()` adds a one-based `Int64` sequence to an ungrouped,
non-`DISTINCT` projection and accepts an optional `AS alias`. The ordered form
`ROW_NUMBER() OVER (ORDER BY int64_column ASC|DESC)` filters with `WHERE`, then
orders equal keys by stable source position and numbers rows before `LIMIT`.
The empty window retains source order. These minimal window forms deliberately
reject arguments, partitioning, multiple or implicit-direction window sort
keys, aggregate projections, `GROUP BY`, `HAVING`, and query-level `ORDER BY`;
their output is covered by the normal result row, value, and byte caps.

RustHouse's bounded in-memory `Catalog` parses and executes a one-column `Int64`
subset covering `CREATE TABLE`, single-row `INSERT INTO ... VALUES`, and
`SELECT` projections across multiple named tables. `SELECT` supports nullable
`Int64` predicates through `WHERE column operator literal`, where `operator` is
`=`, `!=`, `<>`, `<`, `<=`, `>`, or `>=`, as well as `WHERE column IS NULL` and
`WHERE column IS NOT NULL`. Both forms accept an optional nonnegative `LIMIT`.
Scalar `SELECT COUNT(*)`, `SELECT COUNT(column)`, and `SELECT SUM(column)`
support the same comparison filters with explicit scan and aggregate row
bounds while preserving SQL `NULL` semantics. `SELECT MIN(column) FROM table`
provides the bounded unfiltered minimum and returns `NULL` for empty or
all-`NULL` input.
The explicit
`ORDER BY column ASC|DESC NULLS FIRST|LAST LIMIT n` form uses a bounded top-k
operator and materializes rows in stable order. Plain projections borrow a
prefix of the table's column storage;
filtered projections return matching values in source order through bounded
comparison and nullness scans. `SELECT DISTINCT column FROM table` uses
explicit input-row and distinct-value limits and returns deterministic
`NULL`-first, ascending values.
The same catalog exposes narrow one-column equi-joins. `INNER JOIN` projects
the left column for matching rows. `SELECT right_column FROM left_table LEFT
JOIN right_table ON left_column = right_column` projects matching right values
and a typed `NULL` for each unmatched left row. Both forms preserve duplicate
cross-products in deterministic left-major order and enforce explicit input
and output bounds.

## Command-line session

`rusthouse --format table`, `rusthouse --format csv`, `rusthouse --format tsv`,
and `rusthouse --format json` read one complete SQL batch from standard input
through EOF, with explicit limits of 64 MiB and 4,096 statements. Parsing is
lazy and bounds all `INSERT` ASTs in a batch to 100,000
rows and 1,000,000 scalar values. A separate cumulative 100,000-item limit
covers `CREATE` columns plus `SELECT`, `GROUP BY`, and `ORDER BY` lists, so
compact input cannot expand into an unbounded retained token or AST graph.
Every statement shares one in-memory catalog. Successful `CREATE`, `DROP`,
`TRUNCATE`, and `INSERT` statements are silent, and each `SELECT`, `SHOW
TABLES`, `SHOW CREATE TABLE`, or `DESCRIBE TABLE` query is executed and emitted
before the next statement. Table output uses
bordered, human-readable columns, escapes control characters, renders SQL
`NULL` as `NULL`, and separates multiple query results with a blank line. Each
padded table is size-checked against a 16 MiB formatted-output limit before
being streamed, so a wide cell cannot amplify many short rows into unbounded
memory or output. CSV output uses a CSVWithNames-compatible header followed by
typed rows; commas, quotes, and newlines in strings are CSV-escaped. JSON output
is newline-delimited, with one compact object per query containing typed column
metadata and positional rows. Numbers and booleans use native JSON values, SQL
`NULL` becomes `null`, and strings are JSON-escaped.
TSV output follows ClickHouse's `TabSeparatedWithNames` shape: every result has
an escaped header and typed rows, SQL `NULL` is `\N`, and backslashes, tabs,
carriage returns, line feeds, NUL, backspace, form feed, and apostrophes in
column names and strings use ClickHouse's backslash escapes.
A query result is checked before cloning against limits of 10,000 rows, 250,000
values, and an estimated 16 MiB. Grouped queries additionally allow 100,000
groups and bound grouped keys to 500,000 cells and an estimated 32 MiB. Their
grouped-key accounting includes the reusable lookup probe for tuples wider
than two columns. Aggregate working state has separate 500,000-cell and
estimated 32 MiB limits, including cloned string extrema. The collecting
library API separately caps all retained query results at an estimated 64 MiB.
Typed batch tables also retain at most 1,000,000 rows each by default.
`Database::with_max_rows_per_table` and the matching `SharedDatabase`
constructor configure this per-table cap; an oversized `INSERT` is rejected
atomically before any of its rows are appended, and `TRUNCATE TABLE` restores
the table's full capacity.

Running `rusthouse` without options retains the legacy line-oriented `Int64`
session. It reads one statement from each nonempty input line and prints a row
list such as `[7, NULL, -2]` for each projection. That session allows 65,536
input bytes, 1,024 statements, 64 tables, and 1,024 rows per table. In either
mode, malformed or failed SQL is reported on standard error and exits nonzero.

```bash
printf '%s\n' \
  "CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);" \
  "INSERT INTO metrics VALUES (1, 2.5, true, 'alpha'), (2, 4.0, false, 'beta');" \
  "SELECT COUNT(*) AS rows, AVG(score) AS mean FROM metrics;" |
  cargo run -- --format json
```

Use `--format csv` instead to emit the same query results as CSVWithNames.
Use `--format tsv` for ClickHouse-style TabSeparatedWithNames output.
Use `--format table` for bordered output intended for direct terminal reading.

For concurrent in-process access, `SharedCatalog` wraps a catalog in an
`Arc<RwLock<Catalog>>`. Cloned handles serialize `CREATE`, `INSERT`, and CSV
ingestion with a write lock, allow `SELECT` operations through read locks, and
return owned projection rows. Existing catalog failures remain typed, and lock
poisoning is reported separately.

`SharedDatabase` provides the same synchronization for the typed batch SQL
engine. Its `query` method accepts exactly one `SELECT`, `SHOW TABLES`, `SHOW
CREATE TABLE`, or `DESCRIBE TABLE`, takes a shared read lock, and returns an
owned, resource-bounded result, so cloned handles can run analytical reads
concurrently. Mutating batches passed to
`execute` retain one write lock for the entire batch and cannot interleave.
For transactional ingestion, `Database::execute_insert_batch` and the matching
`SharedDatabase` method accept a nonempty `INSERT`-only batch, preflight every
statement and cumulative per-table row cap, then commit in statement order.
Any validation or resource failure leaves all tables unchanged; the shared
form retains one write lock across preflight and commit.
Read-only API misuse and lock poisoning are reported as distinct typed errors.

## HTTP query exchange

`handle_http_query` handles one transport-neutral `Read`/`Write` HTTP/1.1
exchange without opening a listener. It accepts exactly `POST /query`, requires
a nonempty `Host` and one decimal `Content-Length`, rejects transfer encoding
(including chunked requests), and returns `417 Expectation Failed` for
`Expect` instead of waiting for a body whose sender may be awaiting an interim
response. It sends the UTF-8 SQL body through `SharedDatabase::query`.
Successful responses use the same compact JSON column metadata and
positional-row shape as `--format json`; protocol and query failures return
deterministic JSON error objects with an appropriate HTTP status.

The default limits are 16 KiB and 64 fields for request headers, 1 MiB for the
SQL body, and 16 MiB for the complete response including headers. The full
response is prepared and checked before anything is written. Call
`handle_http_query_with_limits` with `HttpQueryLimits` to set smaller embedding
limits. This API deliberately owns only one exchange; listener, connection,
timeout, and shutdown lifecycle remain the embedding application's concern.

The typed engine's `Database::ingest_csv_with_names` API atomically appends a
bounded, multi-column `CSVWithNames` subset to an existing batch table. Its
header must exactly match every schema column in order and case. Data fields
parse according to the table's `Int64`, finite `Float64`, `Bool`, and `String`
types, and callers provide complete-input byte, row, and total-value limits.
Boolean fields are the exact lowercase tokens `true` and `false`. Both LF and
CRLF records are accepted. A `String` data field may be double-quoted so it can
contain commas and LF or CRLF line endings, and doubled quotes inside it decode
to one quote (for example, `"say ""hello"""`). Embedded line endings are
retained exactly. Headers and non-`String` fields must remain unquoted, and
malformed quoting is rejected. Any input, schema, value, limit, or
remaining-capacity failure leaves the table unchanged.

## Snapshot envelope

`SnapshotCodec` encodes and validates bounded byte payloads using an explicit
magic value, format version, declared length, and CRC-32 checksum.
`NullableI64PayloadCodec` provides the first deterministic storage payload: a
bounded row count and tagged nullable `Int64` values.
`restore_int64_table_from_file` reopens one of these files with a hard envelope
read bound and restores a table only after the envelope, payload, schema, and
row cap have all been validated. An explicit-backup helper tries that same
bounded restore against a caller-supplied backup only when the primary fails,
and preserves both typed failures if neither file is valid.
On Unix, `SnapshotCodec::replace_file` atomically creates or replaces an
envelope through an exclusively created, synchronized sibling temporary file,
then synchronizes the parent directory. Directory-relative operations remain
anchored to the opened parent even if its path is renamed or rebound. Typed
stage errors clean up failures before the rename and separately report
post-rename directory-sync uncertainty. The API is not exposed on Windows
because RustHouse does not yet implement the required directory-handle and
flush semantics there.
`Catalog::restore_int64_table_from_file` registers a validated table under a
caller-supplied exact name while also enforcing the catalog's table-count and
per-table row limits. These define the current persistence corruption boundary
without yet choosing catalog serialization. The
exact layouts are documented in [docs/snapshot-format.md](docs/snapshot-format.md).

## CSV ingestion

`ingest_csv_with_names` atomically appends a bounded one-column
`CSVWithNames` subset to an `Int64Table`. The header must exactly match the
schema column, and each LF- or CRLF-delimited record must be an unquoted decimal
`Int64` or `NULL`. Callers supply byte and row limits; format, limit,
nullability, or table-cap failures leave the table unchanged.
`ingest_csv_with_names_from_reader` accepts an `std::io::Read`, consumes at most
the byte limit plus one detection byte, and reports read failures separately
from oversized or invalid CSV. It buffers the complete bounded input before
using the same transactional parser, so every failure leaves existing rows
unchanged.
`Catalog::ingest_csv_with_names` exposes the same transactional ingestion by
exact table name without requiring direct access to catalog-owned tables;
`Catalog::ingest_csv_with_names_from_reader` resolves the exact table before
consuming the bounded reader and preserves the reader importer's typed errors.
`SharedCatalog::ingest_csv_with_names` provides the synchronized equivalent.

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

```bash
cargo test
cargo run -- --help
```

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.

<!-- burner-progress:start -->
## Burner evaluation progress

![Burner evaluation progress](docs/burner-evaluation-progress.svg)

_Updated automatically on every Burner merge. [Raw history](docs/burner-evaluation-history.json)._
<!-- burner-progress:end -->
