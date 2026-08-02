# Architecture

RustHouse is split into narrow modules with explicit boundaries:

1. `database` owns the catalog, configured limits, and query execution.
2. `storage` owns schemas and the four typed column-vector variants. Its batch
   append validates the full batch before mutating any vector.
3. `sql` lexes and parses a typed syntax tree without accessing storage.
4. Query execution scans, filters, projects, groups, aggregates, sorts, and
   limits materialized results.
5. `csv` renders query results without changing execution semantics.
6. Persistence serializes catalog state atomically and rejects corrupt or incompatible data.
7. CLI and HTTP front ends share the same engine API.

The engine is currently single-process, single-node, and in-memory. Public
interfaces leave room for immutable parts, parallel scans, compression, and a
write-ahead log, but those are later work rather than premature abstractions.

Every feature should include end-to-end tests at the SQL boundary. Benchmarks should use reproducible generated data and report enough context to compare future iterations.
