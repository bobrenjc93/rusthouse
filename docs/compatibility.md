# Compatibility and Feature Reference

## Toolchain and stability

RustHouse uses Rust 2024 and supports Rust 1.89 or newer. The repository pins
Rust 1.89.0, including rustfmt and Clippy, as the CI toolchain. Public Rust APIs
follow Cargo semver conventions, but the crate is pre-1.0 and minor releases
may contain breaking API or file-format changes when called out explicitly.

## Public API

| Area | Entry points | Contract |
| --- | --- | --- |
| Catalog | `Catalog`, `CatalogLimits` | Owns named tables and executes bounded SQL. Table names are ASCII case-insensitive; column names are case-sensitive. |
| Storage | `Table`, `Field`, `DataType`, `Value` | Columnar `Int64`, `Float64`, `Bool`, and UTF-8 `String` storage with atomic batch insertion. |
| SQL | `parse_create_table`, `parse_insert`, `parse_select` and limit-aware variants | Produces typed syntax trees with byte, row, projection, predicate, and ordering limits. |
| Results | `SelectResult`, `write_select_csv_with_names` | Streams projections and scalar results; grouped keys are owned and sorted deterministically. |
| Primitives | `Table::scan`, reductions, `Table::grouped_count` | Selection-aware comparisons, aggregates, and one-column grouped counts. |
| Persistence | `SnapshotStore`, catalog save/load methods | Reads and atomically replaces one bounded table snapshot at an explicit path. |

## SQL surface

Supported statements are `CREATE TABLE`, `INSERT INTO ... VALUES`, and
`SELECT`. Selects support named or wildcard projections, comparison predicates
joined by `AND` and `OR`, scalar `COUNT(*)`, `SUM`, `AVG`, `MIN`, and `MAX`, one
column `GROUP BY` with `COUNT(*)`, multi-column `ORDER BY`, and `LIMIT`.

Grouped ordering, grouped limits, `HAVING`, `NULL`, joins, subqueries,
expressions, quoted identifiers, transactions, and schema alteration are not
supported. Empty `AVG`, `MIN`, and `MAX` inputs return typed execution errors
because the value model has no `NULL`.

## Snapshot formats

Snapshots contain two independently versioned little-endian layers:

1. Envelope version 1 begins with `RHSNAP\0\0`, a `u16` version, a `u64`
   payload length, a `u32` CRC-32/ISO-HDLC checksum, and the payload. Readers
   reject oversized, truncated, trailing, corrupt, and unsupported data before
   returning bytes.
2. Table payload version 1 begins with `RHTABLE\0`, a `u16` version, `u64` row
   limit, row count, and field count. Schema entries contain a one-byte type tag
   and a length-prefixed UTF-8 name. Physical columns repeat the type tag and
   value count. Fixed-width values are little-endian; strings are
   length-prefixed UTF-8. Type tags 1 through 4 mean `Int64`, `Float64`, `Bool`,
   and `String`.

Writers use a sibling lock and temporary file before atomic replacement.
Fallback reads are explicit: only missing, truncated, trailing, or
checksum-invalid primary envelopes are eligible. Invalid magic, unsupported
versions, size-limit failures, other I/O errors, and invalid decoded table
payloads are returned directly. Unknown future versions are rejected rather
than guessed.
