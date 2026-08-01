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

## Current transaction model

The library exposes `Database` and independent `Session` handles. A session accepts
`BEGIN`, `COMMIT`, and `ROLLBACK` alongside `CREATE TABLE`, `DROP TABLE`, `INSERT
INTO ... VALUES`, and simple `SELECT` projections and predicates.

Transactions pin an immutable catalog generation. Reads use that generation plus the
session's staged table replacements, so readers remain stable and writers read their
own changes. Commit checks every written table against the pinned generation. Changes
to the same table conflict; changes to disjoint tables merge into a new generation.
A conflict ends the transaction, while a persistence error keeps it active for retry.

`TransactionLimits` bounds cumulative inserted rows and the estimated encoded bytes of
staged DDL/DML. A statement that would exceed either limit has no effect and leaves an
explicit transaction active. The defaults are 1,000,000 rows and 256 MiB.

`Database::open` persists each committed generation with a checksum and format version.
It writes and syncs a temporary file, atomically renames it, and syncs the parent
directory before publishing the generation to other sessions. A canonical database
path has one exclusive in-process or cross-process owner; a second `Database::open`
returns `Error::DatabaseAlreadyOpen` until the first handle and its clones are dropped.
Locks use a reserved `.rusthouse-lock` namespace; database paths ending in that suffix
are rejected so no database can replace another database's active lock inode.
Candidates and recovery backups use the reserved `.rusthouse-tmp.` filename prefix,
which `Database::open` also rejects after canonicalizing the path. A concurrent database
therefore cannot adopt or replace another database's pending snapshot.
Writer-side validation guarantees every committed catalog satisfies the decoder's
table, column, row, string, file-size, and total-allocation bounds.

Snapshot temporary files start owner-only and are synced before publication. Existing
Unix UID/GID, modes, and ACLs are copied before the atomic rename (ACLs on macOS, Linux,
and FreeBSD), followed by a parent-directory sync. If that final sync fails, the API
returns `Error::CommitDurabilityUncertain` but installs the already-published generation
in memory and ends the transaction. Windows uses `ReplaceFileW` for existing
snapshots so ACL/security metadata is retained, and write-through `MoveFileExW` for
first publication; it never attempts POSIX directory syncing. `ReplaceFileW` receives
a unique backup path. On its partial-move error, RustHouse restores the old snapshot
first, otherwise publishes the candidate, and retains both recovery files if neither
operation succeeds. Generic error cleanup therefore never deletes the only snapshot.

`Database::execute` is only for one-shot autocommit DDL, DML, and queries. Transaction
control requires a persistent session returned by `Database::session`.

The CLI uses the same session implementation. Repeat `-e` to keep several statements
in one session, or omit it to read one statement per input line:

```bash
cargo run -- --database demo.db \
  -e 'BEGIN' \
  -e 'CREATE TABLE events (id Int64, label String)' \
  -e "INSERT INTO events VALUES (1, 'ready')" \
  -e 'COMMIT'
```

```rust
use rusthouse::{Database, StatementResult};

let database = Database::new();
let mut session = database.session();
session.execute("BEGIN")?;
session.execute("CREATE TABLE events (id Int64, label String)")?;
session.execute("INSERT INTO events VALUES (1, 'ready')")?;
session.execute("COMMIT")?;

let StatementResult::Query(rows) = session.execute("SELECT * FROM events")? else {
    unreachable!();
};
assert_eq!(rows.row_count(), 1);
# Ok::<(), rusthouse::Error>(())
```
