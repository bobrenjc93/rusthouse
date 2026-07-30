# Architecture direction

RustHouse should evolve through narrow modules with explicit boundaries:

1. A catalog owns schemas and tables.
2. A columnar storage layer owns typed vectors, validates row shape, and publishes each insert as an immutable part. Ordered tables sort each part and sample sparse primary-key marks.
3. A parser produces a small typed syntax tree without coupling syntax to execution.
4. A query engine plans scans, including leading-key sparse pruning, then filters, projects, groups, aggregates, sorts, or merges compatible ordered parts for a limit.
5. Formats render results without changing execution semantics.
6. Persistence serializes catalog state atomically and rejects corrupt or incompatible data.
7. CLI and HTTP front ends share the same engine API.

The initial engine can be single-process and single-node. Parts and scan ranges are explicit boundaries for future parallel scans, compression, compaction, and a write-ahead log without requiring those mechanisms yet.

Every feature should include end-to-end tests at the SQL boundary. Benchmarks should use reproducible generated data and report enough context to compare future iterations.
