# RustHouse

RustHouse is a small analytical SQL engine written in Rust. It keeps tables in memory and stores each field in a contiguous, typed column (Vec<i64>, Vec<f64>, Vec<bool>, or Vec<String>).

## What works

- CREATE TABLE with Int64, Float64, Bool, and String columns
- multi-row INSERT INTO ... VALUES with row-width and exact type validation
- SELECT * and named projections, with optional AS aliases
- WHERE comparisons using =, !=, <>, <, <=, >, and >=
- AND, OR, and parentheses in predicates (AND binds more tightly)
- COUNT, SUM, MIN, MAX, and AVG
- GROUP BY, output-column or alias ORDER BY with ASC/DESC, and LIMIT
- bounded execution with spill-backed grouping and sorting
- parallel filtered scans and partial aggregation with deterministic result merging
- semicolon-separated SQL batches
- table, CSV, and JSON output from the CLI
- SQL input from --execute or standard input

Identifiers are unquoted and case-insensitive; TRUE and FALSE are reserved Boolean literals and cannot be column names. String literals use single quotes; write a quote inside one as ''.

## CLI

Run a batch directly:

~~~bash
cargo run -- --execute "
  CREATE TABLE sales (region String, amount Int64, online Bool);
  INSERT INTO sales VALUES
    ('west', 10, true),
    ('east', 4, false),
    ('west', 7, true);
  SELECT region, COUNT(*) AS orders, SUM(amount) AS total, AVG(amount) AS mean
  FROM sales
  WHERE online = true
  GROUP BY region
  ORDER BY total DESC
  LIMIT 10;
"
~~~

Choose table (the default), csv, or json:

~~~bash
cargo run -- --format json --execute \
  "CREATE TABLE t (id Int64); INSERT INTO t VALUES (2), (1); SELECT * FROM t ORDER BY id"
~~~

By default, scans use the process's available parallelism. Set an explicit
maximum worker count with `--workers`; small inputs still run on the calling
thread:

~~~bash
cargo run -- --workers 4 --execute \
  "CREATE TABLE t (id Int64); INSERT INTO t VALUES (2), (1); SELECT COUNT(*) FROM t"
~~~

Or pipe a batch through standard input:

~~~bash
printf '%s\n' \
  "CREATE TABLE flags (name String, enabled Bool);" \
  "INSERT INTO flags VALUES ('search', true);" \
  "SELECT * FROM flags;" |
  cargo run -- --format csv
~~~

Command acknowledgements go to stderr so CSV and JSON query data on stdout remain usable in pipelines.
JSON output is always one document with a top-level results array. Each SELECT result contains explicit column name/type metadata and positional row arrays, so multiple SELECT statements and duplicate aliases preserve every value.

## Library API

Database retains an in-memory catalog across calls and returns structured results:

Database parses a complete SQL batch before execution: any syntax error leaves the catalog unchanged. After parsing succeeds, statements execute in order; if a later execution error occurs, earlier successful statements remain applied.

~~~rust
use rusthouse::{Database, StatementResult};

let mut database = Database::new();
database.execute("CREATE TABLE events (id Int64, name String)")?;
database.execute("INSERT INTO events VALUES (1, 'launch')")?;

let results = database.execute("SELECT * FROM events WHERE id = 1")?;
let StatementResult::Query(result) = &results[0] else {
    unreachable!();
};
assert_eq!(result.rows.len(), 1);

# Ok::<(), rusthouse::Error>(())
~~~

## Resource limits

`Database::with_limits` and `Database::set_limits` configure inclusive ceilings
for every SQL batch. The defaults are:

| Resource | Default |
| --- | ---: |
| SQL input | 16 MiB |
| Tokens | 1,000,000 |
| Statements | 10,000 |
| Columns per schema | 65,536 |
| Stored values | 100,000,000 |
| Intermediate rows | 10,000,000 |
| Transient execution memory | 64 MiB |
| Result rows | 1,000,000 |
| Rendered output | 64 MiB |

~~~rust
use rusthouse::format::{OutputFormat, render_with_limit};
use rusthouse::{Database, ExecutionLimits};

let limits = ExecutionLimits {
    max_memory_bytes: 8 * 1024 * 1024,
    max_result_rows: 10_000,
    max_rendered_bytes: 4 * 1024 * 1024,
    ..ExecutionLimits::default()
};
let mut database = Database::with_limits(limits.clone());
database.execute("CREATE TABLE events (id Int64)")?;
let results = database.execute("SELECT id FROM events ORDER BY id")?;
let query = match &results[0] {
    rusthouse::StatementResult::Query(query) => query,
    rusthouse::StatementResult::Command { .. } => unreachable!(),
};
let output = render_with_limit(query, OutputFormat::Json, limits.max_rendered_bytes)?;
assert!(!output.is_empty());

# Ok::<(), rusthouse::Error>(())
~~~

Input, token, statement, schema, stored-value, intermediate-row, memory, and
result-row failures return `Error::ResourceLimitExceeded` with the resource,
configured limit, and observed value. `last_execution_stats` remains available
after success or failure. Its row counters are cumulative across all statements
in the batch; stored values report the database total after the attempt. Parse
failures retain the number of recognized tokens, completed statements, and
widest schema prefix reached before the error.

Ordered scans and grouped scans write deterministic fixed-width index runs to
the system temporary directory when their index buffer reaches the memory
budget. Run metadata uses a charged, fixed-capacity vector, and the two smallest
runs merge whenever a third run is retained. This bounds metadata and live file
count independently of input size while keeping merges balanced. Runs use
keyed-random 128-bit names, exclusive creation, and owner-only permissions, and
are removed on success or error. Applications that need a controlled location
can use `Database::with_limits_and_spill_directory`.

The memory limit accounts for transient executor-owned index buffers, grouped
values, and every retained result allocation, including batch/result vectors,
column metadata and names, row vectors, values, and strings. Sort chunks reserve
their actual vector capacity before accepting a row and are sized from the
batch's remaining memory after earlier results. Persistent typed columns are
governed by `max_stored_values`; parser allocations are governed by input and
token limits.

Planning vectors for expanded projections, grouping, aggregate specifications,
and ordering reserve their actual capacity before allocation and are released
after execution. Group keys, aggregate-state/output vectors, string extrema,
and grouped-row capacity follow the same pre-allocation accounting.

Compiled predicate nodes reserve their boxed-node footprint before allocation.
Literal values are borrowed from the bounded parser output rather than cloned,
so large string predicates do not create a second copy during execution.

String `MIN` and `MAX` states retain source row indices while scanning and clone
only the final extremum, so discarded candidates do not allocate executor
memory.

`render_with_limit` counts the exact attempted output while retaining no more
than its byte limit. Table output computes widths in one pass and streams cells
in a second pass; CSV escaping is also streamed. The CLI applies the default
rendered-output ceiling and reads at most one byte beyond the input ceiling from
standard input before rejecting oversized piped SQL.

`Database::new()` uses the available parallelism reported by the operating
system. `Database::with_worker_count` and `Database::set_worker_count` select an
explicit positive maximum. Scans use fixed 4,096-row morsels; eligible
per-morsel aggregate states are merged in source order, so results are
reproducible across worker counts. A scan that fits in one morsel does not create
worker threads. Parallel aggregation falls back to the spill-backed path when
its conservative transient-memory reservation would exceed the configured
budget.

## Current boundaries

RustHouse has no NULL, joins, arithmetic expressions, updates, deletes, quoted identifiers, persistence, transactions spanning multiple SQL statements, HTTP API, or network protocol. Data exists only for the lifetime of the Database value or CLI process. A multi-row INSERT is validated in full before any of its rows are appended.

To keep recursive predicate processing bounded, each WHERE expression is limited to 64 levels of parenthesis nesting and 256 total comparison/boolean AST nodes. Queries over either limit return a SQL error before execution.

On empty input, COUNT and SUM return numeric zero. MIN, MAX, and AVG return an actionable error because the current type system has no nullable result.

## Development

The only runtime dependency is `getrandom`, used to source OS entropy for secure
spill filenames. Rust 1.85 is the minimum supported version;
`rust-toolchain.toml` pins Rust 1.92.0, rustfmt, and Clippy for reproducible
development and CI.

Run the complete quality gates with:

~~~bash
cargo fmt --all -- --check
cargo build --all-targets --locked
cargo test --all-targets --locked
cargo test --doc --locked
cargo clippy --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items --locked
cargo +1.85.0 check --all-targets --locked
~~~

RustHouse is distributed under the [MIT License](LICENSE).

<!-- burner-progress:start -->
## Burner evaluation progress

![Burner evaluation progress](docs/burner-evaluation-progress.svg)

The versioned [raw history](docs/burner-evaluation-history.json) is the source of
truth. After each successful Burner PR merge, Burner automation passes the PR
number, full merge SHA, merge timestamp, title, and complete enabled-evaluation
score map to `python3 scripts/burner_history.py update`. The command validates
the schema, upserts the merge by its `pr:<number>` key so retries cannot create
duplicates, serializes concurrent merge hooks, and transactionally regenerates
both tracked artifacts. Equal merge timestamps are ordered by numeric PR number.
Evaluations introduced later declare the preceding history point in
`introducedAfter`, so older points correctly omit those scores while the first
subsequent merge requires them.

The merge automation interface is:

~~~bash
python3 scripts/burner_history.py update \
  --pr-number "$PR_NUMBER" \
  --merge-sha "$MERGE_SHA" \
  --recorded-at "$MERGED_AT" \
  --title "$PR_TITLE" \
  --scores-file burner-scores.json
~~~

`burner-scores.json` must be a flat JSON object mapping every evaluation ID
enabled for that merge to an integer score from 0 through 100. `MERGE_SHA` is a
full lowercase Git SHA and `MERGED_AT` is an RFC 3339 UTC timestamp ending in
`Z`. The command writes both tracked artifacts only after the complete update
validates.

CI runs `python3 -m unittest scripts.test_burner_history` and
`python3 scripts/burner_history.py check`; it fails on missing or malformed
scores, invalid provenance or chronology, noncanonical JSON, and a stale SVG.
CI verifies artifacts from the merge-coupled automatic update but does not
synthesize scores.
<!-- burner-progress:end -->
