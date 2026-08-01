# RustHouse

RustHouse is a from-scratch analytical database in Rust, inspired by the useful core of ClickHouse: typed columnar data, fast scans and aggregations, a practical SQL surface, and an operational interface that is easy to embed and understand.

This is intentionally not a wire-compatible ClickHouse clone. The goal is to grow a credible small competitor through measured, reviewed iterations while keeping the implementation approachable.

## Product target

The first useful release should support:

- typed tables with `Int64`, `Float64`, `Bool`, and `String` columns;
- a genuinely columnar in-memory representation;
- `CREATE TABLE`, `INSERT INTO ... VALUES`, and `SELECT`;
- projections, `WHERE` comparisons, `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`, `GROUP BY`, `ORDER BY`, and `LIMIT`;
- a batch/interactive CLI with readable table, CSV, and JSON output;
- durable local snapshots with an explicit, documented file format;
- an HTTP endpoint for executing SQL;
- deterministic tests and a small benchmark that demonstrate analytical behavior.

The early implementation should favor Rust's standard library and a small dependency surface. Correctness, clear errors, bounded resource use, and a modular path toward vectorized execution matter more than superficial feature count.

## Development model

RustHouse is the dogfood project for [Burner](https://github.com/bobrenjc93/burner). Plain-language repository evaluations establish a baseline. Burner then gives isolated implementation ideas to Codex authors, runs an independent reviewer/author revision loop until approval, reruns the evaluations on the exact candidate branch, and opens impact-stamped pull requests.

```bash
cargo test
cargo run -- --help
```

## HTTP query service

The `rusthouse::http` module exposes a long-lived HTTP/1.1 server behind the
engine-independent `QueryService` trait. An engine implements that trait and
returns a `QueryResult`; the frontend owns protocol parsing, output encoding,
admission control, deadlines, cancellation, and shutdown.

`POST /query` accepts UTF-8 SQL as `application/sql`, or a JSON object such as
`{"query":"SELECT 1"}` with `application/json`. A Content-Type is required and
browser-originated requests are rejected; the endpoint intentionally does not
accept browser-safelisted form or `text/plain` submissions. Select an output with
`Accept` or with `?format=`:

| Format | `Accept` value |
| --- | --- |
| JSON array of row objects | `application/json` |
| newline-delimited row objects | `application/x-ndjson` |
| CSV with a header row | `text/csv` |

Errors are JSON objects with stable codes, messages, and request IDs. The same
request ID is returned in `x-request-id`. `GET /health/live` checks the process;
`GET /health/ready` and `GET /health` report `QueryService::health()`.

The server defaults to 1 MiB requests, 16 MiB encoded responses, 16 concurrent
queries, 64 concurrent HTTP query requests, a 10 second body-read deadline, a 30
second query deadline, 128 accepted connections, a 10 second header deadline, a
60 second connection idle deadline, and a 10 second graceful shutdown window.
Connection admission is held through response delivery, while the idle deadline
closes stalled headers, keep-alive clients, and response readers. Busy request or
execution slots fail immediately with `503` and `Retry-After`, so slow clients do
not create an unbounded queue. Transient accept errors are retried with capped
backoff, and owners can await unexpected server-task termination. Query futures
and result encoding run on bounded blocking workers; timed-out jobs retain their
execution slot until they actually exit. Configured timeouts are capped at 365
days so deadline arithmetic remains representable.
Query implementations should observe the supplied `QueryCancellation` while doing
expensive work. It is signaled when a request is dropped, its deadline expires, or
forced shutdown begins. Ctrl-C and SIGTERM both use the bounded graceful-shutdown
path.

The executable currently has no SQL engine to attach, so its readiness check and
queries report `503`; it is provided to exercise deployment and transport wiring:

```bash
cargo run -- serve --bind 127.0.0.1:8080 --max-concurrent-queries 8
```

The repository begins as a deliberately tiny seed. Substantial functionality should arrive through Burner-managed pull requests so the measured history remains visible.
