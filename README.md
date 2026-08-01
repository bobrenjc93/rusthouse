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
# std::fs::remove_file(".catalog.rhcat.lock")?;
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
`.catalog.rhcat.lock`. While holding it, a commit validates and encodes the
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
