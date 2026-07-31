# RustHouse

RustHouse is a small, dependency-free analytical SQL engine written in Rust. It keeps tables in memory and stores each field in a contiguous, typed column (Vec<i64>, Vec<f64>, Vec<bool>, or Vec<String>).

## What works

- CREATE TABLE with Int64, Float64, Bool, and String columns
- multi-row INSERT INTO ... VALUES with row-width and exact type validation
- SELECT * and named projections, with optional AS aliases
- WHERE comparisons using =, !=, <>, <, <=, >, and >=
- AND, OR, and parentheses in predicates (AND binds more tightly)
- COUNT, SUM, MIN, MAX, and AVG
- GROUP BY, output-column or alias ORDER BY with ASC/DESC, and LIMIT
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

## Current boundaries

RustHouse has no NULL, joins, arithmetic expressions, updates, deletes, quoted identifiers, persistence, transactions spanning multiple SQL statements, HTTP API, or network protocol. Data exists only for the lifetime of the Database value or CLI process. A multi-row INSERT is validated in full before any of its rows are appended.

To keep recursive predicate processing bounded, each WHERE expression is limited to 64 levels of parenthesis nesting and 256 total comparison/boolean AST nodes. Queries over either limit return a SQL error before execution.

On empty input, COUNT and SUM return numeric zero. MIN, MAX, and AVG return an actionable error because the current type system has no nullable result.

## Development

The crate has no third-party dependencies. Rust 1.85 is the minimum supported
version; `rust-toolchain.toml` pins Rust 1.92.0, rustfmt, and Clippy for
reproducible development and CI.

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
