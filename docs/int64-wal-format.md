# Int64 write-ahead log format

RustHouse can opt one existing batch table with exactly one `Int64` or
programmatic `Nullable(Int64)` column into a bounded write-ahead log on Unix.
The WAL is independent of snapshots: its first committed record bootstraps the
selected table and later records describe appends, truncates, and atomic value
replacements. A registry manifest can publish a bounded, deterministic set of
these independently replayed member WALs as one catalog recovery unit.

## Version 1 frame

All integers are little-endian. Records are contiguous and sequences begin at
zero. Every record has this layout:

| Offset | Size | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic `RHI64WAL` |
| 8 | 2 | Version (`1`) |
| 10 | 1 | Kind |
| 11 | 1 | Reserved (`0`) |
| 12 | 8 | Sequence (`u64`) |
| 20 | 8 | Payload length (`u64`) |
| 28 | 4 | CRC-32/ISO-HDLC checksum |
| 32 | variable | Payload |
| 32 + payload length | 8 | Commit magic `RHWLCMIT` |
| 40 + payload length | 8 | Repeated sequence |

The checksum covers version, kind, reserved byte, sequence, payload length,
and payload. It uses the same CRC-32/ISO-HDLC parameters as snapshot envelopes.
The repeated commit sequence makes a complete footer the commit boundary.

Kinds are `1` bootstrap, `2` non-nullable append, `3` truncate, `4`
non-nullable replacement, `5` nullable append, and `6` nullable replacement. The
bootstrap stores the table and column display names, explicit nullability,
the table-local and database-default row/column/cell caps, all query resource
caps (including result bytes), the aggregate worker cap, and the current rows.
Append stores a row count and `i64` values. Truncate has an empty payload.
Replacement stores a count followed by strictly increasing `(u64 row, i64
value)` pairs. Nullable values use one presence byte followed by eight value
bytes; a null has tag `0` and canonical zero value bytes, while a present value
has tag `1`. Nullable replacement entries therefore contain a row followed by
that nine-byte value. Non-nullable bootstrap and mutation bytes are unchanged
from the original version-1 format.

## Version 1 registry manifest

A registry directory contains `manifest.rhi64` and only the member names it
lists. Member names are generated as `table-NNNNNNNN.wal`; table text never
becomes a pathname. All manifest integers are little-endian:

| Offset | Size | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic `RHI64REG` |
| 8 | 2 | Version (`1`) |
| 10 | 2 | Reserved (`0`) |
| 12 | 4 | Descriptor count |
| 16 | 8 | Payload length |
| 24 | 4 | CRC-32/ISO-HDLC checksum |
| 28 | variable | Descriptor payload |

The checksum covers version, reserved, descriptor count, payload length, and
payload. Each descriptor is a `u32` table-name byte length and UTF-8 bytes,
then a `u32` member-name byte length and UTF-8 bytes. Descriptors must be in
strict case-insensitive table-name order. Table names and member names must be
unique case-insensitively, and every member must be one normal path component.

## Commit and recovery

Creation opens the parent directory, exclusively creates the final basename
relative to that descriptor, and later syncs that same descriptor. Renaming or
rebinding the parent pathname cannot redirect creation to one directory while
another is synchronized.

For the bootstrap and every later mutation, the header and payload (the record
body) are written and synchronized first. Only then is the commit footer
written and synchronized. The new parent directory entry is synchronized after
the bootstrap footer. This ordering ensures that any footer which can survive
a crash refers to an already-durable body. The in-memory mutation is published
only after the footer sync succeeds. A write or sync failure poisons that
writer; the failed mutation is not published and callers must recover before
continuing because an I/O error can leave the commit outcome indeterminate.

Recovery opens its source nonblocking and accepts only a regular-file
descriptor, so a FIFO or a concurrently replaced special-file path cannot
stall replay. It reads no more than the configured file-byte bound and checks
the payload-byte and committed-record bounds before allocation. It replays only
a contiguous, checksummed sequence beginning with a bootstrap. A final partial
header, payload, or footer is an uncommitted crash tail and is ignored. When a
declared length extends beyond EOF, recovery derives the sole payload boundary
the versioned writer can emit for that record kind and authenticates the exact
footer and checksum there. This bounded check applies to intermediate and final
records, so a corrupted overlong length cannot disguise a committed record or
silently discard authenticated later records as a torn tail. Invalid bytes in
any complete frame, a bad checksum or sequence, malformed mutation, or a
replayed table-cap violation is a typed error. Replay constructs the catalog
and cached metrics privately; failure never returns partially visible database
state.

Registry creation exclusively creates the final directory relative to one
opened parent descriptor and synchronizes the parent. It then creates and
synchronizes every member bootstrap relative to the opened new directory.
Only after all members are durable does it exclusively write and synchronize
the manifest and synchronize the directory. Recovery opens the final directory,
manifest, and members with descriptor-relative no-follow operations. It also
enumerates the opened directory and requires the exact manifest/member set;
hard-link inode aliases are rejected. Per-table bounds apply before member
allocation, while table count, manifest bytes, aggregate member bytes, and
aggregate committed records have independent inclusive bounds. Members replay
in manifest order. Their database defaults, query limits, and worker cap must
agree, and each descriptor table name must exactly match its member bootstrap.
All members and cached metrics are constructed privately before the fresh
database is returned.

The current recovery API is read-only and does not append to or truncate the
source WAL, so repeated recovery is idempotent. A recovered database is not
automatically durable. To resume logging, or to compact a long history, enable
a new WAL or registry at a new path; its bootstrap is the compacted current
state. After that succeeds, the old WAL may be archived or removed by the
caller. There is no in-place or online compaction, cross-table transaction log,
or log rotation. The configured per-table and aggregate limits are hard caps;
reaching one rejects the mutation before visible state changes.

While a WAL is attached, table/column renames, schema changes, row deletion,
table drop, and snapshot replacement of a logged table are rejected. Other
tables remain ordinary in-memory tables and are not reconstructed. An atomic
INSERT batch spanning more than one independently logged registry table is
rejected before any WAL write.
