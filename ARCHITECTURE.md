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

High-cardinality grouping keeps its in-memory path until a fixed group-state
budget is reached. Spill I/O is isolated from aggregate semantics: it owns
deterministic hash partitions of row indices, recursive repartitioning,
physical-allocation accounting, private file creation, and cleanup. Recursive
partitions are consumed depth-first so the live file and path count is fixed
independently of group cardinality.
