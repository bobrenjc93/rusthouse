use std::mem::size_of;

use crate::batch::{Column, RecordBatch};
use crate::error::{Error, Result};

use super::aggregate::{AggregateExpr, AggregateKind, AggregateResult, ScalarValue, SumValue};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const CANONICAL_NAN: u64 = 0x7ff8_0000_0000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupByConfig {
    pub max_groups: usize,
    pub memory_limit_bytes: usize,
}

impl GroupByConfig {
    pub const fn new(max_groups: usize, memory_limit_bytes: usize) -> Self {
        Self {
            max_groups,
            memory_limit_bytes,
        }
    }

    pub const fn unlimited(max_groups: usize) -> Self {
        Self::new(max_groups, usize::MAX)
    }
}

/// A normalized, owned grouping key. NULLs and all NaNs form one group; signed zeroes coalesce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupKey {
    Null,
    Int64(i64),
    Float64(u64),
    Boolean(bool),
    String(Box<str>),
}

impl GroupKey {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float64(bits) => Some(f64::from_bits(*bits)),
            _ => None,
        }
    }

    pub fn retained_bytes(&self) -> usize {
        match self {
            Self::String(value) => value.len(),
            _ => 0,
        }
    }
}

#[derive(Debug)]
enum Accumulator {
    Count(u64),
    SumInt { total: i128, seen: bool },
    SumFloat { total: f64, seen: bool },
    MinInt(Option<i64>),
    MinFloat(Option<f64>),
    MinBool(Option<bool>),
    MinString(Option<Box<str>>),
    MaxInt(Option<i64>),
    MaxFloat(Option<f64>),
    MaxBool(Option<bool>),
    MaxString(Option<Box<str>>),
    AvgInt { total: i128, count: u64 },
    AvgFloat { total: f64, count: u64 },
}

impl Accumulator {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::MinString(Some(value)) | Self::MaxString(Some(value)) => value.len(),
            _ => 0,
        }
    }

    fn result(&self) -> AggregateResult {
        match self {
            Self::Count(value) => AggregateResult::Count(*value),
            Self::SumInt { total, seen } => {
                AggregateResult::Sum(seen.then_some(SumValue::Int128(*total)))
            }
            Self::SumFloat { total, seen } => {
                AggregateResult::Sum(seen.then_some(SumValue::Float64(*total)))
            }
            Self::MinInt(value) => AggregateResult::Min(value.map(ScalarValue::Int64)),
            Self::MinFloat(value) => AggregateResult::Min(value.map(ScalarValue::Float64)),
            Self::MinBool(value) => AggregateResult::Min(value.map(ScalarValue::Boolean)),
            Self::MinString(value) => AggregateResult::Min(
                value
                    .as_ref()
                    .map(|value| ScalarValue::String(value.clone())),
            ),
            Self::MaxInt(value) => AggregateResult::Max(value.map(ScalarValue::Int64)),
            Self::MaxFloat(value) => AggregateResult::Max(value.map(ScalarValue::Float64)),
            Self::MaxBool(value) => AggregateResult::Max(value.map(ScalarValue::Boolean)),
            Self::MaxString(value) => AggregateResult::Max(
                value
                    .as_ref()
                    .map(|value| ScalarValue::String(value.clone())),
            ),
            Self::AvgInt { total, count } => {
                AggregateResult::Avg((*count != 0).then(|| *total as f64 / *count as f64))
            }
            Self::AvgFloat { total, count } => {
                AggregateResult::Avg((*count != 0).then(|| *total / *count as f64))
            }
        }
    }
}

#[derive(Debug)]
struct Group {
    hash: u64,
    keys: Box<[GroupKey]>,
    accumulators: Box<[Accumulator]>,
}

impl Group {
    fn retained_bytes(&self) -> usize {
        self.keys.len() * size_of::<GroupKey>()
            + self
                .keys
                .iter()
                .map(GroupKey::retained_bytes)
                .sum::<usize>()
            + self.accumulators.len() * size_of::<Accumulator>()
            + self
                .accumulators
                .iter()
                .map(Accumulator::retained_bytes)
                .sum::<usize>()
    }
}

/// Borrowed access to one group in deterministic first-seen order.
#[derive(Debug, Clone, Copy)]
pub struct GroupView<'a> {
    group: &'a Group,
}

impl<'a> GroupView<'a> {
    pub fn keys(self) -> &'a [GroupKey] {
        &self.group.keys
    }

    pub fn aggregate_count(self) -> usize {
        self.group.accumulators.len()
    }

    pub fn aggregate(self, index: usize) -> Option<AggregateResult> {
        self.group.accumulators.get(index).map(Accumulator::result)
    }
}

/// Bounded hash-grouping output, including the retained hash table for exact accounting.
#[derive(Debug)]
pub struct GroupedResults {
    slots: Box<[Option<usize>]>,
    groups: Box<[Option<Group>]>,
    group_count: usize,
    retained_bytes: usize,
    peak_retained_bytes: usize,
    memory_limit_bytes: usize,
}

impl GroupedResults {
    pub fn len(&self) -> usize {
        self.group_count
    }

    pub fn is_empty(&self) -> bool {
        self.group_count == 0
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = GroupView<'_>> {
        self.groups[..self.group_count]
            .iter()
            .map(|group| GroupView {
                group: group.as_ref().expect("groups are inserted contiguously"),
            })
    }

    /// Exact owned heap bytes, excluding allocator metadata and borrowed input arrays.
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    pub const fn peak_retained_bytes(&self) -> usize {
        self.peak_retained_bytes
    }

    pub const fn memory_limit_bytes(&self) -> usize {
        self.memory_limit_bytes
    }

    fn update_peak(&mut self) {
        self.peak_retained_bytes = self.peak_retained_bytes.max(self.retained_bytes);
    }
}

pub fn hash_group(
    batch: &RecordBatch,
    key_columns: &[usize],
    aggregates: &[AggregateExpr],
    config: GroupByConfig,
) -> Result<GroupedResults> {
    validate(batch, key_columns, aggregates)?;
    let slot_count = config
        .max_groups
        .max(1)
        .checked_mul(2)
        .and_then(usize::checked_next_power_of_two)
        .ok_or(Error::InvalidCapacity {
            capacity: config.max_groups,
        })?;
    let base_retained = slot_count
        .checked_mul(size_of::<Option<usize>>())
        .and_then(|slots| {
            config
                .max_groups
                .checked_mul(size_of::<Option<Group>>())
                .and_then(|groups| slots.checked_add(groups))
        })
        .ok_or(Error::InvalidCapacity {
            capacity: config.max_groups,
        })?;
    ensure_memory(base_retained, config.memory_limit_bytes)?;

    let mut result = GroupedResults {
        slots: vec![None; slot_count].into_boxed_slice(),
        groups: std::iter::repeat_with(|| None)
            .take(config.max_groups)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
        group_count: 0,
        retained_bytes: base_retained,
        peak_retained_bytes: base_retained,
        memory_limit_bytes: config.memory_limit_bytes,
    };

    for word_index in 0..batch.selection().word_count() {
        let mut selected = batch.selection().word(word_index);
        while selected != 0 {
            let bit = selected.trailing_zeros() as usize;
            let row = word_index * u64::BITS as usize + bit;
            insert_or_update(&mut result, batch, row, key_columns, aggregates)?;
            selected &= selected - 1;
        }
    }
    Ok(result)
}

fn validate(
    batch: &RecordBatch,
    key_columns: &[usize],
    aggregates: &[AggregateExpr],
) -> Result<()> {
    for &column in key_columns {
        batch.column(column)?;
    }
    for expression in aggregates {
        match (expression.kind, expression.column) {
            (AggregateKind::Count, None) => {}
            (_, None) => {
                return Err(Error::InvalidAggregate {
                    aggregate: expression.kind.name(),
                    reason: "a column is required",
                });
            }
            (AggregateKind::Count | AggregateKind::Min | AggregateKind::Max, Some(column)) => {
                batch.column(column)?;
            }
            (AggregateKind::Sum | AggregateKind::Avg, Some(column)) => {
                let column = batch.column(column)?;
                if !matches!(column, Column::Int64(_) | Column::Float64(_)) {
                    return Err(Error::UnsupportedAggregate {
                        aggregate: expression.kind.name(),
                        data_type: column.data_type().name(),
                    });
                }
            }
        }
    }
    Ok(())
}

fn insert_or_update(
    result: &mut GroupedResults,
    batch: &RecordBatch,
    row: usize,
    key_columns: &[usize],
    expressions: &[AggregateExpr],
) -> Result<()> {
    let hash = hash_row(batch, row, key_columns);
    let mask = result.slots.len() - 1;
    let mut slot = hash as usize & mask;
    loop {
        match result.slots[slot] {
            Some(group_index) => {
                let matches = {
                    let group = result.groups[group_index]
                        .as_ref()
                        .expect("hash slot points at a group");
                    group.hash == hash && keys_match(&group.keys, batch, row, key_columns)
                };
                if matches {
                    update_existing(result, group_index, batch, row, expressions)?;
                    return Ok(());
                }
                slot = (slot + 1) & mask;
            }
            None => {
                if result.group_count == result.groups.len() {
                    return Err(Error::GroupLimitExceeded {
                        max_groups: result.groups.len(),
                    });
                }
                let estimated = estimate_group_bytes(batch, row, key_columns, expressions)?;
                let required = result.retained_bytes.checked_add(estimated).ok_or(
                    Error::MemoryLimitExceeded {
                        operator: "hash grouping",
                        required: usize::MAX,
                        limit: result.memory_limit_bytes,
                    },
                )?;
                ensure_memory(required, result.memory_limit_bytes)?;

                let mut group = Group {
                    hash,
                    keys: build_keys(batch, row, key_columns).into_boxed_slice(),
                    accumulators: build_accumulators(batch, expressions).into_boxed_slice(),
                };
                let mut retained = result.retained_bytes + group.retained_bytes();
                for (accumulator, expression) in group.accumulators.iter_mut().zip(expressions) {
                    retained = update_accumulator(
                        accumulator,
                        *expression,
                        batch,
                        row,
                        retained,
                        result.memory_limit_bytes,
                    )?;
                }
                debug_assert_eq!(retained, required);

                let group_index = result.group_count;
                result.groups[group_index] = Some(group);
                result.slots[slot] = Some(group_index);
                result.group_count += 1;
                result.retained_bytes = retained;
                result.update_peak();
                return Ok(());
            }
        }
    }
}

fn update_existing(
    result: &mut GroupedResults,
    group_index: usize,
    batch: &RecordBatch,
    row: usize,
    expressions: &[AggregateExpr],
) -> Result<()> {
    let group = result.groups[group_index]
        .as_mut()
        .expect("hash slot points at a group");
    let mut retained = result.retained_bytes;
    for (accumulator, expression) in group.accumulators.iter_mut().zip(expressions) {
        retained = update_accumulator(
            accumulator,
            *expression,
            batch,
            row,
            retained,
            result.memory_limit_bytes,
        )?;
    }
    result.retained_bytes = retained;
    result.update_peak();
    Ok(())
}

fn build_accumulators(batch: &RecordBatch, expressions: &[AggregateExpr]) -> Vec<Accumulator> {
    expressions
        .iter()
        .map(|expression| match (expression.kind, expression.column) {
            (AggregateKind::Count, _) => Accumulator::Count(0),
            (AggregateKind::Sum, Some(column)) => match batch.columns()[column] {
                Column::Int64(_) => Accumulator::SumInt {
                    total: 0,
                    seen: false,
                },
                Column::Float64(_) => Accumulator::SumFloat {
                    total: 0.0,
                    seen: false,
                },
                _ => unreachable!("aggregate expressions were validated"),
            },
            (AggregateKind::Min, Some(column)) => match batch.columns()[column] {
                Column::Int64(_) => Accumulator::MinInt(None),
                Column::Float64(_) => Accumulator::MinFloat(None),
                Column::Boolean(_) => Accumulator::MinBool(None),
                Column::String(_) => Accumulator::MinString(None),
            },
            (AggregateKind::Max, Some(column)) => match batch.columns()[column] {
                Column::Int64(_) => Accumulator::MaxInt(None),
                Column::Float64(_) => Accumulator::MaxFloat(None),
                Column::Boolean(_) => Accumulator::MaxBool(None),
                Column::String(_) => Accumulator::MaxString(None),
            },
            (AggregateKind::Avg, Some(column)) => match batch.columns()[column] {
                Column::Int64(_) => Accumulator::AvgInt { total: 0, count: 0 },
                Column::Float64(_) => Accumulator::AvgFloat {
                    total: 0.0,
                    count: 0,
                },
                _ => unreachable!("aggregate expressions were validated"),
            },
            (_, None) => unreachable!("aggregate expressions were validated"),
        })
        .collect()
}

fn update_accumulator(
    accumulator: &mut Accumulator,
    expression: AggregateExpr,
    batch: &RecordBatch,
    row: usize,
    retained: usize,
    limit: usize,
) -> Result<usize> {
    let column = expression.column.map(|column| &batch.columns()[column]);
    let valid = column.is_none_or(|column| column.validity().get(row));
    match (accumulator, column) {
        (Accumulator::Count(count), _) if valid => {
            *count = count
                .checked_add(1)
                .ok_or(Error::ArithmeticOverflow { aggregate: "COUNT" })?;
        }
        (Accumulator::Count(_), _) => {}
        (Accumulator::SumInt { total, seen }, Some(Column::Int64(array))) if valid => {
            *total = total
                .checked_add(i128::from(array.values()[row]))
                .ok_or(Error::ArithmeticOverflow { aggregate: "SUM" })?;
            *seen = true;
        }
        (Accumulator::SumFloat { total, seen }, Some(Column::Float64(array))) if valid => {
            *total += array.values()[row];
            *seen = true;
        }
        (Accumulator::MinInt(value), Some(Column::Int64(array))) if valid => {
            update_ord(value, array.values()[row], true);
        }
        (Accumulator::MaxInt(value), Some(Column::Int64(array))) if valid => {
            update_ord(value, array.values()[row], false);
        }
        (Accumulator::MinFloat(value), Some(Column::Float64(array))) if valid => {
            update_float(value, array.values()[row], true);
        }
        (Accumulator::MaxFloat(value), Some(Column::Float64(array))) if valid => {
            update_float(value, array.values()[row], false);
        }
        (Accumulator::MinBool(value), Some(Column::Boolean(array))) if valid => {
            update_ord(value, array.value(row).expect("row is valid"), true);
        }
        (Accumulator::MaxBool(value), Some(Column::Boolean(array))) if valid => {
            update_ord(value, array.value(row).expect("row is valid"), false);
        }
        (Accumulator::MinString(value), Some(Column::String(array))) if valid => {
            return update_string(
                value,
                array.value(row).expect("row is valid"),
                true,
                retained,
                limit,
            );
        }
        (Accumulator::MaxString(value), Some(Column::String(array))) if valid => {
            return update_string(
                value,
                array.value(row).expect("row is valid"),
                false,
                retained,
                limit,
            );
        }
        (Accumulator::AvgInt { total, count }, Some(Column::Int64(array))) if valid => {
            *total = total
                .checked_add(i128::from(array.values()[row]))
                .ok_or(Error::ArithmeticOverflow { aggregate: "AVG" })?;
            *count = count
                .checked_add(1)
                .ok_or(Error::ArithmeticOverflow { aggregate: "AVG" })?;
        }
        (Accumulator::AvgFloat { total, count }, Some(Column::Float64(array))) if valid => {
            *total += array.values()[row];
            *count = count
                .checked_add(1)
                .ok_or(Error::ArithmeticOverflow { aggregate: "AVG" })?;
        }
        _ => {}
    }
    Ok(retained)
}

fn update_ord<T: Ord + Copy>(value: &mut Option<T>, candidate: T, is_min: bool) {
    if value
        .is_none_or(|current| (is_min && candidate < current) || (!is_min && candidate > current))
    {
        *value = Some(candidate);
    }
}

fn update_float(value: &mut Option<f64>, candidate: f64, is_min: bool) {
    if value.is_none_or(|current| {
        let ordering = candidate.total_cmp(&current);
        (is_min && ordering.is_lt()) || (!is_min && ordering.is_gt())
    }) {
        *value = Some(candidate);
    }
}

fn update_string(
    value: &mut Option<Box<str>>,
    candidate: &str,
    is_min: bool,
    retained: usize,
    limit: usize,
) -> Result<usize> {
    let replace = value
        .as_deref()
        .is_none_or(|current| (is_min && candidate < current) || (!is_min && candidate > current));
    if !replace {
        return Ok(retained);
    }
    let old_len = value.as_deref().map_or(0, str::len);
    let required = retained
        .checked_sub(old_len)
        .and_then(|bytes| bytes.checked_add(candidate.len()))
        .ok_or(Error::MemoryLimitExceeded {
            operator: "hash grouping",
            required: usize::MAX,
            limit,
        })?;
    ensure_memory(required, limit)?;
    *value = None;
    *value = Some(candidate.into());
    Ok(required)
}

fn estimate_group_bytes(
    batch: &RecordBatch,
    row: usize,
    key_columns: &[usize],
    expressions: &[AggregateExpr],
) -> Result<usize> {
    let key_strings = key_columns
        .iter()
        .filter_map(|&column| match &batch.columns()[column] {
            Column::String(array) => array.value(row).map(str::len),
            _ => None,
        })
        .try_fold(0_usize, usize::checked_add)
        .ok_or(Error::MemoryLimitExceeded {
            operator: "hash grouping",
            required: usize::MAX,
            limit: usize::MAX,
        })?;
    let accumulator_strings = expressions
        .iter()
        .filter_map(|expression| {
            if !matches!(expression.kind, AggregateKind::Min | AggregateKind::Max) {
                return None;
            }
            match &batch.columns()[expression.column?] {
                Column::String(array) => array.value(row).map(str::len),
                _ => None,
            }
        })
        .try_fold(0_usize, usize::checked_add)
        .ok_or(Error::MemoryLimitExceeded {
            operator: "hash grouping",
            required: usize::MAX,
            limit: usize::MAX,
        })?;
    key_columns
        .len()
        .checked_mul(size_of::<GroupKey>())
        .and_then(|bytes| bytes.checked_add(key_strings))
        .and_then(|bytes| {
            expressions
                .len()
                .checked_mul(size_of::<Accumulator>())
                .and_then(|states| bytes.checked_add(states))
        })
        .and_then(|bytes| bytes.checked_add(accumulator_strings))
        .ok_or(Error::MemoryLimitExceeded {
            operator: "hash grouping",
            required: usize::MAX,
            limit: usize::MAX,
        })
}

fn build_keys(batch: &RecordBatch, row: usize, key_columns: &[usize]) -> Vec<GroupKey> {
    key_columns
        .iter()
        .map(|&column| match &batch.columns()[column] {
            Column::Int64(array) => array.value(row).map_or(GroupKey::Null, GroupKey::Int64),
            Column::Float64(array) => array.value(row).map_or(GroupKey::Null, |value| {
                GroupKey::Float64(canonical_float(value))
            }),
            Column::Boolean(array) => array.value(row).map_or(GroupKey::Null, GroupKey::Boolean),
            Column::String(array) => array
                .value(row)
                .map_or(GroupKey::Null, |value| GroupKey::String(value.into())),
        })
        .collect()
}

fn keys_match(keys: &[GroupKey], batch: &RecordBatch, row: usize, key_columns: &[usize]) -> bool {
    keys.iter()
        .zip(key_columns)
        .all(|(key, &column)| match (key, &batch.columns()[column]) {
            (GroupKey::Null, column) => !column.validity().get(row),
            (GroupKey::Int64(value), Column::Int64(array)) => array.value(row) == Some(*value),
            (GroupKey::Float64(bits), Column::Float64(array)) => array
                .value(row)
                .is_some_and(|value| canonical_float(value) == *bits),
            (GroupKey::Boolean(value), Column::Boolean(array)) => array.value(row) == Some(*value),
            (GroupKey::String(value), Column::String(array)) => {
                array.value(row) == Some(value.as_ref())
            }
            _ => false,
        })
}

fn hash_row(batch: &RecordBatch, row: usize, key_columns: &[usize]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &column in key_columns {
        let column = &batch.columns()[column];
        if !column.validity().get(row) {
            hash_bytes(&mut hash, &[0]);
            continue;
        }
        match column {
            Column::Int64(array) => {
                hash_bytes(&mut hash, &[1]);
                hash_bytes(&mut hash, &array.values()[row].to_le_bytes());
            }
            Column::Float64(array) => {
                hash_bytes(&mut hash, &[2]);
                hash_bytes(
                    &mut hash,
                    &canonical_float(array.values()[row]).to_le_bytes(),
                );
            }
            Column::Boolean(array) => {
                hash_bytes(
                    &mut hash,
                    &[3, u8::from(array.value(row).expect("row is valid"))],
                );
            }
            Column::String(array) => {
                let value = array.value(row).expect("row is valid");
                hash_bytes(&mut hash, &[4]);
                hash_bytes(&mut hash, &value.len().to_le_bytes());
                hash_bytes(&mut hash, value.as_bytes());
            }
        }
    }
    hash
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

fn canonical_float(value: f64) -> u64 {
    if value == 0.0 {
        0
    } else if value.is_nan() {
        CANONICAL_NAN
    } else {
        value.to_bits()
    }
}

fn ensure_memory(required: usize, limit: usize) -> Result<()> {
    if required > limit {
        Err(Error::MemoryLimitExceeded {
            operator: "hash grouping",
            required,
            limit,
        })
    } else {
        Ok(())
    }
}
