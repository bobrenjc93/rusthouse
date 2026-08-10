# Architecture direction

RustHouse should evolve through narrow modules with explicit boundaries:

1. A catalog owns schemas and tables.
2. A columnar storage layer owns typed vectors and validates row shape.
3. A parser produces a small typed syntax tree without coupling syntax to execution.
4. A query engine plans scans, filters, projections, grouping, aggregation, sorting, and limits.
5. Formats render results without changing execution semantics.
6. Persistence serializes catalog state atomically and rejects corrupt or incompatible data.
7. CLI and HTTP front ends share the same engine API.

The initial engine remains single-process and single-node. Validated `Int64` range partitions are local table metadata used only to prune impossible physical row ranges before the existing exact SELECT executor; they do not introduce distributed partition routing, sharding, replication, or durable partition manifests. Mutations invalidate that metadata and restore the complete scan path. A deliberately narrow parallel path reduces global `countIf(Bool)`, sole ungrouped `SUM(Int64)`, `AVG(Int64)`, `MIN(Int64)`, `MIN(Float64)`, `MAX(Int64)`, and `MAX(Float64)`, and the exact ungrouped two-item `COUNT(*)`/`COUNT()` plus `SUM(Int64)`, `AVG(Int64)`, `MIN(Int64)`, `MIN(Float64)`, `MAX(Int64)`, or `MAX(Float64)` shapes over large filtered row sets with scoped workers admitted by one process-wide nonblocking budget. The paired shapes reuse the corresponding aggregate partitions and derive COUNT from the checked filtered cardinality. Each database supplies an additional nonzero lane cap, while a parameterized HTTP query may copy and tighten that cap through `max_threads` without mutating database settings. Hardware and a fixed 16-lane ceiling remain hard upper bounds; grouped execution and other aggregate shapes stay sequential.

On Unix, an opted-in one-column `Int64` or programmatic `Nullable(Int64)` table can use a bounded, checksummed, fsync-ordered WAL for crash-safe appends, truncates, and replacements, including transitions to and from `NULL`. A bounded multi-table registry durably publishes independently logged tables as one recovery unit: its manifest and member files have per-table and directory-wide limits. Recovery is atomic at the registry boundary because it stages every table and cached metric before returning a database, so a missing, corrupt, inconsistent, or over-limit member exposes no partial catalog. Recovery remains read-only and does not resume logging automatically. Registry members do not form a cross-table transaction log, and an atomic INSERT batch spanning multiple logged members is rejected before any WAL write. There is no in-place or online compaction and no log rotation; compaction or resumed durability requires enabling a new WAL or registry at a new path after recovery. Snapshots continue to cover the broader persistence surface. Public interfaces should leave room for broader parallel scans, compression, and transactional multi-table logging without prematurely coupling those concerns.

Every feature should include end-to-end tests at the SQL boundary. Benchmarks should use reproducible generated data and report enough context to compare future iterations.
