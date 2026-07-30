use std::cmp::Ordering;

use crate::error::{Error, Result};
use crate::sql::{ComparisonOperator, Operand, Predicate};
use crate::storage::{Column, Table};
use crate::value::{DataType, Value, int_float_cmp};

const BLOCK_ROWS: usize = 1_024;
const BLOCK_WORDS: usize = BLOCK_ROWS / u64::BITS as usize;

/// Matching rows stored one bit per source row.
#[derive(Debug)]
pub(crate) struct RowSelection {
    words: Vec<u64>,
    row_count: usize,
    match_count: usize,
}

impl RowSelection {
    pub(crate) fn all(row_count: usize) -> Self {
        let mut words = vec![u64::MAX; row_count.div_ceil(u64::BITS as usize)];
        if let (Some(last), remainder) = (words.last_mut(), row_count % u64::BITS as usize)
            && remainder != 0
        {
            *last = (1_u64 << remainder) - 1;
        }
        Self {
            words,
            row_count,
            match_count: row_count,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.match_count
    }

    pub(crate) fn iter(&self) -> RowSelectionIter<'_> {
        RowSelectionIter {
            words: &self.words,
            row_count: self.row_count,
            word: 0,
            remaining: self.words.first().copied().unwrap_or(0),
            remaining_count: self.match_count,
        }
    }
}

pub(crate) struct RowSelectionIter<'a> {
    words: &'a [u64],
    row_count: usize,
    word: usize,
    remaining: u64,
    remaining_count: usize,
}

impl Iterator for RowSelectionIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.remaining != 0 {
                let bit = self.remaining.trailing_zeros() as usize;
                self.remaining &= self.remaining - 1;
                let row = self.word * u64::BITS as usize + bit;
                self.remaining_count -= 1;
                return (row < self.row_count).then_some(row);
            }
            self.word += 1;
            self.remaining = *self.words.get(self.word)?;
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining_count, Some(self.remaining_count))
    }
}

impl ExactSizeIterator for RowSelectionIter<'_> {}

pub(crate) fn select_rows(table: &Table, predicate: &Predicate) -> Result<RowSelection> {
    let kernel = PredicateKernel::compile(table, predicate)?;
    let row_count = table.row_count();
    let mut words = vec![0; row_count.div_ceil(u64::BITS as usize)];

    for block_start in (0..row_count).step_by(BLOCK_ROWS) {
        let block_len = (row_count - block_start).min(BLOCK_ROWS);
        let active = BlockBitmap::active(block_len);
        let result = kernel.evaluate(table, block_start, block_len, &active);
        let block_words = block_len.div_ceil(u64::BITS as usize);
        let destination = block_start / u64::BITS as usize;
        words[destination..destination + block_words]
            .copy_from_slice(&result.truth.words[..block_words]);
    }

    let match_count = words.iter().map(|word| word.count_ones() as usize).sum();
    Ok(RowSelection {
        words,
        row_count,
        match_count,
    })
}

#[derive(Debug, Clone)]
struct BlockBitmap {
    words: [u64; BLOCK_WORDS],
}

impl BlockBitmap {
    fn empty() -> Self {
        Self {
            words: [0; BLOCK_WORDS],
        }
    }

    fn active(row_count: usize) -> Self {
        debug_assert!(row_count <= BLOCK_ROWS);
        let mut bitmap = Self::empty();
        let full_words = row_count / u64::BITS as usize;
        bitmap.words[..full_words].fill(u64::MAX);
        let remainder = row_count % u64::BITS as usize;
        if remainder != 0 {
            bitmap.words[full_words] = (1_u64 << remainder) - 1;
        }
        bitmap
    }
}

#[derive(Debug)]
struct TruthBitmap {
    truth: BlockBitmap,
    unknown: BlockBitmap,
}

impl TruthBitmap {
    fn constant(value: SqlTruth, active: &BlockBitmap) -> Self {
        match value {
            SqlTruth::True => Self {
                truth: active.clone(),
                unknown: BlockBitmap::empty(),
            },
            SqlTruth::False => Self {
                truth: BlockBitmap::empty(),
                unknown: BlockBitmap::empty(),
            },
            SqlTruth::Unknown => Self {
                truth: BlockBitmap::empty(),
                unknown: active.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqlTruth {
    True,
    False,
    Unknown,
}

#[derive(Debug)]
enum PredicateKernel {
    Comparison(ComparisonKernel),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl PredicateKernel {
    fn compile(table: &Table, predicate: &Predicate) -> Result<Self> {
        match predicate {
            Predicate::Comparison {
                left,
                operator,
                right,
            } => Ok(Self::Comparison(ComparisonKernel::compile(
                table, left, *operator, right,
            )?)),
            Predicate::And(left, right) => Ok(Self::And(
                Box::new(Self::compile(table, left)?),
                Box::new(Self::compile(table, right)?),
            )),
            Predicate::Or(left, right) => Ok(Self::Or(
                Box::new(Self::compile(table, left)?),
                Box::new(Self::compile(table, right)?),
            )),
        }
    }

    fn evaluate(
        &self,
        table: &Table,
        block_start: usize,
        block_len: usize,
        active: &BlockBitmap,
    ) -> TruthBitmap {
        match self {
            Self::Comparison(kernel) => kernel.evaluate(table, block_start, block_len, active),
            Self::And(left, right) => {
                let left = left.evaluate(table, block_start, block_len, active);
                let mut right_active = BlockBitmap::empty();
                for word in 0..BLOCK_WORDS {
                    right_active.words[word] = left.truth.words[word] | left.unknown.words[word];
                }
                let right = right.evaluate(table, block_start, block_len, &right_active);
                let mut result = TruthBitmap::constant(SqlTruth::False, active);
                for word in 0..BLOCK_WORDS {
                    result.truth.words[word] = left.truth.words[word] & right.truth.words[word];
                    result.unknown.words[word] = (left.truth.words[word]
                        & right.unknown.words[word])
                        | (left.unknown.words[word]
                            & (right.truth.words[word] | right.unknown.words[word]));
                }
                result
            }
            Self::Or(left, right) => {
                let left = left.evaluate(table, block_start, block_len, active);
                let mut right_active = BlockBitmap::empty();
                for word in 0..BLOCK_WORDS {
                    right_active.words[word] = active.words[word] & !left.truth.words[word];
                }
                let right = right.evaluate(table, block_start, block_len, &right_active);
                let mut result = TruthBitmap::constant(SqlTruth::False, active);
                for word in 0..BLOCK_WORDS {
                    result.truth.words[word] = left.truth.words[word] | right.truth.words[word];
                    result.unknown.words[word] = active.words[word]
                        & ((left.unknown.words[word] & !right.truth.words[word])
                            | (right.unknown.words[word] & !left.truth.words[word]));
                }
                result
            }
        }
    }
}

#[derive(Debug)]
enum ComparisonKernel {
    Int64 {
        left: Int64Input,
        operator: ComparisonOperator,
        right: Int64Input,
    },
    Float64 {
        left: Float64Input,
        operator: ComparisonOperator,
        right: Float64Input,
    },
    Bool {
        left: BoolInput,
        operator: ComparisonOperator,
        right: BoolInput,
    },
    String {
        left: StringInput,
        operator: ComparisonOperator,
        right: StringInput,
    },
    Int64Float64 {
        left: Int64Input,
        operator: ComparisonOperator,
        right: Float64Input,
    },
    Float64Int64 {
        left: Float64Input,
        operator: ComparisonOperator,
        right: Int64Input,
    },
}

impl ComparisonKernel {
    fn compile(
        table: &Table,
        left: &Operand,
        operator: ComparisonOperator,
        right: &Operand,
    ) -> Result<Self> {
        let left = ResolvedOperand::compile(table, left)?;
        let right = ResolvedOperand::compile(table, right)?;
        let left_type = left.data_type();
        let right_type = right.data_type();

        match (left, right) {
            (ResolvedOperand::Int64(left), ResolvedOperand::Int64(right)) => Ok(Self::Int64 {
                left,
                operator,
                right,
            }),
            (ResolvedOperand::Float64(left), ResolvedOperand::Float64(right)) => {
                Ok(Self::Float64 {
                    left,
                    operator,
                    right,
                })
            }
            (ResolvedOperand::Bool(left), ResolvedOperand::Bool(right)) => Ok(Self::Bool {
                left,
                operator,
                right,
            }),
            (ResolvedOperand::String(left), ResolvedOperand::String(right)) => Ok(Self::String {
                left,
                operator,
                right,
            }),
            (ResolvedOperand::Int64(left), ResolvedOperand::Float64(right)) => {
                Ok(Self::Int64Float64 {
                    left,
                    operator,
                    right,
                })
            }
            (ResolvedOperand::Float64(left), ResolvedOperand::Int64(right)) => {
                Ok(Self::Float64Int64 {
                    left,
                    operator,
                    right,
                })
            }
            _ => Err(Error::TypeMismatch {
                context: "WHERE comparison".to_owned(),
                expected: left_type.to_string(),
                actual: right_type.to_string(),
            }),
        }
    }

    fn evaluate(
        &self,
        table: &Table,
        block_start: usize,
        block_len: usize,
        active: &BlockBitmap,
    ) -> TruthBitmap {
        match self {
            Self::Int64 {
                left,
                operator,
                right,
            } => evaluate_int64(
                table,
                block_start,
                block_len,
                active,
                left,
                *operator,
                right,
            ),
            Self::Float64 {
                left,
                operator,
                right,
            } => evaluate_float64(
                table,
                block_start,
                block_len,
                active,
                left,
                *operator,
                right,
            ),
            Self::Bool {
                left,
                operator,
                right,
            } => evaluate_bool(
                table,
                block_start,
                block_len,
                active,
                left,
                *operator,
                right,
            ),
            Self::String {
                left,
                operator,
                right,
            } => evaluate_string(
                table,
                block_start,
                block_len,
                active,
                left,
                *operator,
                right,
            ),
            Self::Int64Float64 {
                left,
                operator,
                right,
            } => evaluate_int64_float64(
                table,
                block_start,
                block_len,
                active,
                left,
                *operator,
                right,
            ),
            Self::Float64Int64 {
                left,
                operator,
                right,
            } => evaluate_float64_int64(
                table,
                block_start,
                block_len,
                active,
                left,
                *operator,
                right,
            ),
        }
    }
}

#[derive(Debug)]
enum ResolvedOperand {
    Int64(Int64Input),
    Float64(Float64Input),
    Bool(BoolInput),
    String(StringInput),
}

impl ResolvedOperand {
    fn compile(table: &Table, operand: &Operand) -> Result<Self> {
        match operand {
            Operand::Column(name) => {
                let column = table.column_index(name)?;
                Ok(match table.schema()[column].data_type {
                    DataType::Int64 => Self::Int64(Int64Input::Column(column)),
                    DataType::Float64 => Self::Float64(Float64Input::Column(column)),
                    DataType::Bool => Self::Bool(BoolInput::Column(column)),
                    DataType::String => Self::String(StringInput::Column(column)),
                })
            }
            Operand::Literal(Value::Int64(value)) => Ok(Self::Int64(Int64Input::Literal(*value))),
            Operand::Literal(Value::Float64(value)) => {
                Ok(Self::Float64(Float64Input::Literal(*value)))
            }
            Operand::Literal(Value::Bool(value)) => Ok(Self::Bool(BoolInput::Literal(*value))),
            Operand::Literal(Value::String(value)) => {
                Ok(Self::String(StringInput::Literal(value.clone())))
            }
        }
    }

    fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }
}

#[derive(Debug)]
enum Int64Input {
    Column(usize),
    Literal(i64),
}

#[derive(Debug)]
enum Float64Input {
    Column(usize),
    Literal(f64),
}

#[derive(Debug)]
enum BoolInput {
    Column(usize),
    Literal(bool),
}

#[derive(Debug)]
enum StringInput {
    Column(usize),
    Literal(String),
}

fn evaluate_int64(
    table: &Table,
    start: usize,
    len: usize,
    active: &BlockBitmap,
    left: &Int64Input,
    operator: ComparisonOperator,
    right: &Int64Input,
) -> TruthBitmap {
    match (left, right) {
        (Int64Input::Column(left), Int64Input::Column(right)) => {
            let left = int64_slice(table, *left, start, len);
            let right = int64_slice(table, *right, start, len);
            evaluate_active(active, operator, |row| Some(left[row].cmp(&right[row])))
        }
        (Int64Input::Column(left), Int64Input::Literal(right)) => {
            let left = int64_slice(table, *left, start, len);
            evaluate_active(active, operator, |row| Some(left[row].cmp(right)))
        }
        (Int64Input::Literal(left), Int64Input::Column(right)) => {
            let right = int64_slice(table, *right, start, len);
            evaluate_active(active, operator, |row| Some(left.cmp(&right[row])))
        }
        (Int64Input::Literal(left), Int64Input::Literal(right)) => {
            TruthBitmap::constant(compare(Some(left.cmp(right)), operator), active)
        }
    }
}

fn evaluate_float64(
    table: &Table,
    start: usize,
    len: usize,
    active: &BlockBitmap,
    left: &Float64Input,
    operator: ComparisonOperator,
    right: &Float64Input,
) -> TruthBitmap {
    match (left, right) {
        (Float64Input::Column(left), Float64Input::Column(right)) => {
            let left = float64_slice(table, *left, start, len);
            let right = float64_slice(table, *right, start, len);
            evaluate_active(active, operator, |row| left[row].partial_cmp(&right[row]))
        }
        (Float64Input::Column(left), Float64Input::Literal(right)) => {
            let left = float64_slice(table, *left, start, len);
            evaluate_active(active, operator, |row| left[row].partial_cmp(right))
        }
        (Float64Input::Literal(left), Float64Input::Column(right)) => {
            let right = float64_slice(table, *right, start, len);
            evaluate_active(active, operator, |row| left.partial_cmp(&right[row]))
        }
        (Float64Input::Literal(left), Float64Input::Literal(right)) => {
            TruthBitmap::constant(compare(left.partial_cmp(right), operator), active)
        }
    }
}

fn evaluate_bool(
    table: &Table,
    start: usize,
    len: usize,
    active: &BlockBitmap,
    left: &BoolInput,
    operator: ComparisonOperator,
    right: &BoolInput,
) -> TruthBitmap {
    match (left, right) {
        (BoolInput::Column(left), BoolInput::Column(right)) => {
            let left = bool_slice(table, *left, start, len);
            let right = bool_slice(table, *right, start, len);
            evaluate_active(active, operator, |row| Some(left[row].cmp(&right[row])))
        }
        (BoolInput::Column(left), BoolInput::Literal(right)) => {
            let left = bool_slice(table, *left, start, len);
            evaluate_active(active, operator, |row| Some(left[row].cmp(right)))
        }
        (BoolInput::Literal(left), BoolInput::Column(right)) => {
            let right = bool_slice(table, *right, start, len);
            evaluate_active(active, operator, |row| Some(left.cmp(&right[row])))
        }
        (BoolInput::Literal(left), BoolInput::Literal(right)) => {
            TruthBitmap::constant(compare(Some(left.cmp(right)), operator), active)
        }
    }
}

fn evaluate_string(
    table: &Table,
    start: usize,
    len: usize,
    active: &BlockBitmap,
    left: &StringInput,
    operator: ComparisonOperator,
    right: &StringInput,
) -> TruthBitmap {
    match (left, right) {
        (StringInput::Column(left), StringInput::Column(right)) => {
            let left = string_slice(table, *left, start, len);
            let right = string_slice(table, *right, start, len);
            evaluate_active(active, operator, |row| Some(left[row].cmp(&right[row])))
        }
        (StringInput::Column(left), StringInput::Literal(right)) => {
            let left = string_slice(table, *left, start, len);
            evaluate_active(active, operator, |row| Some(left[row].cmp(right)))
        }
        (StringInput::Literal(left), StringInput::Column(right)) => {
            let right = string_slice(table, *right, start, len);
            evaluate_active(active, operator, |row| Some(left.cmp(&right[row])))
        }
        (StringInput::Literal(left), StringInput::Literal(right)) => {
            TruthBitmap::constant(compare(Some(left.cmp(right)), operator), active)
        }
    }
}

fn evaluate_int64_float64(
    table: &Table,
    start: usize,
    len: usize,
    active: &BlockBitmap,
    left: &Int64Input,
    operator: ComparisonOperator,
    right: &Float64Input,
) -> TruthBitmap {
    match (left, right) {
        (Int64Input::Column(left), Float64Input::Column(right)) => {
            let left = int64_slice(table, *left, start, len);
            let right = float64_slice(table, *right, start, len);
            evaluate_active(active, operator, |row| int_float_cmp(left[row], right[row]))
        }
        (Int64Input::Column(left), Float64Input::Literal(right)) => {
            let left = int64_slice(table, *left, start, len);
            evaluate_active(active, operator, |row| int_float_cmp(left[row], *right))
        }
        (Int64Input::Literal(left), Float64Input::Column(right)) => {
            let right = float64_slice(table, *right, start, len);
            evaluate_active(active, operator, |row| int_float_cmp(*left, right[row]))
        }
        (Int64Input::Literal(left), Float64Input::Literal(right)) => {
            TruthBitmap::constant(compare(int_float_cmp(*left, *right), operator), active)
        }
    }
}

fn evaluate_float64_int64(
    table: &Table,
    start: usize,
    len: usize,
    active: &BlockBitmap,
    left: &Float64Input,
    operator: ComparisonOperator,
    right: &Int64Input,
) -> TruthBitmap {
    match (left, right) {
        (Float64Input::Column(left), Int64Input::Column(right)) => {
            let left = float64_slice(table, *left, start, len);
            let right = int64_slice(table, *right, start, len);
            evaluate_active(active, operator, |row| {
                int_float_cmp(right[row], left[row]).map(Ordering::reverse)
            })
        }
        (Float64Input::Column(left), Int64Input::Literal(right)) => {
            let left = float64_slice(table, *left, start, len);
            evaluate_active(active, operator, |row| {
                int_float_cmp(*right, left[row]).map(Ordering::reverse)
            })
        }
        (Float64Input::Literal(left), Int64Input::Column(right)) => {
            let right = int64_slice(table, *right, start, len);
            evaluate_active(active, operator, |row| {
                int_float_cmp(right[row], *left).map(Ordering::reverse)
            })
        }
        (Float64Input::Literal(left), Int64Input::Literal(right)) => TruthBitmap::constant(
            compare(
                int_float_cmp(*right, *left).map(Ordering::reverse),
                operator,
            ),
            active,
        ),
    }
}

fn evaluate_active(
    active: &BlockBitmap,
    operator: ComparisonOperator,
    mut comparison: impl FnMut(usize) -> Option<Ordering>,
) -> TruthBitmap {
    let mut result = TruthBitmap::constant(SqlTruth::False, active);
    for (word_index, active_word) in active.words.iter().copied().enumerate() {
        let mut remaining = active_word;
        while remaining != 0 {
            let bit = remaining.trailing_zeros() as usize;
            remaining &= remaining - 1;
            let row = word_index * u64::BITS as usize + bit;
            match compare(comparison(row), operator) {
                SqlTruth::True => result.truth.words[word_index] |= 1_u64 << bit,
                SqlTruth::Unknown => result.unknown.words[word_index] |= 1_u64 << bit,
                SqlTruth::False => {}
            }
        }
    }
    result
}

fn compare(ordering: Option<Ordering>, operator: ComparisonOperator) -> SqlTruth {
    let Some(ordering) = ordering else {
        return SqlTruth::Unknown;
    };
    let matches = match operator {
        ComparisonOperator::Equal => ordering == Ordering::Equal,
        ComparisonOperator::NotEqual => ordering != Ordering::Equal,
        ComparisonOperator::Less => ordering == Ordering::Less,
        ComparisonOperator::LessOrEqual => ordering != Ordering::Greater,
        ComparisonOperator::Greater => ordering == Ordering::Greater,
        ComparisonOperator::GreaterOrEqual => ordering != Ordering::Less,
    };
    if matches {
        SqlTruth::True
    } else {
        SqlTruth::False
    }
}

fn int64_slice(table: &Table, column: usize, start: usize, len: usize) -> &[i64] {
    let Column::Int64(values) = &table.columns()[column] else {
        unreachable!("predicate column type is resolved")
    };
    &values[start..start + len]
}

fn float64_slice(table: &Table, column: usize, start: usize, len: usize) -> &[f64] {
    let Column::Float64(values) = &table.columns()[column] else {
        unreachable!("predicate column type is resolved")
    };
    &values[start..start + len]
}

fn bool_slice(table: &Table, column: usize, start: usize, len: usize) -> &[bool] {
    let Column::Bool(values) = &table.columns()[column] else {
        unreachable!("predicate column type is resolved")
    };
    &values[start..start + len]
}

fn string_slice(table: &Table, column: usize, start: usize, len: usize) -> &[String] {
    let Column::String(values) = &table.columns()[column] else {
        unreachable!("predicate column type is resolved")
    };
    &values[start..start + len]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::ColumnDef;

    fn table_with_rows(row_count: usize) -> Table {
        let mut table = Table::new(
            "decisions".to_owned(),
            vec![
                ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDef {
                    name: "score".to_owned(),
                    data_type: DataType::Float64,
                },
                ColumnDef {
                    name: "enabled".to_owned(),
                    data_type: DataType::Bool,
                },
                ColumnDef {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ],
        )
        .expect("schema");
        for row in 0..row_count {
            table
                .insert_row(vec![
                    Value::Int64(row as i64 - 700),
                    Value::Float64(row as f64 / 3.0 - 200.0),
                    Value::Bool(row.is_multiple_of(3)),
                    Value::String(format!("label_{:02}", row % 17)),
                ])
                .expect("row");
        }
        table
    }

    fn comparison(left: Operand, operator: ComparisonOperator, right: Operand) -> Predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        }
    }

    #[test]
    fn kernels_cross_block_boundaries_and_cover_all_physical_types() {
        let table = table_with_rows(BLOCK_ROWS * 2 + 37);
        let predicate = Predicate::Or(
            Box::new(Predicate::And(
                Box::new(comparison(
                    Operand::Column("id".to_owned()),
                    ComparisonOperator::GreaterOrEqual,
                    Operand::Literal(Value::Float64(900.0)),
                )),
                Box::new(comparison(
                    Operand::Column("enabled".to_owned()),
                    ComparisonOperator::Equal,
                    Operand::Literal(Value::Bool(true)),
                )),
            )),
            Box::new(Predicate::And(
                Box::new(comparison(
                    Operand::Column("score".to_owned()),
                    ComparisonOperator::Less,
                    Operand::Column("id".to_owned()),
                )),
                Box::new(comparison(
                    Operand::Column("label".to_owned()),
                    ComparisonOperator::Equal,
                    Operand::Literal(Value::String("label_05".to_owned())),
                )),
            )),
        );

        let selected = select_rows(&table, &predicate).expect("compiled selection");
        let expected = (0..table.row_count())
            .filter(|row| {
                let id = *row as i64 - 700;
                let score = *row as f64 / 3.0 - 200.0;
                (id >= 900 && row.is_multiple_of(3)) || (score < id as f64 && row % 17 == 5)
            })
            .collect::<Vec<_>>();
        assert_eq!(selected.iter().collect::<Vec<_>>(), expected);
        assert_eq!(selected.len(), expected.len());
    }

    #[test]
    fn active_masks_preserve_three_valued_and_or_semantics() {
        let table = table_with_rows(3);
        let unknown = comparison(
            Operand::Literal(Value::Float64(f64::NAN)),
            ComparisonOperator::Equal,
            Operand::Literal(Value::Float64(1.0)),
        );
        let truth = comparison(
            Operand::Literal(Value::Int64(1)),
            ComparisonOperator::Equal,
            Operand::Literal(Value::Int64(1)),
        );
        let falsity = comparison(
            Operand::Literal(Value::Bool(true)),
            ComparisonOperator::Equal,
            Operand::Literal(Value::Bool(false)),
        );

        let unknown_or_true = Predicate::Or(Box::new(unknown.clone()), Box::new(truth));
        assert_eq!(
            select_rows(&table, &unknown_or_true)
                .expect("selection")
                .iter()
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );

        let unknown_and_false = Predicate::And(Box::new(unknown.clone()), Box::new(falsity));
        assert!(
            select_rows(&table, &unknown_and_false)
                .expect("selection")
                .iter()
                .next()
                .is_none()
        );

        assert!(
            select_rows(&table, &unknown)
                .expect("selection")
                .iter()
                .next()
                .is_none()
        );
    }
}
