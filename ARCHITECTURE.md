# Architecture direction

RustHouse should evolve through narrow modules with explicit boundaries:

1. A catalog owns schemas and tables.
2. A columnar storage layer owns typed vectors and validates row shape.
3. A parser produces a small typed syntax tree without coupling syntax to execution.
4. A query engine plans scans, filters, projections, grouping, aggregation, sorting, and limits.
5. Formats render results without changing execution semantics.
6. Persistence serializes catalog state atomically and rejects corrupt or incompatible data.
7. CLI and HTTP front ends share the same engine API.

The private `batch::scalar_cast` module owns allocation-free validation and parsing for
String-to-`Int64`, String-to-`Float64`, and String-to-`Bool` casts, including typed malformed
versus overflow errors and the normalized decimal comparisons used while ordering values before
range conversion. The execution engine remains responsible for row selection, ordering-state
limits, pagination, and result allocation; this keeps scalar text semantics reusable without
exposing a new public API or moving query policy out of the engine.

The private `batch::scalar_text` module is the inverse boundary for `Int64`, finite `Float64`, and
`Bool`: it owns their canonical `CAST`/`toString` representation, exact payload byte length, and
allocation-free lexicographic comparison. The execution engine continues to own type dispatch,
typed `NULL` propagation, pre-allocation result-byte accounting, and SQL ordering and pagination.
Focused module tests keep each optimized length and ordering primitive differential against the
canonical rendered text; SQL-boundary tests retain the `CAST`/`toString` differential, signed-zero
and extrema coverage, and exact result-limit behavior.

The private `batch::scalar_string` module owns allocation-free ASCII-folded comparison for
`LOWER` and `UPPER` ordering and checked conversion of String byte and Unicode-scalar lengths to
`Int64`. ASCII folding deliberately leaves non-ASCII UTF-8 bytes unchanged, while `lengthUTF8`
counts Unicode scalar values rather than bytes or grapheme clusters. The execution engine retains
physical row access, projection allocation, stable source-row tie breaking, ordering and
pagination, and result-limit accounting. Focused module tests compare the primitives with Rust's
canonical allocating ASCII transforms and byte/scalar counts; SQL-boundary tests continue to
cover Unicode projection results, ordering ties, pagination, and resource limits.

The private `batch::scalar_nullable_int64` module owns the value-level primitives for nullable
`Int64` `ABS`, column-minus-literal subtraction, and `ifNull`, including checked overflow, typed
`NULL` propagation, and allocation-free absolute-value ordering. The execution engine retains type
resolution, physical row access, deferred projection after ordering and pagination, grouping
policy, and result allocation. Focused module tests cover the typed value contract and extrema;
SQL-boundary tests remain responsible for differential projection and ordering behavior, deferred
overflow, pagination, resource limits, and output formats.

The private `batch::scalar_float64` module owns the pure value transforms and allocation-free
comparisons for finite `Float64` `ABS`, `ROUND`, `FLOOR`, and `CEIL`. Its comparisons preserve
numeric equality for signed zero and other transformed ties. Storage and ingestion retain finite
value validation; the execution engine retains type resolution, physical row access, stable
source-row tie breaking, deferred projection after ordering and pagination, result accounting, and
allocation. Focused module tests cover signed zero, subnormal values, extrema, halfway rounding,
and comparison ties; the existing SQL-boundary differential and resource-limit tests continue to
cover complete query behavior.

The private `batch::aggregate_scheduler` module owns process-wide aggregate-worker admission and
deterministic contiguous partitioning. Its generic grouped-aggregate driver additionally owns
scoped worker spawning, ordered partial collection, admission release, and complete-input fallback
after a spawn failure or worker panic. The scheduler has no SQL or aggregate-state policy. The private
`batch::grouped_bool_count` module owns the Bool-grouped `COUNT`/`countIf` partial, row-count,
nullable-column and `countIf` chunk scans, checked ordered reduction, and fixed worker-name prefix;
physical non-nullable `COUNT(column)` reuses its row-count scan without reading argument values. The
private `batch::grouped_bool_min` module similarly owns the non-nullable `Int64` and `Float64` `MIN`
partials, chunk scans, ordered reduction, and fixed worker-name prefixes. The private
`batch::grouped_bool_max` module owns the corresponding non-nullable `Int64` and `Float64` `MAX`
reduction boundary. The private
`batch::grouped_bool_sum_avg` module owns the non-nullable `Int64` `SUM`/`AVG` per-key sum-and-count
partial, chunk scan, first-seen ordering, mode-specific checked reduction and fixed worker-name
prefixes. The private `batch::global_int64_sum_avg` module owns the corresponding global nullable
and non-nullable raw-slice scans, sum-and-count partial, mode-specific overflow contexts, ordered
reduction, and scheduler integration. The engine retains SQL eligibility, physical column dispatch,
grouped resource limits, and typed result construction, while the scheduler continues to own
admission, worker lifecycle, and complete-input fallback.

The initial engine remains single-process and single-node. Validated `Int64` range partitions are
local table metadata used only to prune impossible physical row ranges before the existing exact
SELECT executor; they do not introduce distributed partition routing, sharding, replication, or
durable partition manifests. Mutations invalidate that metadata and restore the complete scan path.
A deliberately narrow parallel path reduces global `countIf(Bool)`, sole ungrouped
`COUNT(Nullable(Int64))`, sole ungrouped non-nullable `SUM(Int64)`, `AVG(Int64)`, `MIN(Int64)`,
`MIN(Float64)`, `MAX(Int64)`, and `MAX(Float64)`, sole ungrouped `SUM(Nullable(Int64))`,
`MIN(Nullable(Int64))`, `MAX(Nullable(Int64))`, and `AVG(Nullable(Int64))`, the exact ungrouped
two-item `COUNT(*)`/`COUNT()` plus `SUM(Int64)`, `AVG(Int64)`, `MIN(Int64)`, or `MAX(Int64)`
(including physical `Nullable(Int64)`) or plus non-nullable `MIN(Float64)` or `MAX(Float64)`, the
exact ungrouped `COUNT(nullable_int64_column)` plus `SUM(the_same_column)` or
`AVG(the_same_column)`, and sole non-nullable `Int64` `SUM` or `AVG` or non-nullable `Int64` or
`Float64` `MIN` or `MAX` grouped by one physical Bool key over large filtered row sets with scoped
workers admitted by one process-wide nonblocking budget.
Nullable COUNT chunks ignore absent values and use checked count reduction; nullable SUM/AVG
chunks ignore absent values while preserving checked i128 sum and present-count reduction; nullable
MIN/MAX chunks ignore absent values and reduce optional extrema. The paired shapes reuse the
corresponding aggregate partitions. Row-count pairs derive COUNT from the checked filtered
cardinality, while same-column nullable COUNT/SUM and COUNT/AVG pairs derive COUNT from the checked
present-count partial. Each database supplies an additional nonzero lane cap, while a parameterized
HTTP query may copy and tighten that cap through `max_threads` without mutating database settings.
Parameterized queries can likewise copy and tighten the configured ordering-state byte cap through
`max_ordering_state_bytes`; the request-local copy is applied before allocating supported ordering
caches and never mutates settings. Hardware and a fixed 16-lane ceiling remain hard upper bounds;
grouped shapes other than the supported Bool `COUNT`/`SUM`/`MIN`/`MAX`/`AVG` cases, different-column
COUNT pairs, and other multi-aggregate nullable projections stay sequential. On Unix, one opted-in,
one-column `Int64` or `Nullable(Int64)` table can use a bounded, checksummed, fsync-ordered WAL for
crash-safe appends, truncates, and replacements. A registry can durably publish a caller-bounded set
of those independent table WALs and recover their complete committed prefixes as one catalog unit.
Registry recovery is bounded and atomic at the catalog level: manifest, per-table, aggregate-byte,
and aggregate-record bounds are checked while all tables and metrics are staged, and any member
failure returns no database. Single-file and registry recovery are read-only and do not attach a
writer. Registry logging does not make mutations spanning member tables transactional, and there is
no in-place compaction or log rotation; recovery can instead be compacted or resumed by enabling a
new WAL or registry at a new path. Snapshots continue to cover the broader persistence surface.
Public interfaces should leave room for broader parallel scans, compression, transactional
multi-table logging, and rotation without prematurely coupling those concerns.

The parameterized HTTP path also copies and tightens the configured aggregate-state byte cap through `max_aggregate_state_bytes`. The request-local copy feeds the existing fixed and dynamic aggregate-state accounting and never mutates database settings or lock behavior.

Every feature should include end-to-end tests at the SQL boundary. Benchmarks should use reproducible generated data and report enough context to compare future iterations.
