# System metadata and observability

RustHouse exposes read-only metadata through SQL and a bounded engine snapshot through HTTP.
The `system` namespace is reserved: `CREATE` in that namespace fails, as do `INSERT` and `DROP`
against a virtual system table. All system-table reads use one committed catalog snapshot; an
active transaction uses its pinned catalog generation rather than a newer concurrent commit.

Snapshots written by older RustHouse versions can contain a quoted table whose literal name starts
with `system.`. Such a real catalog table takes precedence over the virtual table after upgrade and
remains selectable, insertable, and droppable. New colliding tables cannot be created. Dropping the
legacy object completes migration and reveals the virtual table at that name.

## System tables

The following table and column names are stable public fields. They will not be renamed or
removed without a major-version compatibility break. New columns and new metric names may be
added in minor releases.

| Table | Stable columns |
| --- | --- |
| `system.tables` | `database String`, `name String`, `generation Int64`, `column_count Int64`, `row_count Int64`, `logical_bytes Int64` |
| `system.columns` | `database String`, `table String`, `name String`, `ordinal_position Int64`, `data_type String`, `nullable Bool` |
| `system.segments` | `database String`, `table String`, `segment_id String`, `generation Int64`, `row_count Int64`, `logical_bytes Int64` |
| `system.active_queries` | `query_id String`, `query String`, `phase String`, `elapsed_ms Int64`, `scanned_rows Int64`, `scanned_bytes Int64`, `peak_memory_bytes Int64`, `spill_bytes Int64`, `cancelled Bool` |
| `system.engine_metrics` | `metric String`, `value Int64` |

`database` is currently always `default`. A committed table image is the database engine's
immutable logical segment, so `system.segments` contains one row per committed table and its ID is
`<catalog generation>:<table name>`. `logical_bytes` includes schema and owned logical column
storage; it excludes allocator metadata and snapshot-file framing.

Catalog metadata is streamed into the result rather than materialized as an intermediate table.
Cancellation is checked before every metadata row, and the query result limit covers each
temporary row together with already retained output. Table logical size is maintained when data is
loaded or appended, so reading `system.tables` or `system.segments` does not traverse stored values.
Result accounting charges the actual outer row-vector capacity, including spare slots created at
geometric growth boundaries.

`ordinal_position` is one-based. Integer values that exceed SQL `Int64` saturate at `Int64::MAX`;
`query_id` is a decimal string so the full unsigned HTTP request ID remains representable.

## Query fields

The phase values are `queued`, `parsing`, `planning`, `scanning`, and `publishing`. Elapsed time is
monotonic milliseconds since engine execution began. Scanned rows count rows considered by an
engine table scan. Scanned bytes count logical value bytes accessed by predicates and projections,
not filesystem or compressed bytes: `Int64` and `Float64` are 8 bytes, `Bool` is 1 byte,
`String` is its UTF-8 byte length, and `NULL` is 0 bytes. Value-container and allocator overhead is
excluded. Predicates are evaluated left to right, and scanned bytes exclude predicates skipped by
short-circuit evaluation. Peak memory is the largest accounted materialized-result size; allocator
metadata and HTTP serialization buffers are excluded. Spill bytes are zero until an execution
operator uses spill storage. `cancelled` changes as soon as the cooperative cancellation token is
signalled, including while an engine worker is still unwinding.

Active query records retain at most 4,096 UTF-8 bytes of SQL without splitting a code point. The
registry retains at most 1,024 records. `active_queries` remains the exact running-query gauge when
that bound is exceeded, `tracked_active_queries` reports retained records, and
`dropped_active_query_records_total` counts omitted records. These limits are exported as
`MAX_OBSERVED_QUERY_BYTES` and `MAX_ACTIVE_QUERY_ENTRIES`.

## Metrics and logs

`GET /metrics` returns `application/json` with `active_queries` and `engine_metrics`. Scrapes share
the HTTP request, query, and shutdown admission controls; use `query_timeout` as their collection
deadline; and cannot exceed `max_response_bytes`. Collection receives a cooperative cancellation
signal. One dedicated detached metrics worker bounds even a non-cooperative implementation, so a
stalled collector cannot retain query slots or delay Tokio runtime and process shutdown. The active
query objects use the same fields and bounds as `system.active_queries`. Engine metrics have these
stable fields:

- `active_queries` and `tracked_active_queries` are current gauges.
- `queries_total`, `queries_succeeded_total`, `queries_failed_total`, and
  `queries_cancelled_total` are process-lifetime counters.
- `scanned_rows_total`, `scanned_bytes_total`, and `spill_bytes_total` are process-lifetime
  counters rolled up when queries finish.
- `peak_memory_bytes` is the process-lifetime high-water mark for accounted query memory.
- `dropped_active_query_records_total` is the process-lifetime registry overflow counter.

Each query executed through `QueryService` also emits one JSON line to standard error with
`event: "query_finished"`, an `outcome` of `succeeded`, `failed`, or `cancelled`, and the same
bounded query fields. Logs describe engine completion; HTTP result encoding can still fail
afterward. SQL can contain sensitive literals, so operators must protect standard-error output as
they protect query traffic. Serialized lines enter a nonblocking 1,024-entry channel serviced by a
detached writer; full-channel, serialization, and sink failures drop log records and never change a
query, mutation, or shutdown outcome.

The endpoint and tables expose no labels derived from SQL, table names, or query IDs in aggregate
metrics. Per-query cardinality is bounded to 1,024 records. Metadata cardinality is one row per
committed table or column; persisted databases enforce the configured snapshot limits, while an
in-memory database's metadata cardinality follows its catalog. HTTP responses include
`Cache-Control: no-store`.
