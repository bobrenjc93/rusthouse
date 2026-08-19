use crate::batch::sql::ComparisonOperator;
use crate::batch::storage::Int64Filter;

#[derive(Debug, Clone, Copy)]
pub(super) struct Int64MetadataComparison {
    column: usize,
    operator: ComparisonOperator,
    value: i64,
    literal_on_left: bool,
}

impl Int64MetadataComparison {
    pub(super) const fn column_on_left(
        column: usize,
        operator: ComparisonOperator,
        value: i64,
    ) -> Self {
        Self {
            column,
            operator,
            value,
            literal_on_left: false,
        }
    }

    pub(super) const fn literal_on_left(
        value: i64,
        operator: ComparisonOperator,
        column: usize,
    ) -> Self {
        Self {
            column,
            operator,
            value,
            literal_on_left: true,
        }
    }
}

/// Normalizes one direct comparison or one two-comparison conjunction for
/// conservative `Int64` metadata pruning.
pub(super) fn normalize_int64_metadata_filter(
    first: Int64MetadataComparison,
    second: Option<Int64MetadataComparison>,
) -> Option<(usize, Int64Filter)> {
    let first = normalize_comparison(first)?;
    let Some(second) = second else {
        return Some(first);
    };
    combine_range(first, normalize_comparison(second)?)
}

fn normalize_comparison(comparison: Int64MetadataComparison) -> Option<(usize, Int64Filter)> {
    let operator = if comparison.literal_on_left {
        reverse_comparison(comparison.operator)
    } else {
        comparison.operator
    };
    let filter = match operator {
        ComparisonOperator::Equal => Int64Filter::Equal(comparison.value),
        ComparisonOperator::Less => Int64Filter::Less(comparison.value),
        ComparisonOperator::LessOrEqual => Int64Filter::LessOrEqual(comparison.value),
        ComparisonOperator::Greater => Int64Filter::Greater(comparison.value),
        ComparisonOperator::GreaterOrEqual => Int64Filter::GreaterOrEqual(comparison.value),
        ComparisonOperator::NotEqual => return None,
    };
    Some((comparison.column, filter))
}

fn combine_range(
    first: (usize, Int64Filter),
    second: (usize, Int64Filter),
) -> Option<(usize, Int64Filter)> {
    let (lower_column, lower, lower_strict, upper_column, upper, upper_strict) =
        match (first, second) {
            (
                (lower_column, Int64Filter::GreaterOrEqual(lower)),
                (upper_column, Int64Filter::LessOrEqual(upper)),
            ) => (lower_column, lower, false, upper_column, upper, false),
            (
                (upper_column, Int64Filter::LessOrEqual(upper)),
                (lower_column, Int64Filter::GreaterOrEqual(lower)),
            ) => (lower_column, lower, false, upper_column, upper, false),
            (
                (lower_column, Int64Filter::Greater(lower)),
                (upper_column, Int64Filter::LessOrEqual(upper)),
            ) => (lower_column, lower, true, upper_column, upper, false),
            (
                (upper_column, Int64Filter::LessOrEqual(upper)),
                (lower_column, Int64Filter::Greater(lower)),
            ) => (lower_column, lower, true, upper_column, upper, false),
            (
                (lower_column, Int64Filter::GreaterOrEqual(lower)),
                (upper_column, Int64Filter::Less(upper)),
            ) => (lower_column, lower, false, upper_column, upper, true),
            (
                (upper_column, Int64Filter::Less(upper)),
                (lower_column, Int64Filter::GreaterOrEqual(lower)),
            ) => (lower_column, lower, false, upper_column, upper, true),
            (
                (lower_column, Int64Filter::Greater(lower)),
                (upper_column, Int64Filter::Less(upper)),
            ) => (lower_column, lower, true, upper_column, upper, true),
            (
                (upper_column, Int64Filter::Less(upper)),
                (lower_column, Int64Filter::Greater(lower)),
            ) => (lower_column, lower, true, upper_column, upper, true),
            _ => return None,
        };
    if lower_column != upper_column {
        return None;
    }
    Some((
        lower_column,
        normalize_strict_bounds(lower, lower_strict, upper, upper_strict),
    ))
}

fn normalize_strict_bounds(
    lower: i64,
    lower_strict: bool,
    upper: i64,
    upper_strict: bool,
) -> Int64Filter {
    let lower = if lower_strict {
        let Some(lower) = lower.checked_add(1) else {
            return empty_range();
        };
        lower
    } else {
        lower
    };
    let upper = if upper_strict {
        let Some(upper) = upper.checked_sub(1) else {
            return empty_range();
        };
        upper
    } else {
        upper
    };
    Int64Filter::InclusiveRange { lower, upper }
}

const fn empty_range() -> Int64Filter {
    // Both metadata consumers treat lower > upper as an empty range.
    Int64Filter::InclusiveRange {
        lower: i64::MAX,
        upper: i64::MIN,
    }
}

const fn reverse_comparison(operator: ComparisonOperator) -> ComparisonOperator {
    match operator {
        ComparisonOperator::Equal => ComparisonOperator::Equal,
        ComparisonOperator::NotEqual => ComparisonOperator::NotEqual,
        ComparisonOperator::Less => ComparisonOperator::Greater,
        ComparisonOperator::LessOrEqual => ComparisonOperator::GreaterOrEqual,
        ComparisonOperator::Greater => ComparisonOperator::Less,
        ComparisonOperator::GreaterOrEqual => ComparisonOperator::LessOrEqual,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_comparisons_normalize_operand_order_table() {
        for (comparison, expected) in [
            (
                Int64MetadataComparison::column_on_left(2, ComparisonOperator::Equal, 7),
                Some((2, Int64Filter::Equal(7))),
            ),
            (
                Int64MetadataComparison::literal_on_left(7, ComparisonOperator::Equal, 2),
                Some((2, Int64Filter::Equal(7))),
            ),
            (
                Int64MetadataComparison::column_on_left(2, ComparisonOperator::Less, 7),
                Some((2, Int64Filter::Less(7))),
            ),
            (
                Int64MetadataComparison::literal_on_left(7, ComparisonOperator::Less, 2),
                Some((2, Int64Filter::Greater(7))),
            ),
            (
                Int64MetadataComparison::column_on_left(2, ComparisonOperator::LessOrEqual, 7),
                Some((2, Int64Filter::LessOrEqual(7))),
            ),
            (
                Int64MetadataComparison::literal_on_left(7, ComparisonOperator::LessOrEqual, 2),
                Some((2, Int64Filter::GreaterOrEqual(7))),
            ),
            (
                Int64MetadataComparison::column_on_left(2, ComparisonOperator::Greater, 7),
                Some((2, Int64Filter::Greater(7))),
            ),
            (
                Int64MetadataComparison::literal_on_left(7, ComparisonOperator::Greater, 2),
                Some((2, Int64Filter::Less(7))),
            ),
            (
                Int64MetadataComparison::column_on_left(2, ComparisonOperator::GreaterOrEqual, 7),
                Some((2, Int64Filter::GreaterOrEqual(7))),
            ),
            (
                Int64MetadataComparison::literal_on_left(7, ComparisonOperator::GreaterOrEqual, 2),
                Some((2, Int64Filter::LessOrEqual(7))),
            ),
            (
                Int64MetadataComparison::column_on_left(2, ComparisonOperator::NotEqual, 7),
                None,
            ),
            (
                Int64MetadataComparison::literal_on_left(7, ComparisonOperator::NotEqual, 2),
                None,
            ),
        ] {
            assert_eq!(
                normalize_int64_metadata_filter(comparison, None),
                expected,
                "{comparison:?}",
            );
        }
    }

    #[test]
    fn two_sided_ranges_combine_conjunction_and_operand_order_table() {
        for (first, second, expected) in [
            (
                Int64MetadataComparison::column_on_left(3, ComparisonOperator::GreaterOrEqual, 5),
                Int64MetadataComparison::column_on_left(3, ComparisonOperator::LessOrEqual, 15),
                Int64Filter::InclusiveRange {
                    lower: 5,
                    upper: 15,
                },
            ),
            (
                Int64MetadataComparison::column_on_left(3, ComparisonOperator::LessOrEqual, 15),
                Int64MetadataComparison::column_on_left(3, ComparisonOperator::GreaterOrEqual, 5),
                Int64Filter::InclusiveRange {
                    lower: 5,
                    upper: 15,
                },
            ),
            (
                Int64MetadataComparison::literal_on_left(5, ComparisonOperator::LessOrEqual, 3),
                Int64MetadataComparison::literal_on_left(15, ComparisonOperator::GreaterOrEqual, 3),
                Int64Filter::InclusiveRange {
                    lower: 5,
                    upper: 15,
                },
            ),
            (
                Int64MetadataComparison::literal_on_left(15, ComparisonOperator::GreaterOrEqual, 3),
                Int64MetadataComparison::literal_on_left(5, ComparisonOperator::LessOrEqual, 3),
                Int64Filter::InclusiveRange {
                    lower: 5,
                    upper: 15,
                },
            ),
        ] {
            assert_eq!(
                normalize_int64_metadata_filter(first, Some(second)),
                Some((3, expected)),
                "{first:?} AND {second:?}",
            );
        }
    }

    #[test]
    fn strict_bounds_normalize_and_preserve_empty_extremes_table() {
        for (lower, upper, expected) in [
            (
                Int64MetadataComparison::column_on_left(1, ComparisonOperator::Greater, 5),
                Int64MetadataComparison::column_on_left(1, ComparisonOperator::LessOrEqual, 15),
                Int64Filter::InclusiveRange {
                    lower: 6,
                    upper: 15,
                },
            ),
            (
                Int64MetadataComparison::column_on_left(1, ComparisonOperator::GreaterOrEqual, 5),
                Int64MetadataComparison::column_on_left(1, ComparisonOperator::Less, 15),
                Int64Filter::InclusiveRange {
                    lower: 5,
                    upper: 14,
                },
            ),
            (
                Int64MetadataComparison::column_on_left(1, ComparisonOperator::Greater, 5),
                Int64MetadataComparison::column_on_left(1, ComparisonOperator::Less, 15),
                Int64Filter::InclusiveRange {
                    lower: 6,
                    upper: 14,
                },
            ),
            (
                Int64MetadataComparison::column_on_left(1, ComparisonOperator::Greater, i64::MAX),
                Int64MetadataComparison::column_on_left(
                    1,
                    ComparisonOperator::LessOrEqual,
                    i64::MAX,
                ),
                empty_range(),
            ),
            (
                Int64MetadataComparison::column_on_left(
                    1,
                    ComparisonOperator::GreaterOrEqual,
                    i64::MIN,
                ),
                Int64MetadataComparison::column_on_left(1, ComparisonOperator::Less, i64::MIN),
                empty_range(),
            ),
        ] {
            assert_eq!(
                normalize_int64_metadata_filter(lower, Some(upper)),
                Some((1, expected)),
                "{lower:?} AND {upper:?}",
            );
        }
    }

    #[test]
    fn unsupported_conjunction_shapes_fall_back_table() {
        for (first, second) in [
            (
                Int64MetadataComparison::column_on_left(0, ComparisonOperator::Equal, 5),
                Int64MetadataComparison::column_on_left(0, ComparisonOperator::LessOrEqual, 15),
            ),
            (
                Int64MetadataComparison::column_on_left(0, ComparisonOperator::Greater, 5),
                Int64MetadataComparison::column_on_left(1, ComparisonOperator::LessOrEqual, 15),
            ),
            (
                Int64MetadataComparison::column_on_left(0, ComparisonOperator::Greater, 5),
                Int64MetadataComparison::column_on_left(0, ComparisonOperator::GreaterOrEqual, 15),
            ),
            (
                Int64MetadataComparison::column_on_left(0, ComparisonOperator::Less, 5),
                Int64MetadataComparison::column_on_left(0, ComparisonOperator::LessOrEqual, 15),
            ),
            (
                Int64MetadataComparison::column_on_left(0, ComparisonOperator::NotEqual, 5),
                Int64MetadataComparison::column_on_left(0, ComparisonOperator::LessOrEqual, 15),
            ),
        ] {
            assert_eq!(
                normalize_int64_metadata_filter(first, Some(second)),
                None,
                "{first:?} AND {second:?}",
            );
        }
    }
}
