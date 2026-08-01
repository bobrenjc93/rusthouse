# Architecture direction

RustHouse should evolve through narrow modules with explicit boundaries:

1. A catalog owns schemas and tables.
2. A columnar storage layer owns typed vectors and validates row shape.
3. A parser produces a small typed syntax tree without coupling syntax to execution.
4. A query engine plans scans, filters, projections, grouping, aggregation, sorting, and limits.
5. Formats render results without changing execution semantics.
6. Persistence serializes catalog state atomically and rejects corrupt or incompatible data.
7. CLI and HTTP front ends share the same engine API.

The engine is single-process and single-node. Committed catalogs are immutable,
monotonically numbered generations whose unchanged tables share storage through
`Arc`. Each session transaction pins a generation and stages copy-on-write table
replacements. Commit serializes only publication and persistence, rejects changes to
tables modified since the pinned generation, and merges disjoint writes into the
latest catalog. This is snapshot isolation rather than full serializable isolation.

Persistent databases encode a complete catalog generation in a versioned, checksummed
snapshot. Publication uses a synced temporary file, atomic rename, and parent-directory
sync. A canonical-path sidecar lock gives one database handle exclusive ownership across
processes. Encoding validates the same table, column, row, string, and total-allocation
bounds that decoding enforces. The decoder charges raw bytes, catalog/map nodes, table
and `Arc` storage, schema and column vectors, strings, column values, and conservative
allocator overhead before reserving them, and it rejects unknown or malformed data.
Temporary snapshots are owner-only while written. Unix publication restores the prior
mode and supported ACLs before rename and syncs the parent directory. Windows uses
native replacement calls: `ReplaceFileW` retains security metadata for existing files,
while `MoveFileExW` provides write-through first publication without an invalid
directory-sync step.

Every feature should include end-to-end tests at the SQL boundary. Benchmarks should use reproducible generated data and report enough context to compare future iterations.
