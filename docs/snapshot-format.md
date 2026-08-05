# Snapshot envelope format

RustHouse snapshots use a versioned binary envelope. The envelope provides a
corruption and resource boundary. The first defined payload serializes nullable
`Int64` rows; catalog serialization remains outside this format.

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
synchronizes the parent directory. On non-Solaris Unix it also holds an
exclusive advisory lock associated with the parent, so replacement and repair
calls from this crate serialize within one directory. If locking the read-only
directory descriptor returns `EBADF`, as it does on Linux NFS, RustHouse opens
and locks a persistent writable `.rusthouse-snapshot.lock` file in that opened
directory instead. This guarantee is cooperative: every replacing writer must
use these APIs. Direct filesystem writes and renames do not participate in the
advisory lock, must not modify its lock file, and must not run concurrently.
Creation, rename, cleanup, and sync stay relative to the one opened directory
descriptor, so renaming or rebinding the parent path cannot redirect later
stages or strand the temporary file. The temporary-name search is bounded.
Names extend the destination with a unique suffix and are compared to the
destination by filesystem identity immediately after exclusive creation; an
alias is removed and retried before any bytes are written. This covers
case-folding filesystems rather than relying on byte-exact name comparison.
Destinations ending in `/` or `/.` are rejected instead of being normalized to
a different pathname. Ordinary replacement is not exposed on Windows. Solaris
supports replacement without the cooperative lock; repair is therefore not
exposed there.

Encoding, parent-directory opening and (where supported) locking, destination
inspection, temporary creation, writing, temporary-file synchronization,
publication, rename, cleanup, and directory synchronization failures are
distinct typed errors. Conditional repair also reports a changed destination
separately.
Failures before successful publication attempt to remove the temporary file
and leave an existing destination unchanged. A directory-sync failure is
different: the destination has already been replaced and is visible, but the
publication's durability after a system crash is uncertain.

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

## Restoring one Int64 table

`restore_int64_table` composes a caller-configured `SnapshotCodec` and
`NullableI64PayloadCodec` with a caller-supplied schema and table row cap. It
decodes the complete envelope and payload before atomically appending the rows
to a new table. Envelope, payload, schema nullability, and row-cap failures are
reported as distinct typed error variants; a failure never returns a partially
populated table.

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

On supported Unix targets other than Solaris,
`restore_and_repair_int64_table_from_file_with_backup` adds automatic primary
repair to that bounded recovery policy. It opens and locks the primary's parent
before the first restoration attempt, then reads and repairs the primary
relative to that one directory descriptor. A valid primary still returns
without inspecting the backup. Otherwise, a valid backup is read once. Its
validated envelope is published only if the primary directory entry and, when
bounded bytes were readable, its exact contents still match the failed attempt.
That check can report a change completed before publication as
`SnapshotReplaceError::DestinationChanged`, but it is not an atomic
compare-and-swap for writers outside the cooperative lock protocol. A
cooperative `SnapshotCodec::replace_file` call waits for repair to finish; a
concurrently created previously missing primary is also protected by
no-replace publication. The backup is never modified. The repair error type
distinguishes failures of both bounded restoration attempts from failures
during atomic primary replacement; a directory-sync failure therefore reports
that the primary is already visible but its crash durability is uncertain.
