# Changelog

## 0.2.0

Filtered aggregates, HAVING expressions, and nullable values extend public AST
and value types. This is a source-breaking release for callers that construct
these types directly or match their variants exhaustively:

- `Select` adds the `having` field.
- `SelectItem::Aggregate` adds the `filter` field.
- `Operand` adds the `Aggregate` variant.
- `Value` and `DataType` add `Null` variants.
- `Column` adds internally constructed `Null` and `Nullable` variants.

The existing `Value::data_type() -> DataType`, `Statement::Select(Select)`, and
single-vector `Column::Int64`, `Float64`, `Bool`, and `String` APIs remain
unchanged.
