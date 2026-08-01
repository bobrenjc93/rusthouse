# SQL scalar and NULL semantics

This document defines the runtime contract of RustHouse's scalar expression
and aggregate subsystem. Errors are returned as `rusthouse::Error`; evaluation
does not panic for data-dependent failures.

Parsed and directly constructed expressions have a maximum depth of
`MAX_EXPRESSION_DEPTH` (currently 128). The parser guards recursive syntax and
left-deep operator chains, and evaluation checks the AST iteratively before
descending. Deeper input returns `Error::ExpressionTooDeep`, and direct ASTs
are destroyed iteratively so rejecting them cannot overflow the call stack.

## Values and NULL

Runtime values are `Int64`, `Float64`, `Bool`, `String`, or `NULL`. `NULL` is
untyped until a surrounding schema assigns a type. It is distinct from zero,
`FALSE`, the empty string, and floating-point NaN.

Arithmetic, comparisons, casts, and ordinary scalar functions return `NULL`
when any required input is `NULL`. The exceptions are:

- `IS NULL` and `IS NOT NULL`, which always return `Bool`;
- `AND`, `OR`, and `NOT`, which implement the SQL three-valued truth tables;
- `CASE`, which treats only `TRUE` as a matching searched condition;
- `COALESCE`, which returns its first non-NULL argument.

Double-quoted names are identifiers even when their contents are keywords, so
`"NULL"`, `"TRUE"`, and `"CASE"` refer to columns rather than SQL syntax.
Quoted bindings are resolved with exact case; unquoted bindings use ASCII
case-folding. Public `Expr::Function` names are also ASCII case-insensitive,
matching parsed SQL.

`CASE` evaluates only conditions through the first match and its selected
result. `COALESCE` evaluates only through its first non-NULL argument.
Evaluation is left-to-right. `FALSE AND ...` and `TRUE OR ...` do not evaluate
their right operand; other logical combinations require both operands.
Ordinary multi-argument functions evaluate every argument before applying
NULL propagation or runtime type validation.

## Operators

`+`, `-`, and `*` preserve `Int64` when both operands are integers. Integer
operations and unary negation are checked; overflow returns `Error::Overflow`.
If either operand is `Float64`, the result is `Float64`. `/` always returns
`Float64`, including for two integers. `%` returns `Int64` for two integers
and `Float64` otherwise. Division or remainder by positive or negative zero
returns `Error::DivideByZero`; a NULL operand still produces NULL.

Floating arithmetic follows IEEE 754 and can produce infinity or NaN rather
than an overflow error. NaN is a non-NULL value. It is unequal to every value,
including itself; `<>` is true and all ordered comparisons involving NaN are
false. Other comparisons accept two numeric values or values of the same
nonnumeric type. Mixed numeric comparison is exact and does not round an
`Int64` to `Float64`. Incompatible domains return `Error::Type` rather than
being converted implicitly.

Logical operators accept only `Bool` and `NULL`. Their truth tables are:

| AND | TRUE | FALSE | NULL |
| --- | --- | --- | --- |
| TRUE | TRUE | FALSE | NULL |
| FALSE | FALSE | FALSE | FALSE |
| NULL | NULL | FALSE | NULL |

| OR | TRUE | FALSE | NULL |
| --- | --- | --- | --- |
| TRUE | TRUE | TRUE | TRUE |
| FALSE | TRUE | FALSE | NULL |
| NULL | TRUE | NULL | NULL |

`NOT TRUE` is `FALSE`, `NOT FALSE` is `TRUE`, and `NOT NULL` is `NULL`.

## Casts

`CAST(value AS type)` supports the names `Int64`, `Float64`, `Bool`, and
`String`, plus common aliases such as `INTEGER`, `DOUBLE`, `BOOLEAN`, and
`TEXT`. NULL casts to NULL. Numeric conversions are explicit: finite floats
truncate toward zero when cast to `Int64`, and an out-of-range value, NaN, or
infinity returns `Error::InvalidCast`. Integers and floats cast to `Bool` by
comparing with zero. Bool casts to numeric zero or one.

String-to-number casts trim surrounding whitespace and require the whole
remaining string to parse. String-to-Bool accepts case-insensitive
`true`/`false`, `t`/`f`, and `1`/`0`. No other implicit string/numeric
conversion is performed.

## String functions

`LOWER`, `UPPER`, `LENGTH`/`CHAR_LENGTH`, `TRIM`, `LTRIM`, and `RTRIM` take
one string. `CONCAT` takes one or more strings. `SUBSTRING`/`SUBSTR` takes a
string, a one-based positive `Int64` start, and an optional nonnegative
`Int64` length. Length and substring positions count Unicode scalar values,
not UTF-8 bytes. Wrong types, arity, or ranges return `Error::InvalidArgument`
or `Error::Type`. Positions beyond the platform index width behave as beyond
the end of the string, and oversized lengths consume the remainder. These
functions propagate NULL, including `CONCAT`.

## Aggregates

`COUNT(*)` (`AggregateFunction::CountAll`) counts every input row.
`COUNT(expression)` counts non-NULL values, including NaN. `SUM`, `MIN`,
`MAX`, and `AVG` ignore NULL inputs and return NULL for empty or all-NULL
input. `COUNT` returns zero in those cases.

Integer `SUM` accumulates exactly in an internal `i128` and checks the final
`Int64` result, so cancellation and overflow do not depend on input order.
Encountering a `Float64` promotes the result to `Float64`; integer and floating
components are retained separately until finalization, so promotion does not
depend on which arrives first. `AVG` always returns `Float64`.
NaN propagates through all numeric aggregates, including `MIN` and `MAX`.
Mixed numeric inputs are comparable, while incompatible MIN/MAX domains and
nonnumeric SUM/AVG inputs return `Error::Aggregate`. Equal mixed numeric
MIN/MAX values use the canonical `Float64` representation, independent of row
order.

## Joins and qualified names

SQL queries support chained `INNER` and `LEFT` hash joins whose `ON` clause is
one or more equality predicates joined by `AND`. Each equality must compare a
column from the new right table with a column already in the left input.
Aliases qualify columns and hide the original table name. An unqualified name
that exists in more than one input returns `Error::AmbiguousColumn`, including
when the tables are empty.

`NULL` and NaN join keys never match. Duplicate build keys produce one result
for every match in build-table row order. A `LEFT JOIN` emits one null-extended
row when no key matches, and its right-side result columns are nullable.

## Window functions

`ROW_NUMBER()` assigns consecutive positions within each partition. `RANK()`
assigns the same value to peers and leaves gaps after ties. Partition and sort
columns are bound before execution, including on empty input. `NULL` partition
keys group together. Window sorts are stable, so row-number assignments within
ties follow source row order; an outer `ORDER BY` defines returned row order.
Ascending order places `NULL` last and descending order places it first unless
`NULLS FIRST` or `NULLS LAST` is explicit.

Windowed `SUM(column)` and `COUNT(column)` ignore `NULL`; `COUNT(*)` includes
every framed row. Empty frames return zero for `COUNT` and `NULL` for `SUM`.
Integer sums use an `i128` prefix and fail if the framed result does not fit
`Int64`. With `ORDER BY`, the omitted frame is running rows from unbounded
preceding through the current row. Without it, the omitted frame covers the
whole partition. Explicit `ROWS` frames accept unbounded, current-row, and
nonnegative preceding/following bounds.

Float64 running frames accumulate in order. Bounded frames are accumulated
from their own rows instead of subtracting rounded floating-point prefixes.

`QueryLimits` rejects a query before a join build exceeds its row ceiling or
before build or expanded-output state exceeds the join byte ceiling. It bounds
every window partition by rows and cumulatively accounts partition hash
entries, index vectors, window output vectors, and temporary accumulators. Join
expansion and final output rows are separately bounded. These failures are
reported as resource-limit errors.
