//! Bounded recursive-descent implementations for the public SQL parsers.

use std::collections::HashMap;

use super::*;

/// Parses one bounded `CREATE TABLE` statement using the default limits.
pub fn parse_create_table(input: &str) -> Result<CreateTableStatement, ParseError> {
    parse_create_table_with_limits(input, ParseLimits::default())
}

/// Parses one bounded `CREATE TABLE` statement.
///
/// Keywords and data types are case-insensitive. Unquoted identifiers must match
/// `[A-Za-z_][A-Za-z0-9_]*`. One trailing semicolon is accepted as a statement
/// terminator, but comments, quoted identifiers, and additional statements are
/// outside this parser's intentionally narrow SQL surface.
pub fn parse_create_table_with_limits(
    input: &str,
    limits: ParseLimits,
) -> Result<CreateTableStatement, ParseError> {
    if input.len() > limits.max_input_bytes {
        return Err(ParseError {
            position: limits.max_input_bytes,
            kind: ParseErrorKind::InputTooLong {
                limit: limits.max_input_bytes,
                actual: input.len(),
            },
        });
    }

    Parser::new(input).parse_create_table(limits.max_columns)
}

/// Parses one bounded `INSERT INTO ... VALUES` statement using the default limits.
pub fn parse_insert(input: &str) -> Result<InsertStatement, ParseError> {
    parse_insert_with_limits(input, InsertParseLimits::default())
}

/// Parses one bounded `INSERT INTO ... VALUES` statement.
///
/// The parser accepts one or more non-empty rows containing `Int64`, `Float64`,
/// `Bool`, and single-quoted `String` literals. A quote inside a string is
/// escaped by doubling it (`'can''t'`). String limits apply to decoded UTF-8
/// bytes, after doubled quotes have been reduced to one byte. Catalog lookup,
/// schema validation, and table mutation are deliberately outside this syntax
/// boundary.
pub fn parse_insert_with_limits(
    input: &str,
    limits: InsertParseLimits,
) -> Result<InsertStatement, ParseError> {
    if input.len() > limits.max_input_bytes {
        return Err(ParseError {
            position: limits.max_input_bytes,
            kind: ParseErrorKind::InputTooLong {
                limit: limits.max_input_bytes,
                actual: input.len(),
            },
        });
    }

    Parser::new(input).parse_insert(limits)
}

/// Parses one bounded `SELECT` statement using the default limits.
pub fn parse_select(input: &str) -> Result<SelectStatement, ParseError> {
    parse_select_with_limits(input, SelectParseLimits::default())
}

/// Parses one bounded `SELECT` statement.
///
/// Projections are `*`, a non-empty list of unquoted column names, or a
/// non-empty aggregate-only list containing `COUNT(*)`, `COUNT(DISTINCT
/// column)`, `SUM(column)`, `AVG(column)`, `MIN(column)`, and `MAX(column)`.
/// Every aggregate may have an `AS` alias. A one-column grouped count has the
/// exact projection `key, COUNT(*) [AS alias]` and requires a matching `GROUP
/// BY key`. The statement reads one table and may contain `WHERE` groups joined
/// by `OR`, each containing column-to-literal comparisons joined by `AND`. One
/// optional pair of parentheses may wrap each whole group. Literals may be
/// `Int64`, `Float64`, `Bool`, or `String`. The clause may be followed by one
/// bounded `ORDER BY` list of `column [ASC|DESC]` keys and a nonnegative integer
/// `LIMIT`. `NOT`, nested expressions, other grouped aggregates, multiple
/// grouping keys, grouped ordering, `HAVING`, raw-column/aggregate mixing, and
/// other predicate or result forms are outside this intentionally narrow syntax
/// boundary.
pub fn parse_select_with_limits(
    input: &str,
    limits: SelectParseLimits,
) -> Result<SelectStatement, ParseError> {
    if input.len() > limits.max_input_bytes {
        return Err(ParseError {
            position: limits.max_input_bytes,
            kind: ParseErrorKind::InputTooLong {
                limit: limits.max_input_bytes,
                actual: input.len(),
            },
        });
    }

    Parser::new(input).parse_select(limits)
}

struct Parser<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn parse_create_table(
        mut self,
        max_columns: usize,
    ) -> Result<CreateTableStatement, ParseError> {
        self.parse_keyword("CREATE")?;
        self.parse_keyword("TABLE")?;
        let (name, _) = self.parse_identifier(IdentifierContext::Table)?;
        self.expect_byte(b'(', "'('")?;

        let mut columns = Vec::new();
        let mut column_positions = HashMap::new();

        loop {
            self.skip_whitespace();
            if self.peek().is_none() || matches!(self.peek(), Some(b')' | b',')) {
                return Err(self.error(ParseErrorKind::EmptyColumn));
            }
            if columns.len() == max_columns {
                return Err(self.error(ParseErrorKind::TooManyColumns { limit: max_columns }));
            }

            let (column_name, column_position) =
                self.parse_identifier(IdentifierContext::Column)?;
            let normalized_name = column_name.to_ascii_lowercase();
            if let Some(first_position) = column_positions.get(&normalized_name) {
                return Err(ParseError {
                    position: column_position,
                    kind: ParseErrorKind::DuplicateColumn {
                        name: column_name,
                        first_position: *first_position,
                    },
                });
            }

            let data_type = self.parse_data_type()?;
            column_positions.insert(normalized_name, column_position);
            columns.push(ColumnDefinition {
                name: column_name,
                data_type,
            });

            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b')') => {
                    self.position += 1;
                    break;
                }
                _ => {
                    return Err(self.error(ParseErrorKind::ExpectedToken {
                        expected: "',' or ')'",
                    }));
                }
            }
        }

        self.skip_whitespace();
        if self.peek() == Some(b';') {
            self.position += 1;
            self.skip_whitespace();
        }
        if self.peek().is_some() {
            return Err(self.error(ParseErrorKind::TrailingSyntax));
        }

        Ok(CreateTableStatement { name, columns })
    }

    fn parse_insert(mut self, limits: InsertParseLimits) -> Result<InsertStatement, ParseError> {
        self.parse_keyword("INSERT")?;
        self.parse_keyword("INTO")?;
        let (name, _) = self.parse_identifier(IdentifierContext::Table)?;
        self.parse_keyword("VALUES")?;

        let mut rows = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'(') {
                return Err(self.error(ParseErrorKind::ExpectedToken { expected: "'('" }));
            }
            if rows.len() == limits.max_rows {
                return Err(self.error(ParseErrorKind::TooManyRows {
                    limit: limits.max_rows,
                }));
            }
            self.position += 1;

            let row = self.parse_row(limits)?;
            rows.push(row);

            self.skip_whitespace();
            if self.peek() == Some(b',') {
                self.position += 1;
                continue;
            }
            break;
        }

        self.finish_statement()?;
        Ok(InsertStatement { name, rows })
    }

    fn parse_select(mut self, limits: SelectParseLimits) -> Result<SelectStatement, ParseError> {
        self.parse_keyword("SELECT")?;
        let projections = self.parse_projections(limits.max_projections)?;
        self.parse_keyword("FROM")?;
        let (table, _) = self.parse_identifier(IdentifierContext::Table)?;

        self.skip_whitespace();
        let predicate_groups = if self.peek_token_is("WHERE") {
            self.parse_keyword("WHERE")?;
            self.parse_predicate_groups(
                limits.max_predicates,
                limits.max_predicate_groups,
                limits.max_input_bytes,
            )?
        } else {
            Vec::new()
        };

        self.skip_whitespace();
        if let SelectProjection::GroupedCount { key, .. } = &projections {
            self.parse_keyword("GROUP")?;
            self.parse_keyword("BY")?;
            let (grouped_key, grouped_key_position) =
                self.parse_identifier(IdentifierContext::Column)?;
            if grouped_key != *key {
                return Err(ParseError {
                    position: grouped_key_position,
                    kind: ParseErrorKind::GroupKeyMismatch {
                        projected: key.clone(),
                        grouped: grouped_key,
                    },
                });
            }
        }

        self.skip_whitespace();
        let grouped = matches!(projections, SelectProjection::GroupedCount { .. });
        let order_by = if !grouped && self.peek_token_is("ORDER") {
            self.parse_keyword("ORDER")?;
            self.parse_keyword("BY")?;
            Some(self.parse_order_keys(limits.max_order_keys)?)
        } else {
            None
        };

        self.skip_whitespace();
        let limit = if !grouped && self.peek_token_is("LIMIT") {
            self.parse_keyword("LIMIT")?;
            Some(self.parse_limit()?)
        } else {
            None
        };

        self.finish_statement()?;
        Ok(SelectStatement {
            projections,
            table,
            predicate_groups,
            order_by,
            limit,
        })
    }

    fn parse_order_keys(
        &mut self,
        max_order_keys: usize,
    ) -> Result<Vec<OrderByClause>, ParseError> {
        let mut order_keys = Vec::new();
        loop {
            self.skip_whitespace();
            if order_keys.len() == max_order_keys {
                return Err(self.error(ParseErrorKind::TooManyOrderKeys {
                    limit: max_order_keys,
                }));
            }

            let (column, _) = self.parse_identifier(IdentifierContext::Column)?;
            self.skip_whitespace();
            let direction = if self.peek_token_is("ASC") {
                self.parse_keyword("ASC")?;
                OrderDirection::Ascending
            } else if self.peek_token_is("DESC") {
                self.parse_keyword("DESC")?;
                OrderDirection::Descending
            } else {
                OrderDirection::Ascending
            };
            order_keys.push(OrderByClause { column, direction });

            self.skip_whitespace();
            if self.peek() != Some(b',') {
                break;
            }
            self.position += 1;
        }
        Ok(order_keys)
    }

    fn parse_projections(
        &mut self,
        max_projections: usize,
    ) -> Result<SelectProjection, ParseError> {
        self.skip_whitespace();
        if self.peek() == Some(b'*') {
            if max_projections == 0 {
                return Err(self.error(ParseErrorKind::TooManyProjections {
                    limit: max_projections,
                }));
            }
            self.position += 1;
            return Ok(SelectProjection::All);
        }

        if self.peek().is_none() || self.peek() == Some(b',') || self.peek_token_is("FROM") {
            return Err(self.error(ParseErrorKind::ExpectedProjection));
        }

        let (first, first_position) = self.parse_identifier(IdentifierContext::Column)?;
        self.skip_whitespace();
        if let Some(kind) = aggregate_kind(&first)
            && self.peek() == Some(b'(')
        {
            return self.parse_aggregate_projections(kind, first_position, max_projections);
        }
        if max_projections == 0 {
            return Err(ParseError {
                position: first_position,
                kind: ParseErrorKind::TooManyProjections {
                    limit: max_projections,
                },
            });
        }

        let mut columns = vec![first];
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b',') {
                break;
            }
            self.position += 1;
            self.skip_whitespace();
            if self.peek().is_none() || self.peek() == Some(b',') || self.peek_token_is("FROM") {
                return Err(self.error(ParseErrorKind::ExpectedProjection));
            }
            if self.peek() == Some(b'*') {
                return Err(self.error(ParseErrorKind::MixedAggregateProjection));
            }
            if columns.len() == max_projections {
                return Err(self.error(ParseErrorKind::TooManyProjections {
                    limit: max_projections,
                }));
            }

            let (column, column_position) = self.parse_identifier(IdentifierContext::Column)?;
            self.skip_whitespace();
            if let Some(kind) = aggregate_kind(&column)
                && self.peek() == Some(b'(')
            {
                if columns.len() == 1 && kind == AggregateKind::Count {
                    let aggregate = self.parse_aggregate(kind)?;
                    if !matches!(aggregate.function, AggregateFunction::CountAll) {
                        return Err(ParseError {
                            position: column_position,
                            kind: ParseErrorKind::MixedAggregateProjection,
                        });
                    }
                    return Ok(SelectProjection::GroupedCount {
                        key: columns.pop().expect("one grouping key was parsed"),
                        alias: aggregate.alias,
                    });
                }
                return Err(ParseError {
                    position: column_position,
                    kind: ParseErrorKind::MixedAggregateProjection,
                });
            }
            columns.push(column);
        }

        Ok(SelectProjection::Columns(columns))
    }

    fn parse_aggregate_projections(
        &mut self,
        first_kind: AggregateKind,
        first_position: usize,
        max_projections: usize,
    ) -> Result<SelectProjection, ParseError> {
        if max_projections == 0 {
            return Err(ParseError {
                position: first_position,
                kind: ParseErrorKind::TooManyProjections {
                    limit: max_projections,
                },
            });
        }

        let mut aggregates = vec![self.parse_aggregate(first_kind)?];
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b',') {
                break;
            }
            self.position += 1;
            self.skip_whitespace();
            if self.peek().is_none() || self.peek() == Some(b',') || self.peek_token_is("FROM") {
                return Err(self.error(ParseErrorKind::ExpectedProjection));
            }
            if self.peek() == Some(b'*') {
                return Err(self.error(ParseErrorKind::MixedAggregateProjection));
            }
            if aggregates.len() == max_projections {
                return Err(self.error(ParseErrorKind::TooManyProjections {
                    limit: max_projections,
                }));
            }

            let (function, function_position) = self.parse_identifier(IdentifierContext::Column)?;
            self.skip_whitespace();
            let Some(kind) = aggregate_kind(&function) else {
                return Err(ParseError {
                    position: function_position,
                    kind: ParseErrorKind::MixedAggregateProjection,
                });
            };
            if self.peek() != Some(b'(') {
                return Err(ParseError {
                    position: function_position,
                    kind: ParseErrorKind::MixedAggregateProjection,
                });
            }
            aggregates.push(self.parse_aggregate(kind)?);
        }

        if aggregates.len() == 1 && matches!(aggregates[0].function, AggregateFunction::CountAll) {
            let aggregate = aggregates.pop().expect("one aggregate was parsed");
            return Ok(SelectProjection::CountAll {
                alias: aggregate.alias,
            });
        }

        Ok(SelectProjection::Aggregates(aggregates))
    }

    fn parse_aggregate(&mut self, kind: AggregateKind) -> Result<AggregateProjection, ParseError> {
        self.position += 1;
        let function = match kind {
            AggregateKind::Count => {
                self.skip_whitespace();
                if self.peek() == Some(b'*') {
                    self.position += 1;
                    AggregateFunction::CountAll
                } else {
                    self.parse_keyword("DISTINCT")?;
                    AggregateFunction::CountDistinct {
                        column: self.parse_identifier(IdentifierContext::Column)?.0,
                    }
                }
            }
            AggregateKind::Sum => AggregateFunction::Sum {
                column: self.parse_identifier(IdentifierContext::Column)?.0,
            },
            AggregateKind::Avg => AggregateFunction::Avg {
                column: self.parse_identifier(IdentifierContext::Column)?.0,
            },
            AggregateKind::Min => AggregateFunction::Min {
                column: self.parse_identifier(IdentifierContext::Column)?.0,
            },
            AggregateKind::Max => AggregateFunction::Max {
                column: self.parse_identifier(IdentifierContext::Column)?.0,
            },
        };
        self.expect_byte(b')', "')'")?;

        let alias = if self.peek_token_is("AS") {
            self.parse_keyword("AS")?;
            Some(self.parse_identifier(IdentifierContext::Column)?.0)
        } else {
            None
        };

        Ok(AggregateProjection { function, alias })
    }

    fn parse_comparison(
        &mut self,
        max_string_bytes: usize,
    ) -> Result<ComparisonPredicate, ParseError> {
        let column = self.parse_comparison_column()?;
        let operator = self.parse_comparison_operator()?;
        let value = self.parse_value(max_string_bytes)?;
        Ok(ComparisonPredicate {
            column,
            operator,
            value,
        })
    }

    fn parse_predicate_groups(
        &mut self,
        max_predicates: usize,
        max_predicate_groups: usize,
        max_string_bytes: usize,
    ) -> Result<Vec<Vec<ComparisonPredicate>>, ParseError> {
        let mut groups = Vec::new();
        let mut predicate_count = 0;

        loop {
            self.skip_whitespace();
            if groups.len() == max_predicate_groups {
                return Err(self.error(ParseErrorKind::TooManyPredicateGroups {
                    limit: max_predicate_groups,
                }));
            }

            let parenthesized = self.peek() == Some(b'(');
            if parenthesized {
                self.position += 1;
            }

            let mut group = Vec::new();
            loop {
                self.skip_whitespace();
                if predicate_count == max_predicates {
                    return Err(self.error(ParseErrorKind::TooManyPredicates {
                        limit: max_predicates,
                    }));
                }
                group.push(self.parse_comparison(max_string_bytes)?);
                predicate_count += 1;

                self.skip_whitespace();
                if !self.peek_token_is("AND") {
                    break;
                }
                self.parse_keyword("AND")?;
            }

            if parenthesized {
                self.expect_byte(b')', "')'")?;
            }
            groups.push(group);

            self.skip_whitespace();
            if !self.peek_token_is("OR") {
                break;
            }
            self.parse_keyword("OR")?;
        }

        Ok(groups)
    }

    fn parse_comparison_column(&mut self) -> Result<String, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        while let Some(byte) = self.peek() {
            if is_whitespace(byte)
                || matches!(byte, b'(' | b')' | b',' | b';' | b'=' | b'!' | b'<' | b'>')
            {
                break;
            }
            self.position += 1;
        }

        let identifier = &self.input[start..self.position];
        if identifier.is_empty() {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedIdentifier {
                    context: IdentifierContext::Column,
                },
            });
        }
        if let Some(offset) = invalid_identifier_offset(identifier) {
            return Err(ParseError {
                position: start + offset,
                kind: ParseErrorKind::InvalidIdentifier {
                    context: IdentifierContext::Column,
                    identifier: identifier.to_owned(),
                },
            });
        }

        Ok(identifier.to_owned())
    }

    fn parse_comparison_operator(&mut self) -> Result<ComparisonOperator, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b'=' | b'!' | b'<' | b'>'))
        {
            self.position += 1;
        }

        let operator = &self.input[start..self.position];
        match operator {
            "=" => Ok(ComparisonOperator::Equal),
            "!=" | "<>" => Ok(ComparisonOperator::NotEqual),
            "<" => Ok(ComparisonOperator::LessThan),
            "<=" => Ok(ComparisonOperator::LessThanOrEqual),
            ">" => Ok(ComparisonOperator::GreaterThan),
            ">=" => Ok(ComparisonOperator::GreaterThanOrEqual),
            "" => Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedComparisonOperator,
            }),
            _ => Err(ParseError {
                position: start,
                kind: ParseErrorKind::InvalidComparisonOperator {
                    operator: operator.to_owned(),
                },
            }),
        }
    }

    fn parse_limit(&mut self) -> Result<usize, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let literal = self.take_token();
        if literal.is_empty() {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedLimit,
            });
        }
        if !literal.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::InvalidLimit {
                    literal: literal.to_owned(),
                },
            });
        }

        literal.parse().map_err(|_| ParseError {
            position: start,
            kind: ParseErrorKind::LimitOutOfRange {
                literal: literal.to_owned(),
            },
        })
    }

    fn parse_row(&mut self, limits: InsertParseLimits) -> Result<Vec<Value>, ParseError> {
        let mut values = Vec::new();

        loop {
            self.skip_whitespace();
            if self.peek() == Some(b')') {
                return Err(self.error(if values.is_empty() {
                    ParseErrorKind::EmptyRow
                } else {
                    ParseErrorKind::ExpectedValue
                }));
            }
            if values.len() == limits.max_values_per_row {
                return Err(self.error(ParseErrorKind::TooManyValues {
                    limit: limits.max_values_per_row,
                }));
            }

            values.push(self.parse_value(limits.max_string_bytes)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b')') => {
                    self.position += 1;
                    return Ok(values);
                }
                _ => {
                    return Err(self.error(ParseErrorKind::ExpectedToken {
                        expected: "',' or ')'",
                    }));
                }
            }
        }
    }

    fn parse_value(&mut self, max_string_bytes: usize) -> Result<Value, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        if self.peek() == Some(b'\'') {
            return self.parse_string(max_string_bytes).map(Value::String);
        }

        let literal = self.take_token();
        if literal.is_empty() {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedValue,
            });
        }
        if literal.eq_ignore_ascii_case("true") {
            return Ok(Value::Bool(true));
        }
        if literal.eq_ignore_ascii_case("false") {
            return Ok(Value::Bool(false));
        }

        match numeric_literal_kind(literal) {
            Some(NumericLiteralKind::Integer) => {
                literal
                    .parse::<i64>()
                    .map(Value::Int64)
                    .map_err(|_| ParseError {
                        position: start,
                        kind: ParseErrorKind::IntegerLiteralOutOfRange {
                            literal: literal.to_owned(),
                        },
                    })
            }
            Some(NumericLiteralKind::Float) => {
                let value = literal.parse::<f64>().map_err(|_| ParseError {
                    position: start,
                    kind: ParseErrorKind::InvalidLiteral {
                        literal: literal.to_owned(),
                    },
                })?;
                if value.is_finite() {
                    Ok(Value::Float64(value))
                } else {
                    Err(ParseError {
                        position: start,
                        kind: ParseErrorKind::FloatLiteralOutOfRange {
                            literal: literal.to_owned(),
                        },
                    })
                }
            }
            None => Err(ParseError {
                position: start,
                kind: ParseErrorKind::InvalidLiteral {
                    literal: literal.to_owned(),
                },
            }),
        }
    }

    fn parse_string(&mut self, max_bytes: usize) -> Result<String, ParseError> {
        self.position += 1;
        let mut value = String::new();

        loop {
            let segment_start = self.position;
            while self.peek().is_some_and(|byte| byte != b'\'') {
                self.position += 1;
            }

            let segment = &self.input[segment_start..self.position];
            let remaining = max_bytes.saturating_sub(value.len());
            if segment.len() > remaining {
                return Err(ParseError {
                    position: segment_start + remaining,
                    kind: ParseErrorKind::StringTooLong { limit: max_bytes },
                });
            }
            value.push_str(segment);

            if self.peek().is_none() {
                return Err(self.error(ParseErrorKind::UnterminatedString));
            }
            if self.input.as_bytes().get(self.position + 1) == Some(&b'\'') {
                if value.len() == max_bytes {
                    return Err(self.error(ParseErrorKind::StringTooLong { limit: max_bytes }));
                }
                value.push('\'');
                self.position += 2;
            } else {
                self.position += 1;
                return Ok(value);
            }
        }
    }

    fn finish_statement(&mut self) -> Result<(), ParseError> {
        self.skip_whitespace();
        if self.peek() == Some(b';') {
            self.position += 1;
            self.skip_whitespace();
        }
        if self.peek().is_some() {
            return Err(self.error(ParseErrorKind::TrailingSyntax));
        }
        Ok(())
    }

    fn parse_keyword(&mut self, expected: &'static str) -> Result<(), ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let found = self.take_token();
        if found.eq_ignore_ascii_case(expected) {
            Ok(())
        } else {
            Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedKeyword {
                    expected,
                    found: (!found.is_empty()).then(|| found.to_owned()),
                },
            })
        }
    }

    fn parse_identifier(
        &mut self,
        context: IdentifierContext,
    ) -> Result<(String, usize), ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let identifier = self.take_token();
        if identifier.is_empty() {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedIdentifier { context },
            });
        }

        if let Some(offset) = invalid_identifier_offset(identifier) {
            return Err(ParseError {
                position: start + offset,
                kind: ParseErrorKind::InvalidIdentifier {
                    context,
                    identifier: identifier.to_owned(),
                },
            });
        }

        Ok((identifier.to_owned(), start))
    }

    fn parse_data_type(&mut self) -> Result<DataType, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let type_name = self.take_token();
        if type_name.is_empty() {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedType,
            });
        }

        if type_name.eq_ignore_ascii_case("Int64") {
            Ok(DataType::Int64)
        } else if type_name.eq_ignore_ascii_case("Float64") {
            Ok(DataType::Float64)
        } else if type_name.eq_ignore_ascii_case("Bool") {
            Ok(DataType::Bool)
        } else if type_name.eq_ignore_ascii_case("String") {
            Ok(DataType::String)
        } else {
            Err(ParseError {
                position: start,
                kind: ParseErrorKind::UnknownType {
                    type_name: type_name.to_owned(),
                },
            })
        }
    }

    fn expect_byte(&mut self, byte: u8, expected: &'static str) -> Result<(), ParseError> {
        self.skip_whitespace();
        if self.peek() == Some(byte) {
            self.position += 1;
            Ok(())
        } else {
            Err(self.error(ParseErrorKind::ExpectedToken { expected }))
        }
    }

    fn take_token(&mut self) -> &'a str {
        let start = self.position;
        while let Some(byte) = self.peek() {
            if is_whitespace(byte) || matches!(byte, b'(' | b')' | b',' | b';' | b'*') {
                break;
            }
            self.position += 1;
        }
        &self.input[start..self.position]
    }

    fn peek_token_is(&self, expected: &str) -> bool {
        let bytes = self.input.as_bytes();
        let mut position = self.position;
        while bytes.get(position).copied().is_some_and(is_whitespace) {
            position += 1;
        }
        let start = position;
        while let Some(byte) = bytes.get(position) {
            if is_whitespace(*byte) || matches!(byte, b'(' | b')' | b',' | b';' | b'*') {
                break;
            }
            position += 1;
        }
        self.input[start..position].eq_ignore_ascii_case(expected)
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(is_whitespace) {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.position).copied()
    }

    fn error(&self, kind: ParseErrorKind) -> ParseError {
        ParseError {
            position: self.position,
            kind,
        }
    }
}

fn invalid_identifier_offset(identifier: &str) -> Option<usize> {
    let bytes = identifier.as_bytes();
    if !bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
    {
        return Some(0);
    }

    bytes
        .iter()
        .position(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AggregateKind {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

fn aggregate_kind(function: &str) -> Option<AggregateKind> {
    if function.eq_ignore_ascii_case("COUNT") {
        Some(AggregateKind::Count)
    } else if function.eq_ignore_ascii_case("SUM") {
        Some(AggregateKind::Sum)
    } else if function.eq_ignore_ascii_case("AVG") {
        Some(AggregateKind::Avg)
    } else if function.eq_ignore_ascii_case("MIN") {
        Some(AggregateKind::Min)
    } else if function.eq_ignore_ascii_case("MAX") {
        Some(AggregateKind::Max)
    } else {
        None
    }
}

#[derive(Clone, Copy)]
enum NumericLiteralKind {
    Integer,
    Float,
}

fn numeric_literal_kind(literal: &str) -> Option<NumericLiteralKind> {
    let bytes = literal.as_bytes();
    let mut position = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let integer_start = position;
    while bytes.get(position).is_some_and(u8::is_ascii_digit) {
        position += 1;
    }
    let integer_digits = position - integer_start;

    let mut kind = NumericLiteralKind::Integer;
    let mut fractional_digits = 0;
    if bytes.get(position) == Some(&b'.') {
        kind = NumericLiteralKind::Float;
        position += 1;
        let fractional_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        fractional_digits = position - fractional_start;
    }
    if integer_digits == 0 && fractional_digits == 0 {
        return None;
    }

    if matches!(bytes.get(position), Some(b'e' | b'E')) {
        kind = NumericLiteralKind::Float;
        position += 1;
        if matches!(bytes.get(position), Some(b'+' | b'-')) {
            position += 1;
        }
        let exponent_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        if position == exponent_start {
            return None;
        }
    }

    (position == bytes.len()).then_some(kind)
}

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}
