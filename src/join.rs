//! Bounded equi-joins over nullable typed columns.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

/// Resource bounds applied to an equi-join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinLimits {
    /// Maximum number of rows permitted in each input column.
    pub max_input_rows: usize,
    /// Maximum number of row-index pairs permitted in the output.
    pub max_output_pairs: usize,
}

impl JoinLimits {
    /// Creates explicit per-input-row and output-pair bounds.
    pub const fn new(max_input_rows: usize, max_output_pairs: usize) -> Self {
        Self {
            max_input_rows,
            max_output_pairs,
        }
    }
}

/// A matched pair of source row indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinRowPair {
    left_row: usize,
    right_row: usize,
}

impl JoinRowPair {
    fn new(left_row: usize, right_row: usize) -> Self {
        Self {
            left_row,
            right_row,
        }
    }

    /// Returns the source row index from the left input.
    pub const fn left_row(self) -> usize {
        self.left_row
    }

    /// Returns the source row index from the right input.
    pub const fn right_row(self) -> usize {
        self.right_row
    }

    /// Returns the match as a `(left_row, right_row)` pair.
    pub const fn into_pair(self) -> (usize, usize) {
        (self.left_row, self.right_row)
    }
}

/// A left row paired with an optional matching right row.
///
/// An absent right row represents the single output row retained by a left
/// outer join when the left key is `NULL` or has no match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeftOuterJoinRowPair {
    left_row: usize,
    right_row: Option<usize>,
}

impl LeftOuterJoinRowPair {
    fn new(left_row: usize, right_row: Option<usize>) -> Self {
        Self {
            left_row,
            right_row,
        }
    }

    /// Returns the source row index from the left input.
    pub const fn left_row(self) -> usize {
        self.left_row
    }

    /// Returns the matching right source row, or `None` for an unmatched row.
    pub const fn right_row(self) -> Option<usize> {
        self.right_row
    }

    /// Returns the output as a `(left_row, optional_right_row)` pair.
    pub const fn into_pair(self) -> (usize, Option<usize>) {
        (self.left_row, self.right_row)
    }
}

/// A resource limit rejected by an equi-join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinError {
    /// The left column contains more rows than the configured input bound.
    LeftInputLimitExceeded { rows: usize, max_rows: usize },
    /// The right column contains more rows than the configured input bound.
    RightInputLimitExceeded { rows: usize, max_rows: usize },
    /// The join produces more row-index pairs than the configured output bound.
    OutputLimitExceeded { pairs: usize, max_pairs: usize },
}

impl fmt::Display for JoinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LeftInputLimitExceeded { rows, max_rows } => write!(
                formatter,
                "join left input has {rows} rows, exceeding the limit of {max_rows}"
            ),
            Self::RightInputLimitExceeded { rows, max_rows } => write!(
                formatter,
                "join right input has {rows} rows, exceeding the limit of {max_rows}"
            ),
            Self::OutputLimitExceeded { pairs, max_pairs } => write!(
                formatter,
                "join has at least {pairs} output pairs, exceeding the limit of {max_pairs}"
            ),
        }
    }
}

impl Error for JoinError {}

/// Inner-joins two nullable `i64` columns by equality.
///
/// `None` represents SQL `NULL` and never matches, including another `NULL`.
/// Every duplicate match is returned as a cross-product. Results are ordered
/// first by ascending left source row and then by ascending right source row.
///
/// The row bound applies independently to each input and is validated before
/// either input is inspected. The complete output size is checked before the
/// result is allocated, so an output-limit failure never returns a partial
/// result.
///
/// # Examples
///
/// ```
/// use rusthouse::{JoinLimits, inner_equi_join_nullable_i64};
///
/// let left = [Some(4), None, Some(4)];
/// let right = [Some(4), Some(9), Some(4), None];
/// let matches = inner_equi_join_nullable_i64(
///     &left,
///     &right,
///     JoinLimits::new(right.len(), 4),
/// )?;
/// let pairs: Vec<_> = matches.into_iter().map(|pair| pair.into_pair()).collect();
///
/// assert_eq!(pairs, vec![(0, 0), (0, 2), (2, 0), (2, 2)]);
/// # Ok::<(), rusthouse::JoinError>(())
/// ```
pub fn inner_equi_join_nullable_i64(
    left: &[Option<i64>],
    right: &[Option<i64>],
    limits: JoinLimits,
) -> Result<Vec<JoinRowPair>, JoinError> {
    validate_input_limits(left.len(), right.len(), limits.max_input_rows)?;

    let right_rows_by_value = index_right_rows(right);

    let output_pairs = count_output_pairs(left, &right_rows_by_value, limits.max_output_pairs)?;
    let mut matches = Vec::with_capacity(output_pairs);

    for (left_row, value) in left.iter().copied().enumerate() {
        let Some(right_rows) = value.and_then(|value| right_rows_by_value.get(&value)) else {
            continue;
        };

        matches.extend(
            right_rows
                .iter()
                .copied()
                .map(|right_row| JoinRowPair::new(left_row, right_row)),
        );
    }

    Ok(matches)
}

/// Left-outer-joins two nullable `i64` columns by equality.
///
/// `None` represents SQL `NULL` and never matches, including another `NULL`.
/// Each left row with no match is returned exactly once with no right row.
/// Duplicate keys produce their full cross-product. Results are ordered first
/// by ascending left source row and then by ascending right source row.
///
/// The row bound applies independently to each input and is validated before
/// either input is inspected. The complete matched-or-unmatched output size is
/// checked before the result is allocated, so an output-limit failure never
/// returns a partial result.
///
/// # Examples
///
/// ```
/// use rusthouse::{JoinLimits, left_outer_equi_join_nullable_i64};
///
/// let left = [Some(4), None, Some(8)];
/// let right = [Some(4), Some(4), None];
/// let rows = left_outer_equi_join_nullable_i64(
///     &left,
///     &right,
///     JoinLimits::new(3, 4),
/// )?;
/// let pairs: Vec<_> = rows.into_iter().map(|row| row.into_pair()).collect();
///
/// assert_eq!(
///     pairs,
///     vec![(0, Some(0)), (0, Some(1)), (1, None), (2, None)],
/// );
/// # Ok::<(), rusthouse::JoinError>(())
/// ```
pub fn left_outer_equi_join_nullable_i64(
    left: &[Option<i64>],
    right: &[Option<i64>],
    limits: JoinLimits,
) -> Result<Vec<LeftOuterJoinRowPair>, JoinError> {
    validate_input_limits(left.len(), right.len(), limits.max_input_rows)?;

    let right_rows_by_value = index_right_rows(right);
    let output_pairs =
        count_left_outer_output_pairs(left, &right_rows_by_value, limits.max_output_pairs)?;
    let mut rows = Vec::with_capacity(output_pairs);

    for (left_row, value) in left.iter().copied().enumerate() {
        let Some(right_rows) = value.and_then(|value| right_rows_by_value.get(&value)) else {
            rows.push(LeftOuterJoinRowPair::new(left_row, None));
            continue;
        };

        rows.extend(
            right_rows
                .iter()
                .copied()
                .map(|right_row| LeftOuterJoinRowPair::new(left_row, Some(right_row))),
        );
    }

    Ok(rows)
}

fn index_right_rows(right: &[Option<i64>]) -> HashMap<i64, Vec<usize>> {
    let mut right_rows_by_value = HashMap::<i64, Vec<usize>>::new();
    for (right_row, value) in right.iter().copied().enumerate() {
        if let Some(value) = value {
            right_rows_by_value
                .entry(value)
                .or_default()
                .push(right_row);
        }
    }
    right_rows_by_value
}

fn validate_input_limits(
    left_rows: usize,
    right_rows: usize,
    max_input_rows: usize,
) -> Result<(), JoinError> {
    if left_rows > max_input_rows {
        return Err(JoinError::LeftInputLimitExceeded {
            rows: left_rows,
            max_rows: max_input_rows,
        });
    }

    if right_rows > max_input_rows {
        return Err(JoinError::RightInputLimitExceeded {
            rows: right_rows,
            max_rows: max_input_rows,
        });
    }

    Ok(())
}

fn count_output_pairs(
    left: &[Option<i64>],
    right_rows_by_value: &HashMap<i64, Vec<usize>>,
    max_output_pairs: usize,
) -> Result<usize, JoinError> {
    let mut output_pairs = 0_usize;

    for value in left.iter().flatten() {
        let matching_rows = right_rows_by_value.get(value).map_or(0, Vec::len);
        if matching_rows > max_output_pairs - output_pairs {
            return Err(JoinError::OutputLimitExceeded {
                pairs: max_output_pairs.saturating_add(1),
                max_pairs: max_output_pairs,
            });
        }
        output_pairs += matching_rows;
    }

    Ok(output_pairs)
}

fn count_left_outer_output_pairs(
    left: &[Option<i64>],
    right_rows_by_value: &HashMap<i64, Vec<usize>>,
    max_output_pairs: usize,
) -> Result<usize, JoinError> {
    let mut output_pairs = 0_usize;

    for value in left {
        let matching_rows = value
            .and_then(|value| right_rows_by_value.get(&value))
            .map_or(1, Vec::len);
        if matching_rows > max_output_pairs - output_pairs {
            return Err(JoinError::OutputLimitExceeded {
                pairs: max_output_pairs.saturating_add(1),
                max_pairs: max_output_pairs,
            });
        }
        output_pairs += matching_rows;
    }

    Ok(output_pairs)
}
