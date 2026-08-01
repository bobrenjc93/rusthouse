# Architecture direction

RustHouse should evolve through narrow modules with explicit boundaries:

1. A catalog owns schemas and tables.
2. A columnar storage layer owns typed vectors and validates row shape.
3. A parser produces a small typed syntax tree without coupling syntax to execution.
4. A query engine plans scans, filters, projections, grouping, aggregation, sorting, and limits.
5. Formats render results without changing execution semantics.
6. Persistence uses versioned immutable column segments and rejects corrupt or incompatible data; future catalog snapshots can reference those segments atomically.
7. CLI and HTTP front ends share the same engine API.

The initial engine can be single-process and single-node. Immutable compressed segments are the durable scan unit. Public interfaces should leave room for parallel scans, atomic catalog publication, and a write-ahead log without introducing those concerns into block encoding.

Every feature should include end-to-end tests at the SQL boundary. Benchmarks should use reproducible generated data and report enough context to compare future iterations.
