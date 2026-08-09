# Architecture direction

RustHouse should evolve through narrow modules with explicit boundaries:

1. A catalog owns schemas and tables.
2. A columnar storage layer owns typed vectors and validates row shape.
3. A parser produces a small typed syntax tree without coupling syntax to execution.
4. A query engine plans scans, filters, projections, grouping, aggregation, sorting, and limits.
5. Formats render results without changing execution semantics.
6. Persistence serializes catalog state atomically and rejects corrupt or incompatible data.
7. CLI and HTTP front ends share the same engine API.

The initial engine remains single-process and single-node. A deliberately narrow parallel path reduces global `countIf(Bool)`, sole ungrouped `SUM(Int64)`, `AVG(Int64)`, `MIN(Int64)`, `MIN(Float64)`, `MAX(Int64)`, and `MAX(Float64)`, and the exact ungrouped two-item `COUNT(*)`/`COUNT()` plus `SUM(Int64)` or `AVG(Int64)` shape over large filtered row sets with scoped workers admitted by one process-wide nonblocking budget. The paired shape reuses the SUM/AVG partitions and derives COUNT from the checked filtered cardinality. Each database supplies an additional nonzero lane cap, while a parameterized HTTP query may copy and tighten that cap through `max_threads` without mutating database settings. Hardware and a fixed 16-lane ceiling remain hard upper bounds; grouped execution and other aggregate shapes stay sequential. Public interfaces should leave room for broader parallel scans, compression, and a write-ahead log, but those remain later experiments rather than premature abstractions.

Every feature should include end-to-end tests at the SQL boundary. Benchmarks should use reproducible generated data and report enough context to compare future iterations.
