# Architecture direction

RustHouse should evolve through narrow modules with explicit boundaries:

1. A catalog owns schemas and tables.
2. A columnar storage layer owns typed vectors and validates row shape.
3. A parser produces a small typed syntax tree without coupling syntax to execution.
4. A query engine plans scans, filters, projections, grouping, aggregation, sorting, and limits.
5. Formats render results without changing execution semantics.
6. Unix persistence serializes deterministic, versioned catalog snapshots, replaces them atomically while preserving security metadata, and rejects corrupt or incompatible data. Other platforms reject persistent opens until they have an equivalent durable directory-entry protocol.
7. CLI and HTTP front ends share the same engine API.

The engine is single-process and single-node; snapshot files do not yet provide writer locking. Public interfaces should leave room for immutable parts, parallel scans, compression, and a write-ahead log, but those are later experiments rather than premature abstractions.

Every feature should include end-to-end tests at the SQL boundary. Benchmarks should use reproducible generated data and report enough context to compare future iterations.
