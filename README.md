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
images are rejected before the current snapshot is changed. Only one store may
hold the nonblocking writer lock for a path at a time; concurrent commits made
through one shared store handle are serialized in process. Snapshot filenames
matching the generated `.<name>.lock` or `.<name>.tmp` sidecar namespace are
rejected, including case and trailing-dot/space aliases.

Snapshot persistence supports Windows, macOS, and Linux. Other Unix targets,
including FreeBSD filesystems with either POSIX.1e or NFSv4 ACLs, compile but
return `SnapshotError::UnsupportedPlatform` before creating sidecars because
their ACL semantics are not implemented. Existing or dangling final-component
symbolic links are rejected. On Unix, each subsequent read, metadata copy, and
publication is relative to the parent directory handle opened by the store, so
replacing that final component after open cannot redirect snapshot access.

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
cargo run -- --help
```

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.
