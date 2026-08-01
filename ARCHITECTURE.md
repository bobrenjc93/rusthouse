# Architecture direction

RustHouse should evolve through narrow modules with explicit boundaries:

1. A catalog owns schemas and tables.
2. A columnar storage layer owns typed vectors and validates row shape.
3. A parser produces a small typed syntax tree without coupling syntax to execution.
4. A query engine plans scans, filters, projections, grouping, aggregation, sorting, and limits.
5. Formats render results without changing execution semantics.
6. Persistence serializes catalog state atomically and rejects corrupt or incompatible data.
7. CLI and HTTP front ends share the same engine API.

The initial engine can be single-process and single-node. Public interfaces should leave room for immutable parts, parallel scans, compression, and a write-ahead log, but those are later experiments rather than premature abstractions.

Every feature should include end-to-end tests at the SQL boundary. Benchmarks should use reproducible generated data and report enough context to compare future iterations.

## Batch execution boundary

The `batch` module is deliberately independent of SQL syntax and scalar expression values. It owns typed, fixed-capacity buffers, validity bitmaps, a selection mask, and dictionary strings. The `kernels` module consumes that boundary directly, so a later planner can compose scans and aggregation without changing storage representation or materializing each row.

Operator memory limits use exact retained payload bytes rather than allocator-specific estimates. Batch limits cover all allocations owned by the batch. Hash-group limits cover the fixed hash table, group slots, key/state slices, and owned string bytes; borrowed input buffers and allocator metadata are outside that total.
