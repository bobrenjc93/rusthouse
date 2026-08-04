# Snapshot envelope format

RustHouse snapshots use a versioned binary envelope. The envelope provides a
corruption and resource boundary. The first defined payload serializes nullable
`Int64` rows; catalog serialization and atomic file replacement remain outside
this format.

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

This create-only operation does not use temporary files, replace paths, or
synchronize the parent directory.

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
