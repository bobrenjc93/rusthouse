# Int64 write-ahead log format

RustHouse can opt one existing batch table with exactly one non-nullable
`Int64` column into a bounded write-ahead log on Unix. The WAL is independent
of snapshots and catalogs: its first committed record bootstraps the selected
table and the later records describe its appends, truncates, and atomic value
replacements.

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

Kinds are `1` bootstrap, `2` append, `3` truncate, and `4` replacement. The
bootstrap stores the table and column display names, explicit non-nullability,
the table-local and database-default row/column/cell caps, all query resource
caps (including result bytes), the aggregate worker cap, and the current rows.
Append stores a row count and `i64` values. Truncate has an empty payload.
Replacement stores a count followed by strictly increasing `(u64 row, i64
value)` pairs.

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

The current recovery API is read-only and does not append to or truncate the
source WAL, so repeated recovery is idempotent. A recovered database is not
automatically durable. To resume logging, or to compact a long history, enable
a new WAL at a new path; its bootstrap is the compacted current state. After
that succeeds, the old WAL may be archived or removed by the caller. There is
no in-place or online compaction, no multi-table transaction log, and no log
rotation. The configured file, record, and record-count limits are hard caps;
reaching one rejects the mutation before visible state changes.

While a WAL is attached, table/column renames, schema changes, row deletion,
table drop, and snapshot replacement of the logged table are rejected. Other
tables remain ordinary in-memory tables and are not reconstructed by this
single-table WAL.
