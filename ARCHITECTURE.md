# Architecture direction

RustHouse should evolve through narrow modules with explicit boundaries:

1. A catalog owns schemas and tables.
2. A columnar storage layer owns typed vectors and validates row shape.
3. A parser produces a small typed syntax tree without coupling syntax to execution.
4. A query engine plans scans, filters, projections, grouping, aggregation, sorting, and limits.
5. Formats render results without changing execution semantics.
6. Persistence publishes versioned, checksummed state atomically and rejects corrupt or incompatible data; future catalog snapshots can reference immutable column segments.
7. CLI and HTTP front ends share the same engine API.

The engine is single-process and single-node. Committed catalogs are immutable,
monotonically numbered generations whose unchanged tables share storage through
`Arc`. Each session transaction pins a generation and stages copy-on-write table
replacements. Commit serializes only publication and persistence, rejects changes to
tables modified since the pinned generation, and merges disjoint writes into the
latest catalog. This is snapshot isolation rather than full serializable isolation.

Persistent databases encode a complete catalog generation in a versioned, checksummed
snapshot. Publication uses a synced temporary file, atomic rename, and parent-directory
sync. A canonical-path sidecar lock in a reserved filename namespace gives one database
handle exclusive ownership across processes. Lock opens do not follow symlinks and
acquire the advisory lock before comparing the opened regular-file identity with the
current path using native file IDs. Database parent directories must preexist,
so publication only needs to sync the directory containing the snapshot. Temporary candidates and backups use a
separate reserved filename prefix that cannot be opened as a database. Encoding validates the same table, column, row, string, and total-allocation
bounds that decoding enforces. The decoder charges raw bytes, catalog/map nodes, table
and `Arc` storage, schema and column vectors, strings, column values, and conservative
allocator overhead before reserving them, and it rejects unknown or malformed data.
Temporary snapshots are owner-only while written. Unix publication restores the prior
UID/GID, mode, and supported ACLs before rename and syncs the parent directory. A
post-rename sync error is reported as durability-uncertain after the in-memory head is
advanced, preventing a published generation from being retried as an active transaction. Windows uses
native replacement calls: `ReplaceFileW` retains security metadata for existing files,
while `MoveFileExW` provides write-through first publication without an invalid
directory-sync step. The temp handle is closed before these calls so Windows can obtain
the required unshared access. Since `ReplaceFileW` lacks a supported metadata durability
barrier, a successful existing-file replacement advances memory but returns a
durability-uncertain error. Replacement always supplies a unique backup name; documented
partial-move failures restore the old file, fall back to publishing the candidate, or
retain both artifacts and report manual recovery rather than deleting either snapshot.

Immutable compressed segments are the durable scan unit. Public interfaces should
leave room for parallel scans, catalog snapshots that atomically reference segments,
and a write-ahead log without introducing those concerns into block encoding.

Every feature should include end-to-end tests at the SQL boundary. Benchmarks should use reproducible generated data and report enough context to compare future iterations.

Bulk formats are a query-independent exception to the future SQL boundary: schema-driven readers emit bounded typed column batches. Transactional ingestion stages those batches in a private binary temporary file and only mutates a destination after the entire source validates; replay rolls back to the original row count on failure. The staging encoding is internal and is not a persistence format.
