# Architecture direction

RustHouse should evolve through narrow modules with explicit boundaries:

1. A catalog owns schemas and tables.
2. A columnar storage layer owns typed vectors and validates row shape.
3. A parser produces a small typed syntax tree without coupling syntax to execution.
4. A planner resolves SELECT syntax once into typed scan, filter, projection, aggregation,
   sort/top-k, and limit nodes.
5. A plan-consuming executor owns physical row selection, grouping, ordering, and operator metrics.
6. Formats render results without changing execution semantics.
7. Persistence serializes catalog state atomically and rejects corrupt or incompatible data.
8. CLI and HTTP front ends share the same engine API.

The initial engine can be single-process and single-node. Public interfaces should leave room for immutable parts, parallel scans, compression, and a write-ahead log, but those are later experiments rather than premature abstractions.

Every feature should include end-to-end tests at the SQL boundary. Benchmarks should use reproducible generated data and report enough context to compare future iterations.
