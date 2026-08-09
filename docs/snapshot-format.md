# Snapshot envelope format

RustHouse snapshots use a versioned binary envelope. The envelope provides a
corruption and resource boundary. Separate payload formats serialize nullable
`Int64` rows either directly or with run-length compression, and a
self-describing payload serializes one bounded `Int64Table`. Catalog
serialization remains outside this format.

## Version 1 layout

All integers are unsigned and little-endian. Offsets are measured in bytes.

| Offset | Size | Field | Value |
| ---: | ---: | --- | --- |
| 0 | 8 | Magic | ASCII `RHOUSESN` |
| 8 | 2 | Version | `1` (`u16`) |
| 10 | 8 | Payload length | Number of payload bytes (`u64`) |
| 18 | 4 | Checksum | CRC-32/ISO-HDLC of the payload (`u32`) |
| 22 | Declared length | Payload | Opaque bytes |

The CRC parameters are polynomial `0x04C11DB7` (reflected representation
`0xEDB88320`), initial value `0xFFFFFFFF`, reflected input and output, and final
XOR `0xFFFFFFFF`. The standard check value for ASCII `123456789` is
`0xCBF43926`.

## Validation

Callers configure an inclusive maximum payload length. Encoding rejects a
payload above that bound. Decoding validates, in order:

1. the complete 22-byte header is present;
2. magic and version are supported;
3. the declared length is representable and within the configured bound;
4. the input length exactly matches the declared envelope length; and
5. the payload checksum matches.

Short input, trailing input, incompatible formats, unsupported versions,
oversized declarations, and checksum mismatches produce distinct typed errors.
The decoder borrows the payload from the input and performs no allocation.

## Creating an envelope file

`SnapshotCodec::create_new_file` validates and encodes the complete payload
before accessing the filesystem. It then exclusively creates the destination,
writes the complete envelope, and synchronizes the file contents and metadata.
An existing destination is never replaced or truncated. Encoding, creation,
writing, and synchronization failures are reported as distinct typed errors.

## Atomically replacing an envelope file

On Unix, `SnapshotCodec::replace_file` applies the same payload bound before
accessing the filesystem. It opens the destination's parent directory,
exclusively creates a sibling temporary file, writes and synchronizes the
complete envelope, renames the temporary file over the destination, and
synchronizes the parent directory. Creation, rename, cleanup, and sync stay
relative to the one opened directory descriptor, so renaming or rebinding the
parent path cannot redirect later stages or strand the temporary file. The
temporary-name search is bounded. Names extend the destination with a unique
suffix and are compared to the destination by filesystem identity immediately
after exclusive creation; an alias is removed and retried before any bytes are
written. This covers case-folding filesystems rather than relying on byte-exact
name comparison. Destinations ending in `/` or `/.` are rejected instead of
being normalized to a different pathname. The API is not exposed on Windows
because the required directory-handle opening and flush semantics are not
implemented.

Encoding, parent-directory opening, temporary creation, writing, temporary
file synchronization, rename, cleanup, and directory synchronization failures
are distinct typed errors. Failures before a successful rename attempt to
remove the temporary file and leave an existing destination unchanged. A
directory-sync failure is different: the destination has already been
replaced and is visible, but the rename's durability after a system crash is
uncertain.

`save_int64_table_payload_to_file` is the Unix-only self-describing table save
operation. It first encodes the complete schema, row cap, and rows through
`Int64TablePayloadCodec`, then passes the complete payload to
`SnapshotCodec::replace_file`. Its typed error distinguishes table-payload
encoding from replacement failures. Because encoding completes before
replacement begins, every pre-rename failure preserves an existing
destination. A post-rename directory-sync error explicitly reports that the
destination was already replaced. The matching bounded reopen operation is
`restore_int64_table_payload_from_file` and requires no caller-supplied schema
or row cap.

The batch `Database::restore_int64_table_from_file` API registers one decoded
self-describing payload under a caller-supplied table name. It currently
accepts only a non-nullable column and validates the database's row, column, and
cell limits before changing the catalog or metrics. The payload is strictly a
single-table format: it contains one column and no database name, batch table
name, additional tables, or catalog metadata.

`Database::replace_int64_table_from_file` instead requires that the
case-insensitively resolved target already exist. It checks that requirement
before opening the snapshot, preserves the target's stored display name, and
stages the decoded column schema, rows, and persisted row cap through the same
non-nullability, identifier, and configured table-limit validation. Only then
does it swap the catalog entry once and replace the cached measurements. A
missing target, corrupt snapshot, nullable or invalid column, or limit failure
leaves the prior table and metrics unchanged.

`Database::restore_int64_tables_from_files` transactionally composes those
single-table payloads into a caller-bounded catalog subset. Each
`DatabaseSnapshotRestoreEntry` supplies a table name, source path, and its own
envelope and payload codec bounds; the separate inclusive `max_entries`
argument is checked before name validation or file access. The complete name
set is validated case-insensitively against itself and the current catalog
before file access. Every decoded table then remains staged outside the catalog
until all entries pass corruption, nullability, row-cap, column, and cell validation.
Only the complete set is registered and charged to cached metrics. An excessive
count or entry failure reports the zero-based entry index and caller name, and
leaves the existing catalog and metrics unchanged.

`SharedDatabase::try_restore_int64_table_from_file` is available on every
supported platform. It makes one nonblocking write-lock attempt before opening
or reading the source, then holds that guard while delegating to
`Database::restore_int64_table_from_file`. Active readers and writers report
database contention without source access; poisoned locks and the existing
typed snapshot failures remain distinguishable. The delegated database restore
keeps catalog entries and cached metrics unchanged on every failure.

`SharedDatabase::try_restore_int64_tables_from_files` provides the same
nonblocking synchronization for an atomic snapshot set. It makes exactly one
write-lock attempt before any source file access and, while holding that guard,
delegates the caller's entry slice and inclusive `max_entries` bound to
`Database::restore_int64_tables_from_files`. Contention, lock poisoning, and
indexed set-restore failures are distinct. Count and name validation still
precede file access, and every failure preserves both catalog data and cached
metrics.

`Database::save_int64_table_to_file` adapts one named batch-engine table to the
same self-describing payload and atomic replacement operation. The selected
table must exist and have exactly one non-nullable physical `Int64` column.
Table lookup and the complete column-count and type validation happen before
filesystem access. The adapter retains the stored column name, row order, and
row cap, but the format still does not include the batch table name or other
catalog tables. The existing `restore_int64_table_payload_from_file` decoder
reopens its output. Its save error distinguishes table lookup and shape
validation from the existing payload and replacement failures, and reports
whether a post-rename directory-sync failure occurred. Name, row-cap, row-count,
and encoded-byte limits are checked directly against borrowed batch storage
before allocating the payload; no intermediate `Int64Table` column clone is
created.

`save_int64_table_to_file` is the Unix-only composed table save operation. It
first encodes an existing `Int64Table` through `NullableI64PayloadCodec`, then
passes the complete payload to `SnapshotCodec::replace_file`. Its typed error
distinguishes payload encoding from replacement failures. Because payload
encoding completes before replacement begins, and replacement cleans up its
temporary file before every failed rename, an existing destination is
preserved on every pre-rename failure. A post-rename directory-sync error
explicitly reports that the destination was already replaced.

`save_int64_table_rle_to_file` provides the same Unix-only atomic save contract
using `NullableI64RlePayloadCodec`. It is an explicit format choice: the helper
persists row values as the versioned RLE payload documented below, retains
distinct RLE-encoding and replacement errors, and preserves the destination on
every pre-rename failure. The payload does not contain schema or row-cap
metadata. `restore_int64_table_rle_from_file` is the matching bounded reopen
helper and therefore requires a caller-supplied schema and row cap as well as
the envelope and RLE codecs. The existing `restore_int64_table_from_file`
helper is not format-detecting and continues to expect
`NullableI64PayloadCodec` bytes.

## Nullable Int64 row payload

`NullableI64PayloadCodec` defines a deterministic payload for one nullable
`Int64` column. It is intended to be encoded as the opaque payload of a version
1 snapshot envelope. All integers are little-endian, and rows retain their
input order.

| Offset | Size | Field | Value |
| ---: | ---: | --- | --- |
| 0 | 8 | Row count | Number of rows (`u64`) |
| 8 | Variable | Rows | Exactly `row count` tagged rows |

Each row begins with a one-byte tag:

| Tag | Following bytes | Meaning |
| ---: | ---: | --- |
| `0x00` | 0 | `NULL` |
| `0x01` | 8 | Present signed `Int64` value (`i64`) |

No padding or trailing bytes are permitted. For example, `[NULL, -1]` is
encoded as the row count `02 00 00 00 00 00 00 00`, followed by `00`, then
`01 ff ff ff ff ff ff ff ff`.

Callers configure inclusive row and payload-byte limits. Both limits are
checked during encoding before allocation. Decoding checks the input byte
length and declared row count before allocation, validates every tag and value,
rejects truncation and trailing data, and only then allocates the decoded row
vector. The payload-byte limit includes the row-count field, tags, and values.

## Run-length encoded nullable Int64 row payload

`NullableI64RlePayloadCodec` is a separate, versioned format for one nullable
`Int64` column. It does not change the bytes or interpretation of
`NullableI64PayloadCodec`. All integers are little-endian, and expanded rows
retain their input order.

The version 1 layout is:

| Offset | Size | Field | Value |
| ---: | ---: | --- | --- |
| 0 | 8 | Payload magic | ASCII `RHNRLEP` followed by `00` |
| 8 | 2 | Payload version | `1` (`u16`) |
| 10 | 8 | Row count | Number of expanded rows (`u64`) |
| 18 | 8 | Run count | Number of encoded runs (`u64`) |
| 26 | Variable | Runs | Exactly `run count` runs |

Every run begins with a one-byte tag and a positive eight-byte run length:

| Tag | Following bytes | Meaning |
| ---: | ---: | --- |
| `0x00` | Run length (`u64`) | That many `NULL` rows |
| `0x01` | Run length (`u64`), value (`i64`) | That many rows containing the one value |

The encoder emits maximal runs deterministically: adjacent `NULL` rows share
one run, adjacent equal present values share one run, and a value change starts
a new run. Empty input has zero rows and zero runs. For example,
`[NULL, NULL, -1, -1, -1, NULL]` has three runs: a length-2 null run, a
length-3 value run containing `-1`, and a length-1 null run. A non-null value
repeated any positive number of times occupies 43 payload bytes (the 26-byte
header plus a 17-byte value run), while the original row format occupies
`8 + 9 * row count` bytes.

Callers configure inclusive row, run, and payload-byte limits. Encoding checks
all three before allocation. Decoding checks the complete input byte length,
magic, version, declared row and run limits, and the minimum bytes implied by
the run count. It then validates every tag, positive run length, and optional
value while adding run lengths with checked `u64` arithmetic. The expanded
total must be within the row limit and exactly equal the declared row count,
and no trailing bytes are allowed. Only after all of those checks succeed does
the decoder allocate the row vector. Zero-length runs, sum overflow, unknown
tags, truncation, trailing data, an unreservable decoded row vector, and each
configured limit have distinct typed errors. The complete RLE payload can be
used directly as the opaque payload of `SnapshotCodec` without changing the
version 1 envelope.

## Self-describing Int64 table payload

`Int64TablePayloadCodec` defines an additive payload format for one complete
`Int64Table`. It does not alter or replace the nullable row payload above. All
integers are little-endian, the column name is UTF-8, and rows use the same
`NULL` and present-value tags as the row-only format.

For a column name containing `N` bytes, the layout is:

| Offset | Size | Field | Value |
| ---: | ---: | --- | --- |
| 0 | 8 | Payload magic | ASCII `RHITBLP` followed by `00` |
| 8 | 2 | Payload version | `1` (`u16`) |
| 10 | 1 | Column type tag | `0x01` (`Int64`) |
| 11 | 1 | Nullability tag | `0x00` (not nullable) or `0x01` (nullable) |
| 12 | 8 | Column-name length | `N` (`u64`) |
| 20 | `N` | Column name | Exactly `N` UTF-8 bytes |
| `20 + N` | 8 | Row cap | Maximum rows accepted by the restored table (`u64`) |
| `28 + N` | 8 | Row count | Current number of rows (`u64`) |
| `36 + N` | Variable | Rows | Exactly `row count` tagged rows |

Callers configure inclusive maximum name bytes, rows, and payload bytes. The
row limit applies independently to both the persisted row cap and current row
count. Encoding checks all three limits before allocation. Decoding first
checks the complete input against the payload-byte limit, then validates magic,
version, the known type and nullability tags, name length and UTF-8, row cap,
row count, every row tag and value, schema nullability, truncation, and trailing
bytes. Only after that complete validation pass does it allocate decoded rows
and construct the table. Unknown type, nullability, and row tags are rejected.

This payload contains exactly one one-column `Int64Table`. Its stored name is
the column name, not a catalog table name. It cannot represent a catalog,
multiple tables, or the batch engine's multi-column and non-`Int64` schemas.
It composes directly with `SnapshotCodec` for checksummed envelopes. On Unix,
`save_int64_table_payload_to_file` performs the complete atomic save operation;
the lower-level `SnapshotCodec::replace_file` remains available. Files written
with either replacement API or `SnapshotCodec::create_new_file` can be reopened
with `restore_int64_table_payload_from_file`; the helper obtains the column
schema, nullability, row cap, and rows entirely from the validated payload.

## Restoring one Int64 table

`restore_int64_table_payload_from_file` is the bounded filesystem entry point
for a self-describing table payload. It accepts only a path and the independent
envelope and table-payload codecs: callers do not provide a schema or row cap.
It requires a regular file, rejects a file larger than the 22-byte envelope
header plus the configured envelope payload limit before reading its contents,
and never reads beyond that bound. After reading, it rejects truncated,
corrupt, or trailing envelope input before decoding the complete table payload.
Open, non-regular-file, read, oversized-file, envelope, and payload errors are
separate typed variants. A table is returned only after the payload codec has
validated its schema metadata, nullability, row cap, rows, and exact boundary.

`restore_int64_table_payload_from_file_with_backup` composes that bounded
self-describing restore for a caller-supplied primary and explicit backup path.
It returns a valid primary without inspecting the backup. Any typed primary
failure, including a missing, corrupt, or over-limit file, causes one backup
attempt with exactly the same envelope and table-payload codecs. Success reports
which path supplied the table. If both attempts fail, one recovery error retains
both `Int64TablePayloadFileRestoreError` values. No caller-supplied schema or row
cap is introduced by recovery, and no failure returns a partially decoded table.

`restore_int64_table` composes a caller-configured `SnapshotCodec` and
`NullableI64PayloadCodec` with a caller-supplied schema and table row cap. It
decodes the complete envelope and payload before atomically appending the rows
to a new table. Envelope, payload, schema nullability, and row-cap failures are
reported as distinct typed error variants; a failure never returns a partially
populated table. The payload contains row values but no schema or row-cap
metadata, so this legacy row-only restore path always requires the caller to
supply both. Self-describing table payloads are decoded with
`Int64TablePayloadCodec` instead.

`restore_int64_table_from_file` provides the bounded filesystem entry point.
It requires a regular file so FIFOs and devices cannot block or hide trailing
input behind stream metadata. Unix opens use nonblocking semantics before the
opened descriptor is validated, so replacing a checked path with a FIFO cannot
block the open. It opens an existing snapshot without modifying it, rejects
files larger than the 22-byte header plus the configured envelope payload
limit, and reads no more than that bound. Open, non-regular-file, read,
oversized-file, and restoration failures remain distinct. After the bounded
read it delegates to `restore_int64_table`, retaining the same all-or-nothing
validation behavior.

`restore_int64_table_rle_from_file` provides the corresponding bounded entry
point for row-only RLE snapshots written by `save_int64_table_rle_to_file`.
Like the uncompressed helper, it requires the caller to provide the schema and
restored table row cap because neither is stored in the payload. It rejects
non-regular files and files larger than the 22-byte envelope header plus the
configured envelope payload limit, and never reads beyond that bound. It then
validates the exact envelope and complete RLE run stream before atomically
appending all decoded rows to a new table. Open, read, oversized-file,
envelope, RLE-payload, schema-nullability, and table-capacity failures remain
typed; no error returns a partially populated table.

`Catalog::restore_int64_table_from_file` applies the catalog's per-table row
cap and registers the restored table under a caller-supplied exact name only
after the complete file and table validation succeeds. Duplicate-name and
table-count failures are reported separately from the nested filesystem and
snapshot errors. Every failure leaves all catalog entries unchanged.

`restore_int64_table_from_file_with_backup` composes the same bounded file
restore for a caller-supplied primary and explicit backup path. A valid primary
takes precedence and the backup is not inspected. Any typed primary failure,
including a missing, truncated, corrupt, schema-invalid, or over-limit file,
causes one backup attempt with exactly the same codecs, schema, and row cap.
Success identifies whether the primary or backup supplied the table. When both
fail, one recovery error retains both typed file restoration errors and no
partially restored table is returned.
