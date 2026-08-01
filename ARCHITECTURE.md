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

Bulk formats are a query-independent exception to the future SQL boundary: schema-driven readers emit bounded typed column batches. Transactional ingestion stages those batches in a private binary temporary file and only mutates a destination after the entire source validates; replay rolls back to the original row count on failure. The staging encoding is internal and is not a persistence format.
