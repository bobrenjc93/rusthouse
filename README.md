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

Readers verify header metadata before allocating and enforce configurable limits for files, metadata, rows, columns, blocks, per-block decoded buffers, cumulative decoded results, and strings. Full reads preflight retained output from block metadata; scans charge selected values as they append and use fallible reservations. Opening a segment verifies every block checksum and recomputes every zone map from decoded values; persisted statistics are never trusted until that integrity pass succeeds. Later predicate scans consult the verified zone maps first and do not decode pruned blocks during the scan.

Block decoders stream packed integers and front-coded strings directly into their final nullable vectors. The decoded-block limit covers the nullable vector plus the cumulative string capacity, rather than relying on transient intermediate buffers outside the accounting.

`write_segment` creates its temporary file owner-only before writing, clearing inherited Unix ACLs or applying a protected Windows DACL, then syncs and publishes it with platform-specific no-replace durability. Unix uses a hard link followed by temporary-name removal and a parent-directory sync; Windows uses an atomic `MOVEFILE_WRITE_THROUGH` move without the replace flag. The final path is therefore never visible with partial contents and an existing segment is never replaced. `SegmentWriteOutcome::Durable` confirms publication durability; `PublishedUncertain` means the final path is already visible but cleanup or directory syncing failed, so callers must not retry as a new write.

The format is intentionally self-contained and little-endian. Readers reject unknown versions, unknown encodings, non-canonical or overlapping block extents, invalid UTF-8, inconsistent null maps or statistics, non-zero padding, trailing data, and arithmetic overflow.

## Catalog snapshots

The library exposes typed, nullable columnar catalog images directly, without a
SQL parser or service:

```rust,no_run
use rusthouse::{
    CatalogImage, ColumnData, ColumnImage, SchemaImage, SnapshotStore, TableImage,
};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let id = ColumnImage::new("id", ColumnData::Int64(vec![Some(1), None]))?;
let table = TableImage::new("events", vec![id])?;
let schema = SchemaImage::new("analytics", vec![table])?;
let image = CatalogImage::new(1, vec![schema])?;

let store = SnapshotStore::open("catalog.rhcat")?;
store.commit(&image)?;
assert_eq!(store.load()?, Some(image));
# std::fs::remove_file("catalog.rhcat")?;
# std::fs::remove_file("catalog.rhcat.rusthouse-lock")?;
# Ok(())
# }
```

`SnapshotStore::open_with_limits` accepts `SnapshotLimits` for untrusted or
resource-constrained inputs. Limits cover the complete file, object counts,
rows, total values, names, individual strings, and total string bytes. Counts
and lengths are checked before allocation, allocation is fallible, and invalid
images are rejected before the current snapshot is changed. Writer locks are
nonblocking and cover one snapshot on Unix or one directory on Windows;
concurrent commits made through one shared store handle are serialized in
process. Dot-prefixed snapshot filenames are rejected because that namespace is
reserved for lock and temporary sidecars, including filesystem-specific Unicode
aliases. Windows also rejects all snapshot names with trailing dots or spaces
because Win32 resolves them as aliases of trimmed names.

Snapshot persistence supports Windows, macOS, and Linux. Other Unix targets,
including FreeBSD filesystems with either POSIX.1e or NFSv4 ACLs, compile but
return `SnapshotError::UnsupportedPlatform` before creating sidecars because
their ACL semantics are not implemented. Existing or dangling final-component
symbolic links are rejected. On Unix, each subsequent read, metadata copy, and
sidecar operation is relative to the parent directory handle opened by the
store. Publication and directory fsync use that same handle, so replacing the
parent path or final component after open cannot redirect snapshot access.
Linux ACL operations use file descriptors directly and do not require procfs.
On Windows, writers are serialized per directory and existing snapshots must be
opened with their normalized long filename, so DOS short aliases cannot acquire
independent locks. The store also holds a parent-directory handle without delete
sharing, which prevents renaming or replacing that directory until it is dropped.

### Version 1 binary format

All integers are little-endian. Offsets and sizes below are bytes. CRC32 means
the IEEE CRC-32 used by Ethernet and gzip (polynomial `0xedb88320`, initial and
final XOR `0xffffffff`).

| Offset | Size | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic `52 48 43 41 54 00 0d 0a` (`RHCAT\0\r\n`) |
| 8 | 2 | Format version, currently `1` |
| 10 | 2 | Flags, must be zero |
| 12 | 4 | Header length, must be `32` |
| 16 | 8 | Payload length |
| 24 | 4 | CRC32 of the payload |
| 28 | 4 | CRC32 of header bytes 0 through 27 |

The payload is a depth-first catalog image:

```text
u64 generation
u32 schema_count
  repeated schema_count times:
    string schema_name
    u32 table_count
      repeated table_count times:
        string table_name
        u64 row_count
        u32 column_count
          repeated column_count times:
            string column_name
            u8 type_tag
            u8[3] reserved_zeroes
            u8[ceil(row_count / 8)] validity_bitmap
            non_null_values
```

A `string` is a `u32` byte length followed by that many UTF-8 bytes. Validity
bit `i`, least-significant bit first, is one when row `i` has a stored value;
unused high bits in the last byte must be zero. NULL rows have no value bytes.
Non-NULL values stay in row order and use these encodings:

| Tag | Type | Encoding |
| ---: | --- | --- |
| 1 | `Int64` | 8-byte two's-complement integer |
| 2 | `Float64` | 8-byte IEEE-754 bit pattern |
| 3 | `Bool` | one byte, exactly 0 or 1 |
| 4 | `String` | length-delimited UTF-8 string |

Readers reject unknown versions or tags, nonzero reserved fields, malformed
UTF-8, invalid booleans and bitmap padding, duplicate names, mismatched file
lengths, nonzero rows on a table without columns, trailing bytes, and either
checksum mismatch.

### Commit and recovery contract

For `catalog.rhcat`, the store holds an OS advisory lock on
`catalog.rhcat.rusthouse-lock`. This is the same lock namespace used by
`Database`, so the two persistence APIs cannot concurrently publish incompatible
formats to one path. After locking, the store reopens the sidecar and verifies
its native file identity; Windows also pins the parent directory without delete
sharing for the handle's lifetime. While holding it, a commit validates and encodes the
image, creates `.catalog.rhcat.tmp` as a private staging directory inside the
snapshot directory, writes and calls `sync_all` on its `snapshot` file, then
atomically renames that file over `catalog.rhcat`. Unix creates the staging
directory with mode `0700`; Windows creates it with a protected DACL granting
access only to its owner and the system. Unix also removes inherited and default
ACL entries before creating the staged file. Immediately before publish, an
existing snapshot's Unix owner, group, mode, and native ACL, or Windows owner,
group, and DACL is copied to the staged file. The enclosing private directory
keeps prepared data inaccessible after a failed or interrupted publish. Unix
syncs the parent directory after rename. Windows
publishes with `MoveFileExW` using `MOVEFILE_REPLACE_EXISTING` and
`MOVEFILE_WRITE_THROUGH`, which makes the metadata update durable without opening
the directory. A failure before rename leaves the previous snapshot unchanged.
A crash after rename exposes either the old or new complete file according to
filesystem recovery; reopening validates the selected file.
Reopening while holding the writer lock recursively removes an orphan `.tmp`
staging directory (or a legacy temp file) left before rename; Unix also syncs
that directory removal. Lock files are intentionally retained, but their OS
locks are released when `SnapshotStore` is dropped.

## Bulk CSV and NDJSON

The library exposes query-independent typed storage and streaming bulk formats. A `Schema` defines ordered `Int64`, `Float64`, `Bool`, and `String` fields and whether each field accepts `NULL`. `CsvBatchReader` and `NdjsonBatchReader` produce rectangular `ColumnBatch` values without retaining the complete input.

```rust
use rusthouse::formats::{CsvOptions, export_ndjson, ingest_csv};
use rusthouse::{DataType, Field, Schema, Table};
use std::io::Cursor;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::String, true),
    ])?;
    let mut table = Table::new(schema);
    ingest_csv(
        Cursor::new(b"id,name\n1,Ada\n2,\\N\n"),
        &mut table,
        CsvOptions::default(),
    )?;

    let mut output = Vec::new();
    export_ndjson(&mut output, &table)?;
    Ok(())
}
```

The conversion rules are deliberately explicit:

- CSV headers, when enabled, must exactly equal the schema names in schema order. Records use RFC 4180 quoting, including doubled quotes and embedded line endings.
- The default CSV `NULL` is an exact, unquoted `\N`. A quoted `"\N"` is the string `\N`, and an empty field is an empty string rather than `NULL`. The token is configurable.
- CSV integers and floats must consume the complete field with no surrounding whitespace. Floats must be finite. Booleans are exactly lowercase `true` or `false`.
- Each nonblank NDJSON line must be one JSON object with every schema field exactly once. Field order is irrelevant, but extra, duplicate, and missing fields are errors.
- NDJSON uses JSON scalars without implicit coercion: JSON numbers feed numeric columns, JSON booleans feed `Bool`, JSON strings feed `String`, and only literal `null` produces `NULL`. Nested values are rejected by scalar schemas.
- Invalid UTF-8, non-finite floats, conversion failures, and `NULL` in non-nullable fields are typed errors. CSV and NDJSON exporters apply the inverse escaping rules and preserve schema order.

`FormatLimits` independently bounds total input bytes, rows, fields per record, decoded field bytes, JSON nesting depth, decoded string bytes, record bytes, and rows per batch. JSON depth is capped at the stack-safe `MAX_JSON_NESTING_DEPTH`, and larger configurations are rejected. Batch columns allocate lazily as rows arrive, so a large batch limit does not reserve memory for an empty or short input. Parsing retains one bounded record and one typed batch. The `ingest_csv` and `ingest_ndjson` helpers write validated batches to a private temporary spool first, then replay them into the table; a parse, limit, staging, or replay error leaves the destination at its original row count. Applications that consume the batch iterators directly own any already-consumed batches themselves.

## Vector kernel layer

`rusthouse::batch` provides query-independent fixed-capacity `Int64`, `Float64`, bit-packed Boolean, and dictionary-encoded String arrays. A `RecordBatch` validates its schema, equal column shape, nullability, and byte ceiling before it can be used. `retained_bytes()` is the sum of owned heap payloads, including fixed buffers, bitmap words, dictionary slots and UTF-8 bytes, schema metadata, and column containers; allocator bookkeeping is intentionally excluded.

`rusthouse::kernels` provides selection-aware comparisons, `IS NULL`, `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`, and bounded hash grouping. Predicates use SQL-style NULL filtering and IEEE comparisons. Floating extrema use `f64::total_cmp`; grouping coalesces signed zeroes and groups all NaNs together. Hash grouping has independent `max_groups` and retained-byte limits and reports both final and peak retained bytes.

The deterministic benchmark uses fixed generated input and fixed iteration counts:

```bash
cargo bench --bench vector_kernels
```

## Scalar SQL semantics

The storage-independent scalar expression subsystem parses and evaluates
literals, column references, arithmetic, comparisons, `AND`/`OR`/`NOT`, `IS
[NOT] NULL`, `CAST`, searched and simple `CASE`, `COALESCE`, and core string
functions. Stateful `COUNT`, `SUM`, `MIN`, `MAX`, and `AVG` implementations
define the behavior the query engine will use for groups.

The complete semantic contract, including error and edge-case behavior, is in
[`docs/sql-semantics.md`](docs/sql-semantics.md).

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
Double-quoted tokens are identifiers only and are never interpreted as these
statement keywords.

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
Locks use the same reserved `.rusthouse-lock` namespace as `SnapshotStore`; database
paths ending in that suffix or beginning with `.` are rejected so neither persistence
API can replace another writer's active lock or staging path. Lock files are opened
without following symlinks and, after locking, must resolve to the same native file
identity as the opened handle. The database parent directory must already exist; RustHouse never creates a
directory tree whose ancestor entries fall outside its durability barrier. On Windows,
the opened database retains a no-delete-share parent handle so that path cannot be
renamed and replaced while the writer remains active.
Candidates and recovery backups use the reserved `.rusthouse-tmp.` filename prefix,
which `Database::open` also rejects after canonicalizing the path. A concurrent database
therefore cannot adopt or replace another database's pending snapshot.
Writer-side validation guarantees every committed catalog satisfies the decoder's
table, column, row, string, file-size, and total-allocation bounds.

Snapshot temporary files are protected owner-only before any catalog bytes are written
and are synced before publication. Unix removes inherited ACL entries and Windows uses
a protected DACL granting access only to the owner and system. Existing
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

## HTTP query service

The `rusthouse::http` module exposes a long-lived HTTP/1.1 server behind the
engine-independent `QueryService` trait. An engine implements that trait and
returns a `QueryResult`; the frontend owns protocol parsing, output encoding,
admission control, deadlines, cancellation, and shutdown.

`POST /query` accepts UTF-8 SQL as `application/sql`, or a JSON object such as
`{"query":"SELECT 1"}` with `application/json`. A Content-Type is required and
browser-originated requests are rejected; the endpoint intentionally does not
accept browser-safelisted form or `text/plain` submissions. Select an output with
`Accept` or with `?format=`:

| Format | `Accept` value |
| --- | --- |
| JSON array of row objects | `application/json` |
| newline-delimited row objects | `application/x-ndjson` |
| CSV with a header row | `text/csv` |

Errors are JSON objects with stable codes, messages, and request IDs. The same
request ID is returned in `x-request-id`. `GET /health/live` checks the process;
`GET /health/ready` and `GET /health` report `QueryService::health()`.

The server defaults to 1 MiB requests, 16 MiB encoded responses, 16 concurrent
queries, 64 concurrent HTTP query requests, a 10 second body-read deadline, a 30
second query deadline, 128 accepted connections, a 10 second header deadline, a
60 second connection idle deadline, and a 10 second graceful shutdown window.
Connection admission is held through response delivery, while the idle deadline
closes stalled headers, keep-alive clients, and response readers. Busy request or
execution slots fail immediately with `503` and `Retry-After`, so slow clients do
not create an unbounded queue. Transient accept errors are retried with capped
backoff, and owners can await unexpected server-task termination. Query futures
and result encoding run on bounded blocking workers; timed-out jobs retain their
execution slot until they actually exit. Configured timeouts are capped at 365
days so deadline arithmetic remains representable. The production database adapter
also applies the response-byte ceiling while materializing rows. Mutation cancellation
and publication use an atomic handoff: cancellation before publication prevents the
commit, while a timeout after publication starts returns `query_outcome_unknown`
instead of claiming that the mutation failed. Once execution confirms publication,
an encoding limit or encoding deadline falls back to a bounded `204 No Content`
success response rather than reporting a failed mutation. A database durability
error after publication returns HTTP `202` with code
`mutation_published_durability_uncertain`, distinguishing it from a retryable failure.
Query implementations should observe the supplied `QueryCancellation` while doing
expensive work. It is signaled when a request is dropped, its deadline expires, or
forced shutdown begins. Ctrl-C and SIGTERM both use the bounded graceful-shutdown
path.

JSON and NDJSON encode non-finite `Float64` values as the strings `"NaN"`,
`"Infinity"`, and `"-Infinity"`; finite floats remain JSON numbers.

The executable attaches the HTTP service to the same autocommit database used by
the CLI. It uses an in-memory database by default; pass `--database FILE` to serve
a durable database. Each request executes one statement, so transaction control
remains available only through a persistent library or CLI session:

```bash
cargo run -- serve --database demo.db --bind 127.0.0.1:8080 \
  --max-concurrent-queries 8
```

<!-- burner-progress:start -->
## Burner evaluation progress

![Burner evaluation progress graph](docs/burner-evaluation-progress.svg)

Burner updates this graph only after the `burner_evaluation_completed` workflow authenticates the configured Burner actor and verifies that the referenced pull request is merged into the default branch. Exact dispatch retries are no-ops; untrusted senders, incomplete scores, or conflicting PR and merge keys fail the workflow.

[Raw versioned history and update contract](docs/burner-evaluation-history.json)
<!-- burner-progress:end -->
