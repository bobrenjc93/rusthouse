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

Readers verify header metadata before allocating and enforce configurable limits for files, metadata, rows, columns, blocks, decoded buffers, and strings. Opening a segment verifies every block checksum and recomputes every zone map from decoded values; persisted statistics are never trusted until that integrity pass succeeds. Later predicate scans consult the verified zone maps first and do not decode pruned blocks during the scan.

Block decoders stream packed integers and front-coded strings directly into their final nullable vectors. The decoded-block limit covers the nullable vector plus the cumulative string capacity, rather than relying on transient intermediate buffers outside the accounting.

`write_segment` writes and syncs a uniquely named temporary file in the destination directory, then publishes it with platform-specific no-replace durability. Unix uses a hard link followed by temporary-name removal and a parent-directory sync; Windows uses an atomic `MOVEFILE_WRITE_THROUGH` move without the replace flag. The final path is therefore never visible with partial contents, an existing segment is never replaced, and success is not reported before publication is durable.

The format is intentionally self-contained and little-endian. Readers reject unknown versions, unknown encodings, non-canonical or overlapping block extents, invalid UTF-8, inconsistent null maps or statistics, non-zero padding, trailing data, and arithmetic overflow.

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
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
A conflict ends the transaction, while a pre-publication persistence error keeps it
active for retry. Durability uncertainty ends it because the generation is visible.

`TransactionLimits` bounds cumulative inserted rows and encoded staged DDL/DML bytes.
CREATE accounting includes every persisted string length prefix, name, type,
nullability flag, column count, and row-count field. A statement that would exceed either limit has no effect and leaves an
explicit transaction active. The defaults are 1,000,000 rows and 256 MiB.

`Database::open` persists each committed generation with a checksum and format version.
It writes and syncs a temporary file, atomically renames it, and syncs the parent
directory before publishing the generation to other sessions. A canonical database
path has one exclusive in-process or cross-process owner; a second `Database::open`
returns `Error::DatabaseAlreadyOpen` until the first handle and its clones are dropped.
Locks use a reserved `.rusthouse-lock` namespace; database paths ending in that suffix
are rejected so no database can replace another database's active lock inode. Lock
files are opened without following symlinks and, after locking, must resolve to the same
native file identity as the opened handle. The database parent directory must already exist; RustHouse never creates a
directory tree whose ancestor entries fall outside its durability barrier.
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
first publication; the synced temp handle is closed before either native operation and
Windows never attempts POSIX directory syncing. `ReplaceFileW` receives
a unique backup path. On its partial-move error, RustHouse restores the old snapshot
first, otherwise publishes the candidate, and retains both recovery files if neither
operation succeeds. Because `ReplaceFileW` has no supported write-through metadata
barrier, successful existing-file replacement returns `CommitDurabilityUncertain` while
keeping the published generation installed in memory. Generic error cleanup therefore
never deletes the only snapshot.

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
