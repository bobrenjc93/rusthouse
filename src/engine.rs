use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, Join, JoinKind, MAX_JOIN_KEYS,
    Operand, OrderBy, Predicate, Select, SelectItem, Statement,
};
use crate::storage::Table;
use crate::value::{DataType, Value, ValueRef};

pub const DEFAULT_JOIN_MAX_ROWS: usize = 1_000_000;
pub const DEFAULT_JOIN_MAX_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_JOIN_MAX_CANDIDATE_PAIRS: usize = 1_000_000;

/// Per-operator bounds for a JOIN's hash input and output working set.
///
/// `max_rows` limits both hash-build rows and joined output pairs.
/// `max_candidate_pairs` limits hash-key matches examined before residual `ON`
/// filtering; materialization can only revisit that bounded set. `max_bytes`
/// limits estimated peak allocation bytes for buckets, entries, flat hash keys,
/// row chains, retained input rows, and temporary output pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinLimits {
    pub max_rows: usize,
    pub max_bytes: usize,
    pub max_candidate_pairs: usize,
}

impl Default for JoinLimits {
    fn default() -> Self {
        Self {
            max_rows: DEFAULT_JOIN_MAX_ROWS,
            max_bytes: DEFAULT_JOIN_MAX_BYTES,
            max_candidate_pairs: DEFAULT_JOIN_MAX_CANDIDATE_PAIRS,
        }
    }
}

/// A reusable in-memory SQL database.
#[derive(Debug, Default)]
pub struct Database {
    catalog: Catalog,
    join_limits: JoinLimits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultColumn {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatementResult {
    Command {
        tag: &'static str,
        affected_rows: usize,
    },
    Query(QueryResult),
}

impl Database {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_join_limits(join_limits: JoinLimits) -> Self {
        Self {
            catalog: Catalog::new(),
            join_limits,
        }
    }

    #[must_use]
    pub fn join_limits(&self) -> JoinLimits {
        self.join_limits
    }

    pub fn set_join_limits(&mut self, join_limits: JoinLimits) {
        self.join_limits = join_limits;
    }

    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Execute one or more semicolon-separated statements in order.
    ///
    /// The complete batch is parsed before execution, so a syntax error applies
    /// nothing. Once parsing succeeds, statements execute in order and earlier
    /// statements remain applied if a later execution error occurs.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<StatementResult>> {
        sql::parse(sql)?
            .into_iter()
            .map(|statement| self.execute_statement(statement))
            .collect()
    }

    fn execute_statement(&mut self, statement: Statement) -> Result<StatementResult> {
        match statement {
            Statement::CreateTable { name, columns } => {
                self.catalog.create_table(name, columns)?;
                Ok(StatementResult::Command {
                    tag: "CREATE TABLE",
                    affected_rows: 0,
                })
            }
            Statement::Insert { table, rows } => {
                let affected_rows = rows.len();
                {
                    let target = self.catalog.table(&table)?;
                    for row in &rows {
                        target.validate_row(row)?;
                    }
                }
                let target = self.catalog.table_mut(&table)?;
                for row in rows {
                    target.insert_row(row)?;
                }
                Ok(StatementResult::Command {
                    tag: "INSERT",
                    affected_rows,
                })
            }
            Statement::Select(select) => self.execute_select(select).map(StatementResult::Query),
        }
    }

    fn execute_select(&self, select: Select) -> Result<QueryResult> {
        let input = QueryInput::build(
            &self.catalog,
            &select.table,
            select.table_alias.as_deref(),
            &select.joins,
            self.join_limits,
        )?;
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| compile_predicate(&input, predicate))
            .transpose()?;

        let mut matching_rows = (0..input.row_count())
            .filter(|row| {
                predicate
                    .as_ref()
                    .is_none_or(|predicate| predicate.evaluate(&input, *row))
            })
            .collect::<Vec<_>>();

        let group_columns = resolve_group_columns(&input, &select.group_by)?;
        let (items, result_columns, aggregate_specs) =
            resolve_select_items(&input, &select.items, &group_columns)?;
        let ordering = resolve_ordering(&input, &items, &result_columns, &select.order_by)?;

        let grouped = !group_columns.is_empty() || !aggregate_specs.is_empty();
        let rows = if grouped {
            let grouped =
                execute_grouped(&input, &matching_rows, &group_columns, &aggregate_specs)?;
            let mut selected_groups = (0..grouped.len()).collect::<Vec<_>>();
            order_grouped_rows(
                &mut selected_groups,
                &grouped,
                &items,
                &ordering,
                select.limit,
            );
            grouped.project(&selected_groups, &items)
        } else {
            order_source_rows(&mut matching_rows, &input, &items, &ordering, select.limit);
            execute_projection(&input, &matching_rows, &items)
        };

        Ok(QueryResult {
            columns: result_columns,
            rows,
        })
    }
}

#[derive(Debug)]
struct Relation<'a> {
    qualifier: String,
    table: &'a Table,
}

#[derive(Debug, Clone)]
struct InputColumn<'a> {
    relation: usize,
    physical: usize,
    name: &'a str,
    data_type: DataType,
    nullable: bool,
}

#[derive(Debug)]
struct QueryInput<'a> {
    relations: Vec<Relation<'a>>,
    columns: Vec<InputColumn<'a>>,
    rows: Option<Vec<usize>>,
    row_count: usize,
}

impl<'a> QueryInput<'a> {
    fn build(
        catalog: &'a Catalog,
        table_name: &str,
        alias: Option<&str>,
        joins: &[Join],
        limits: JoinLimits,
    ) -> Result<Self> {
        let table = catalog.table(table_name)?;
        let mut input = Self {
            relations: Vec::new(),
            columns: Vec::new(),
            rows: None,
            row_count: table.row_count(),
        };
        input.push_relation(table, alias.unwrap_or(table_name), false)?;
        for join in joins {
            input.apply_join(catalog, join, limits)?;
        }
        Ok(input)
    }

    fn push_relation(
        &mut self,
        table: &'a Table,
        qualifier: &str,
        force_nullable: bool,
    ) -> Result<()> {
        if self
            .relations
            .iter()
            .any(|relation| relation.qualifier.eq_ignore_ascii_case(qualifier))
        {
            return Err(Error::InvalidQuery(format!(
                "table name or alias '{qualifier}' is specified more than once"
            )));
        }
        let relation = self.relations.len();
        self.relations.push(Relation {
            qualifier: qualifier.to_owned(),
            table,
        });
        self.columns
            .extend(
                table
                    .schema()
                    .iter()
                    .enumerate()
                    .map(|(physical, field)| InputColumn {
                        relation,
                        physical,
                        name: &field.name,
                        data_type: field.data_type,
                        nullable: force_nullable || field.nullable,
                    }),
            );
        Ok(())
    }

    fn row_count(&self) -> usize {
        self.row_count
    }

    fn column_index(&self, name: &str) -> Result<usize> {
        if let Some((qualifier, column_name)) = name.split_once('.') {
            let relation = self
                .relations
                .iter()
                .position(|relation| relation.qualifier.eq_ignore_ascii_case(qualifier))
                .ok_or_else(|| {
                    Error::InvalidQuery(format!("unknown table name or alias '{qualifier}'"))
                })?;
            return self
                .columns
                .iter()
                .position(|column| {
                    column.relation == relation && column.name.eq_ignore_ascii_case(column_name)
                })
                .ok_or_else(|| Error::ColumnNotFound {
                    table: self.relations[relation].table.name().to_owned(),
                    column: column_name.to_owned(),
                });
        }

        let matches = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.name.eq_ignore_ascii_case(name))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [index] => Ok(*index),
            [] if self.relations.len() == 1 => Err(Error::ColumnNotFound {
                table: self.relations[0].table.name().to_owned(),
                column: name.to_owned(),
            }),
            [] => Err(Error::InvalidQuery(format!(
                "column '{name}' does not exist in any input table"
            ))),
            _ => Err(Error::InvalidQuery(format!(
                "column reference '{name}' is ambiguous; qualify it with a table name or alias"
            ))),
        }
    }

    fn qualifier_index(&self, qualifier: &str) -> Result<usize> {
        self.relations
            .iter()
            .position(|relation| relation.qualifier.eq_ignore_ascii_case(qualifier))
            .ok_or_else(|| {
                Error::InvalidQuery(format!("unknown table name or alias '{qualifier}'"))
            })
    }

    fn physical_row(&self, logical_row: usize, relation: usize) -> usize {
        self.rows.as_ref().map_or(logical_row, |rows| {
            rows[logical_row * self.relations.len() + relation]
        })
    }

    fn value(&self, column: usize, logical_row: usize) -> ValueRef<'_> {
        let column = &self.columns[column];
        let physical_row = self.physical_row(logical_row, column.relation);
        if physical_row == NO_JOIN_INDEX {
            ValueRef::Null
        } else {
            self.relations[column.relation]
                .table
                .value_ref(column.physical, physical_row)
        }
    }

    fn cmp_at(&self, column: usize, left: usize, right: usize) -> Ordering {
        self.value(column, left).cmp(&self.value(column, right))
    }

    fn apply_join(&mut self, catalog: &'a Catalog, join: &Join, limits: JoinLimits) -> Result<()> {
        let old_width = self.relations.len();
        let old_row_count = self.row_count;
        let old_rows = self.rows.take();
        let table = catalog.table(&join.table)?;
        self.push_relation(
            table,
            join.alias.as_deref().unwrap_or(&join.table),
            join.kind == JoinKind::Left,
        )?;
        let right_relation = old_width;
        let join_name = join.kind.name();

        let mut keys = Vec::with_capacity(join.conditions.len());
        let mut predicates = Vec::new();
        for condition in &join.conditions {
            let left = self.column_index(&condition.left)?;
            let right = self.column_index(&condition.right)?;
            let left_relation = self.columns[left].relation;
            let right_side_relation = self.columns[right].relation;
            let key = match (
                left_relation == right_relation,
                right_side_relation == right_relation,
            ) {
                (false, true) => Some((left, right)),
                (true, false) => Some((right, left)),
                _ => None,
            };
            if let Some((left_column, right_column)) = key {
                if keys.len() >= MAX_JOIN_KEYS {
                    return Err(Error::InvalidQuery(format!(
                        "{join_name} has too many equality keys; maximum is {MAX_JOIN_KEYS}"
                    )));
                }
                let left_type = self.columns[left_column].data_type;
                let right_type = self.columns[right_column].data_type;
                if !comparable(left_type, right_type) {
                    return Err(Error::TypeMismatch {
                        context: format!("{join_name} equality"),
                        expected: left_type.to_string(),
                        actual: right_type.to_string(),
                    });
                }
                keys.push((left_column, right_column));
            } else {
                let predicate = Predicate::Comparison {
                    left: Operand::Column(condition.left.clone()),
                    operator: ComparisonOperator::Equal,
                    right: Operand::Column(condition.right.clone()),
                };
                predicates.push(compile_predicate_for(self, &predicate, join_name)?);
            }
        }
        if keys.is_empty() {
            return Err(Error::InvalidQuery(format!(
                "{join_name} requires at least one equality connecting the input tables"
            )));
        }
        if let Some(predicate) = &join.predicate {
            predicates.push(compile_predicate_for(self, predicate, join_name)?);
        }
        let predicate = predicates
            .into_iter()
            .reduce(|left, right| CompiledPredicate::And(Box::new(left), Box::new(right)));

        let right_row_count = table.row_count();
        let build_left = old_row_count <= right_row_count;
        let build_rows = if build_left {
            old_row_count
        } else {
            right_row_count
        };
        if build_rows > limits.max_rows {
            return Err(Error::JoinLimitExceeded {
                resource: "rows",
                limit: limits.max_rows,
                actual: build_rows,
            });
        }

        let resident_bytes = checked_join_resident_bytes(old_rows.as_ref(), &keys, limits)?;
        let joined_rows = if old_row_count == 0 {
            Vec::new()
        } else if right_row_count == 0 {
            let output_count = if join.kind == JoinKind::Left {
                old_row_count
            } else {
                0
            };
            let joined_indices = checked_join_output_layout(
                output_count,
                old_width,
                None,
                resident_bytes,
                0,
                limits,
            )?;
            let mut joined = Vec::with_capacity(joined_indices);
            if join.kind == JoinKind::Left {
                for left_row in 0..old_row_count {
                    append_join_match(
                        &mut joined,
                        old_rows.as_deref(),
                        old_width,
                        left_row,
                        NO_JOIN_INDEX,
                    );
                }
            }
            joined
        } else if build_left {
            let layout = JoinHashLayout::new(build_rows, keys.len(), limits)?;
            checked_join_bytes(resident_bytes, layout.estimated_bytes, 0, limits)?;
            let mut hash = JoinHashTable::new(layout);
            let mut scratch = Vec::with_capacity(keys.len());
            for left_row in 0..old_row_count {
                if self.old_join_key(
                    old_rows.as_deref(),
                    old_width,
                    &keys,
                    left_row,
                    &mut scratch,
                ) {
                    hash.insert(&scratch, left_row);
                }
            }
            let mut match_count = 0usize;
            let matched_left_bytes = if join.kind == JoinKind::Left {
                std::mem::size_of::<Vec<u8>>()
                    .checked_add(checked_allocation_bytes::<u8>(old_row_count, limits)?)
                    .ok_or_else(|| join_byte_limit_error(limits, usize::MAX))?
            } else {
                0
            };
            let resident_bytes = resident_bytes
                .checked_add(matched_left_bytes)
                .ok_or_else(|| join_byte_limit_error(limits, usize::MAX))?;
            checked_join_bytes(resident_bytes, layout.estimated_bytes, 0, limits)?;
            let mut matched_left = if join.kind == JoinKind::Left {
                vec![0_u8; old_row_count]
            } else {
                Vec::new()
            };
            let mut candidate_count = 0usize;
            for right_row in 0..right_row_count {
                if !self.right_join_key(&keys, right_row, &mut scratch) {
                    continue;
                }
                if let Some(entry) = hash.get(&scratch) {
                    let mut left_row = entry.first_row;
                    while left_row != NO_JOIN_INDEX {
                        candidate_count = checked_join_candidate_count(candidate_count, 1, limits)?;
                        if self.on_matches(
                            predicate.as_ref(),
                            old_rows.as_deref(),
                            old_width,
                            left_row,
                            right_relation,
                            right_row,
                        ) {
                            match_count = checked_join_match_count(match_count, 1, limits)?;
                            if join.kind == JoinKind::Left {
                                matched_left[left_row] = 1;
                            }
                        }
                        left_row = hash.next_row(left_row);
                    }
                }
            }
            let output_count = if join.kind == JoinKind::Left {
                checked_join_match_count(
                    match_count,
                    matched_left.iter().filter(|matched| **matched == 0).count(),
                    limits,
                )?
            } else {
                match_count
            };
            let joined_indices = checked_join_output_layout(
                output_count,
                old_width,
                Some(match_count),
                resident_bytes,
                layout.estimated_bytes,
                limits,
            )?;
            let mut matches = Vec::with_capacity(match_count);
            for right_row in 0..right_row_count {
                if !self.right_join_key(&keys, right_row, &mut scratch) {
                    continue;
                }
                if let Some(entry) = hash.get(&scratch) {
                    let mut left_row = entry.first_row;
                    while left_row != NO_JOIN_INDEX {
                        if self.on_matches(
                            predicate.as_ref(),
                            old_rows.as_deref(),
                            old_width,
                            left_row,
                            right_relation,
                            right_row,
                        ) {
                            matches.push((left_row, right_row));
                        }
                        left_row = hash.next_row(left_row);
                    }
                }
            }
            debug_assert_eq!(matches.len(), match_count);
            matches.sort_unstable();
            drop(scratch);
            drop(hash);
            self.flatten_join_matches(
                old_rows.as_deref(),
                old_width,
                old_row_count,
                &matches,
                joined_indices,
                join.kind == JoinKind::Left,
            )
        } else {
            let layout = JoinHashLayout::new(build_rows, keys.len(), limits)?;
            checked_join_bytes(resident_bytes, layout.estimated_bytes, 0, limits)?;
            let mut hash = JoinHashTable::new(layout);
            let mut scratch = Vec::with_capacity(keys.len());
            for right_row in 0..right_row_count {
                if self.right_join_key(&keys, right_row, &mut scratch) {
                    hash.insert(&scratch, right_row);
                }
            }
            let mut output_count = 0usize;
            let mut candidate_count = 0usize;
            for left_row in 0..old_row_count {
                let mut matched = false;
                if self.old_join_key(
                    old_rows.as_deref(),
                    old_width,
                    &keys,
                    left_row,
                    &mut scratch,
                ) && let Some(entry) = hash.get(&scratch)
                {
                    let mut right_row = entry.first_row;
                    while right_row != NO_JOIN_INDEX {
                        candidate_count = checked_join_candidate_count(candidate_count, 1, limits)?;
                        if self.on_matches(
                            predicate.as_ref(),
                            old_rows.as_deref(),
                            old_width,
                            left_row,
                            right_relation,
                            right_row,
                        ) {
                            output_count = checked_join_match_count(output_count, 1, limits)?;
                            matched = true;
                        }
                        right_row = hash.next_row(right_row);
                    }
                }
                if !matched && join.kind == JoinKind::Left {
                    output_count = checked_join_match_count(output_count, 1, limits)?;
                }
            }
            let joined_indices = checked_join_output_layout(
                output_count,
                old_width,
                None,
                resident_bytes,
                layout.estimated_bytes,
                limits,
            )?;
            let mut joined = Vec::with_capacity(joined_indices);
            for left_row in 0..old_row_count {
                let mut matched = false;
                if self.old_join_key(
                    old_rows.as_deref(),
                    old_width,
                    &keys,
                    left_row,
                    &mut scratch,
                ) && let Some(entry) = hash.get(&scratch)
                {
                    let mut right_row = entry.first_row;
                    while right_row != NO_JOIN_INDEX {
                        if self.on_matches(
                            predicate.as_ref(),
                            old_rows.as_deref(),
                            old_width,
                            left_row,
                            right_relation,
                            right_row,
                        ) {
                            append_join_match(
                                &mut joined,
                                old_rows.as_deref(),
                                old_width,
                                left_row,
                                right_row,
                            );
                            matched = true;
                        }
                        right_row = hash.next_row(right_row);
                    }
                }
                if !matched && join.kind == JoinKind::Left {
                    append_join_match(
                        &mut joined,
                        old_rows.as_deref(),
                        old_width,
                        left_row,
                        NO_JOIN_INDEX,
                    );
                }
            }
            debug_assert_eq!(joined.len(), joined_indices);
            joined
        };

        self.row_count = joined_rows.len() / (old_width + 1);
        self.rows = Some(joined_rows);
        Ok(())
    }

    fn old_join_key<'s>(
        &'s self,
        old_rows: Option<&[usize]>,
        old_width: usize,
        keys: &[(usize, usize)],
        logical_row: usize,
        scratch: &mut Vec<JoinKeyPart<'s>>,
    ) -> bool {
        scratch.clear();
        for (left, _) in keys {
            let Some(part) =
                JoinKeyPart::from_value(self.old_value(old_rows, old_width, *left, logical_row))
            else {
                return false;
            };
            scratch.push(part);
        }
        true
    }

    fn right_join_key<'s>(
        &'s self,
        keys: &[(usize, usize)],
        physical_row: usize,
        scratch: &mut Vec<JoinKeyPart<'s>>,
    ) -> bool {
        scratch.clear();
        for (_, right) in keys {
            let Some(part) = JoinKeyPart::from_value(self.right_value(*right, physical_row)) else {
                return false;
            };
            scratch.push(part);
        }
        true
    }

    fn on_matches(
        &self,
        predicate: Option<&CompiledPredicate>,
        old_rows: Option<&[usize]>,
        old_width: usize,
        left_row: usize,
        right_relation: usize,
        right_row: usize,
    ) -> bool {
        predicate.is_none_or(|predicate| {
            predicate.evaluate_join(
                self,
                old_rows,
                old_width,
                left_row,
                right_relation,
                right_row,
            )
        })
    }

    fn old_value(
        &self,
        old_rows: Option<&[usize]>,
        old_width: usize,
        column: usize,
        logical_row: usize,
    ) -> ValueRef<'_> {
        let column = &self.columns[column];
        debug_assert!(column.relation < old_width);
        let physical_row = old_rows.map_or(logical_row, |rows| {
            rows[logical_row * old_width + column.relation]
        });
        if physical_row == NO_JOIN_INDEX {
            ValueRef::Null
        } else {
            self.relations[column.relation]
                .table
                .value_ref(column.physical, physical_row)
        }
    }

    fn right_value(&self, column: usize, physical_row: usize) -> ValueRef<'_> {
        let column = &self.columns[column];
        self.relations[column.relation]
            .table
            .value_ref(column.physical, physical_row)
    }

    fn flatten_join_matches(
        &self,
        old_rows: Option<&[usize]>,
        old_width: usize,
        old_row_count: usize,
        matches: &[(usize, usize)],
        joined_indices: usize,
        preserve_left: bool,
    ) -> Vec<usize> {
        let mut joined = Vec::with_capacity(joined_indices);
        if preserve_left {
            let mut next_match = 0;
            for left_row in 0..old_row_count {
                let start = next_match;
                while next_match < matches.len() && matches[next_match].0 == left_row {
                    append_join_match(
                        &mut joined,
                        old_rows,
                        old_width,
                        left_row,
                        matches[next_match].1,
                    );
                    next_match += 1;
                }
                if next_match == start {
                    append_join_match(&mut joined, old_rows, old_width, left_row, NO_JOIN_INDEX);
                }
            }
        } else {
            for (left_row, right_row) in matches {
                append_join_match(&mut joined, old_rows, old_width, *left_row, *right_row);
            }
        }
        debug_assert_eq!(joined.len(), joined_indices);
        joined
    }
}

fn checked_join_match_count(
    current: usize,
    additional: usize,
    limits: JoinLimits,
) -> Result<usize> {
    let actual = current
        .checked_add(additional)
        .ok_or(Error::JoinLimitExceeded {
            resource: "output rows",
            limit: limits.max_rows,
            actual: usize::MAX,
        })?;
    if actual > limits.max_rows {
        Err(Error::JoinLimitExceeded {
            resource: "output rows",
            limit: limits.max_rows,
            actual,
        })
    } else {
        Ok(actual)
    }
}

fn checked_join_candidate_count(
    current: usize,
    additional: usize,
    limits: JoinLimits,
) -> Result<usize> {
    let actual = current
        .checked_add(additional)
        .ok_or(Error::JoinLimitExceeded {
            resource: "candidate pairs",
            limit: limits.max_candidate_pairs,
            actual: usize::MAX,
        })?;
    if actual > limits.max_candidate_pairs {
        Err(Error::JoinLimitExceeded {
            resource: "candidate pairs",
            limit: limits.max_candidate_pairs,
            actual,
        })
    } else {
        Ok(actual)
    }
}

fn checked_join_output_layout(
    output_count: usize,
    old_width: usize,
    temporary_pair_count: Option<usize>,
    resident_bytes: usize,
    hash_bytes: usize,
    limits: JoinLimits,
) -> Result<usize> {
    if output_count > limits.max_rows {
        return Err(Error::JoinLimitExceeded {
            resource: "output rows",
            limit: limits.max_rows,
            actual: output_count,
        });
    }

    let overflow = || join_byte_limit_error(limits, usize::MAX);
    let joined_width = old_width.checked_add(1).ok_or_else(overflow)?;
    let joined_indices = output_count
        .checked_mul(joined_width)
        .ok_or_else(overflow)?;
    let joined_bytes = std::mem::size_of::<Vec<usize>>()
        .checked_add(checked_allocation_bytes::<usize>(joined_indices, limits)?)
        .ok_or_else(overflow)?;
    let pair_bytes = if let Some(pair_count) = temporary_pair_count {
        std::mem::size_of::<Vec<(usize, usize)>>()
            .checked_add(checked_allocation_bytes::<(usize, usize)>(
                pair_count, limits,
            )?)
            .ok_or_else(overflow)?
    } else {
        0
    };
    let peak_bytes = if temporary_pair_count.is_some() {
        resident_bytes
            .checked_add(pair_bytes)
            .ok_or_else(overflow)?
            .checked_add(hash_bytes)
            .ok_or_else(overflow)?
            .max(
                resident_bytes
                    .checked_add(pair_bytes)
                    .ok_or_else(overflow)?
                    .checked_add(joined_bytes)
                    .ok_or_else(overflow)?,
            )
    } else {
        resident_bytes
            .checked_add(hash_bytes)
            .ok_or_else(overflow)?
            .checked_add(joined_bytes)
            .ok_or_else(overflow)?
    };
    if peak_bytes > limits.max_bytes {
        return Err(Error::JoinLimitExceeded {
            resource: "bytes",
            limit: limits.max_bytes,
            actual: peak_bytes,
        });
    }
    Ok(joined_indices)
}

fn checked_join_resident_bytes(
    old_rows: Option<&Vec<usize>>,
    keys: &Vec<(usize, usize)>,
    limits: JoinLimits,
) -> Result<usize> {
    let old_rows_bytes = if let Some(old_rows) = old_rows {
        std::mem::size_of::<Option<Vec<usize>>>()
            .checked_add(checked_allocation_bytes::<usize>(
                old_rows.capacity(),
                limits,
            )?)
            .ok_or_else(|| join_byte_limit_error(limits, usize::MAX))?
    } else {
        std::mem::size_of::<Option<Vec<usize>>>()
    };
    let keys_bytes = std::mem::size_of::<Vec<(usize, usize)>>()
        .checked_add(checked_allocation_bytes::<(usize, usize)>(
            keys.capacity(),
            limits,
        )?)
        .ok_or_else(|| join_byte_limit_error(limits, usize::MAX))?;
    old_rows_bytes
        .checked_add(keys_bytes)
        .ok_or_else(|| join_byte_limit_error(limits, usize::MAX))
}

fn checked_join_bytes(
    resident_bytes: usize,
    structure_bytes: usize,
    output_bytes: usize,
    limits: JoinLimits,
) -> Result<usize> {
    let actual = resident_bytes
        .checked_add(structure_bytes)
        .and_then(|bytes| bytes.checked_add(output_bytes))
        .ok_or_else(|| join_byte_limit_error(limits, usize::MAX))?;
    if actual > limits.max_bytes {
        Err(join_byte_limit_error(limits, actual))
    } else {
        Ok(actual)
    }
}

const ESTIMATED_ALLOCATION_OVERHEAD: usize = 2 * std::mem::size_of::<usize>();

fn checked_allocation_bytes<T>(capacity: usize, limits: JoinLimits) -> Result<usize> {
    if capacity == 0 {
        return Ok(0);
    }
    capacity
        .checked_mul(std::mem::size_of::<T>())
        .and_then(|bytes| bytes.checked_add(ESTIMATED_ALLOCATION_OVERHEAD))
        .ok_or_else(|| join_byte_limit_error(limits, usize::MAX))
}

fn join_byte_limit_error(limits: JoinLimits, actual: usize) -> Error {
    Error::JoinLimitExceeded {
        resource: "bytes",
        limit: limits.max_bytes,
        actual,
    }
}

fn append_join_match(
    joined: &mut Vec<usize>,
    old_rows: Option<&[usize]>,
    old_width: usize,
    left_row: usize,
    right_row: usize,
) {
    if let Some(old_rows) = old_rows {
        let start = left_row * old_width;
        joined.extend_from_slice(&old_rows[start..start + old_width]);
    } else {
        joined.push(left_row);
    }
    joined.push(right_row);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum JoinKeyPart<'a> {
    Int64(i64),
    Float64(u64),
    Bool(bool),
    String(&'a str),
}

impl<'a> JoinKeyPart<'a> {
    fn from_value(value: ValueRef<'a>) -> Option<Self> {
        Some(match value {
            ValueRef::Int64(value) => Self::Int64(value),
            ValueRef::Float64(value) if value.fract() == 0.0 => {
                const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
                if value >= i64::MIN as f64 && value < I64_UPPER_EXCLUSIVE {
                    Self::Int64(value as i64)
                } else {
                    Self::Float64(value.to_bits())
                }
            }
            ValueRef::Float64(value) => Self::Float64(value.to_bits()),
            ValueRef::Bool(value) => Self::Bool(value),
            ValueRef::String(value) => Self::String(value),
            ValueRef::Null => return None,
        })
    }
}

const NO_JOIN_INDEX: usize = usize::MAX;

#[derive(Debug, Clone, Copy)]
struct JoinHashLayout {
    bucket_count: usize,
    entry_capacity: usize,
    key_capacity: usize,
    row_capacity: usize,
    key_width: usize,
    estimated_bytes: usize,
}

impl JoinHashLayout {
    fn new(build_rows: usize, key_width: usize, limits: JoinLimits) -> Result<Self> {
        let bucket_count = build_rows
            .checked_next_power_of_two()
            .ok_or_else(|| join_byte_limit_error(limits, usize::MAX))?;
        let key_capacity = build_rows
            .checked_mul(key_width)
            .ok_or_else(|| join_byte_limit_error(limits, usize::MAX))?;
        let allocations = [
            checked_allocation_bytes::<usize>(bucket_count, limits)?,
            checked_allocation_bytes::<JoinHashEntry>(build_rows, limits)?,
            checked_allocation_bytes::<JoinKeyPart<'_>>(key_capacity, limits)?,
            checked_allocation_bytes::<usize>(build_rows, limits)?,
            checked_allocation_bytes::<JoinKeyPart<'_>>(key_width, limits)?,
        ];
        let mut estimated_bytes =
            std::mem::size_of::<JoinHashTable<'_>>() + std::mem::size_of::<Vec<JoinKeyPart<'_>>>();
        for allocation in allocations {
            estimated_bytes = estimated_bytes
                .checked_add(allocation)
                .ok_or_else(|| join_byte_limit_error(limits, usize::MAX))?;
        }
        Ok(Self {
            bucket_count,
            entry_capacity: build_rows,
            key_capacity,
            row_capacity: build_rows,
            key_width,
            estimated_bytes,
        })
    }
}

#[derive(Debug)]
struct JoinHashEntry {
    first_row: usize,
    last_row: usize,
    row_count: usize,
    next_entry: usize,
}

#[derive(Debug)]
struct JoinHashTable<'a> {
    buckets: Vec<usize>,
    entries: Vec<JoinHashEntry>,
    keys: Vec<JoinKeyPart<'a>>,
    row_next: Vec<usize>,
    key_width: usize,
}

impl<'a> JoinHashTable<'a> {
    fn new(layout: JoinHashLayout) -> Self {
        Self {
            buckets: vec![NO_JOIN_INDEX; layout.bucket_count],
            entries: Vec::with_capacity(layout.entry_capacity),
            keys: Vec::with_capacity(layout.key_capacity),
            row_next: vec![NO_JOIN_INDEX; layout.row_capacity],
            key_width: layout.key_width,
        }
    }

    fn insert(&mut self, key: &[JoinKeyPart<'a>], row: usize) {
        debug_assert_eq!(key.len(), self.key_width);
        debug_assert!(row < self.row_next.len());
        let bucket = self.bucket(key);
        let mut entry_index = self.buckets[bucket];
        while entry_index != NO_JOIN_INDEX {
            if self.entry_key(entry_index) == key {
                let last_row = self.entries[entry_index].last_row;
                self.row_next[last_row] = row;
                let entry = &mut self.entries[entry_index];
                entry.last_row = row;
                entry.row_count += 1;
                return;
            }
            entry_index = self.entries[entry_index].next_entry;
        }

        let entry_index = self.entries.len();
        self.keys.extend_from_slice(key);
        self.entries.push(JoinHashEntry {
            first_row: row,
            last_row: row,
            row_count: 1,
            next_entry: self.buckets[bucket],
        });
        self.buckets[bucket] = entry_index;
    }

    fn get(&self, key: &[JoinKeyPart<'_>]) -> Option<&JoinHashEntry> {
        debug_assert_eq!(key.len(), self.key_width);
        let mut entry_index = self.buckets[self.bucket(key)];
        while entry_index != NO_JOIN_INDEX {
            if self.entry_key(entry_index) == key {
                return Some(&self.entries[entry_index]);
            }
            entry_index = self.entries[entry_index].next_entry;
        }
        None
    }

    fn next_row(&self, row: usize) -> usize {
        self.row_next[row]
    }

    fn bucket(&self, key: &[JoinKeyPart<'_>]) -> usize {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        (hasher.finish() as usize) & (self.buckets.len() - 1)
    }

    fn entry_key(&self, entry: usize) -> &[JoinKeyPart<'a>] {
        let start = entry * self.key_width;
        &self.keys[start..start + self.key_width]
    }
}

#[derive(Debug)]
enum ResolvedItem {
    Column {
        source: usize,
        group_position: Option<usize>,
    },
    Aggregate {
        state: usize,
    },
}

#[derive(Debug, Clone)]
struct AggregateSpec {
    function: AggregateFunction,
    argument: Option<usize>,
    input_type: Option<DataType>,
    input_nullable: bool,
}

fn resolve_group_columns(input: &QueryInput<'_>, names: &[String]) -> Result<Vec<usize>> {
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        let column = input.column_index(name)?;
        if columns.contains(&column) {
            return Err(Error::InvalidQuery(format!(
                "GROUP BY column '{name}' is listed more than once"
            )));
        }
        columns.push(column);
    }
    Ok(columns)
}

fn resolve_select_items(
    input: &QueryInput<'_>,
    requested: &[SelectItem],
    group_columns: &[usize],
) -> Result<(Vec<ResolvedItem>, Vec<ResultColumn>, Vec<AggregateSpec>)> {
    let has_aggregate = requested
        .iter()
        .any(|item| matches!(item, SelectItem::Aggregate { .. }));
    if has_aggregate
        && requested.iter().any(|item| {
            matches!(
                item,
                SelectItem::Wildcard | SelectItem::QualifiedWildcard { .. }
            )
        })
    {
        return Err(Error::InvalidQuery(
            "'*' projection cannot be combined with aggregates".to_owned(),
        ));
    }

    let mut items = Vec::new();
    let mut result_columns = Vec::new();
    let mut aggregate_specs = Vec::new();

    for requested_item in requested {
        match requested_item {
            SelectItem::Wildcard => {
                for (source, field) in input.columns.iter().enumerate() {
                    let group_position = group_columns.iter().position(|column| *column == source);
                    if !group_columns.is_empty() && group_position.is_none() {
                        return Err(Error::InvalidQuery(format!(
                            "column '{}' must appear in GROUP BY",
                            field.name
                        )));
                    }
                    items.push(ResolvedItem::Column {
                        source,
                        group_position,
                    });
                    result_columns.push(ResultColumn {
                        name: field.name.to_owned(),
                        data_type: field.data_type,
                        nullable: field.nullable,
                    });
                }
            }
            SelectItem::QualifiedWildcard { qualifier } => {
                let relation = input.qualifier_index(qualifier)?;
                for (source, field) in input
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, field)| field.relation == relation)
                {
                    let group_position = group_columns.iter().position(|column| *column == source);
                    if !group_columns.is_empty() && group_position.is_none() {
                        return Err(Error::InvalidQuery(format!(
                            "column '{}.{}' must appear in GROUP BY",
                            qualifier, field.name
                        )));
                    }
                    items.push(ResolvedItem::Column {
                        source,
                        group_position,
                    });
                    result_columns.push(ResultColumn {
                        name: field.name.to_owned(),
                        data_type: field.data_type,
                        nullable: field.nullable,
                    });
                }
            }
            SelectItem::Column { name, alias } => {
                let source = input.column_index(name)?;
                let group_position = group_columns.iter().position(|column| *column == source);
                if (has_aggregate || !group_columns.is_empty()) && group_position.is_none() {
                    return Err(Error::InvalidQuery(format!(
                        "column '{name}' must appear in GROUP BY"
                    )));
                }
                items.push(ResolvedItem::Column {
                    source,
                    group_position,
                });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| input.columns[source].name.to_owned()),
                    data_type: input.columns[source].data_type,
                    nullable: input.columns[source].nullable,
                });
            }
            SelectItem::Aggregate {
                function,
                argument,
                alias,
            } => {
                let (argument_index, input_type, input_nullable, argument_name) = match argument {
                    AggregateArgument::Wildcard => {
                        if *function != AggregateFunction::Count {
                            return Err(Error::InvalidQuery(format!(
                                "{}(*) is not supported; use a column argument",
                                function.name()
                            )));
                        }
                        (None, None, false, "*".to_owned())
                    }
                    AggregateArgument::Column(name) => {
                        let index = input.column_index(name)?;
                        (
                            Some(index),
                            Some(input.columns[index].data_type),
                            input.columns[index].nullable,
                            input.columns[index].name.to_owned(),
                        )
                    }
                };
                validate_aggregate(*function, input_type)?;
                let state = aggregate_specs.len();
                aggregate_specs.push(AggregateSpec {
                    function: *function,
                    argument: argument_index,
                    input_type,
                    input_nullable,
                });
                items.push(ResolvedItem::Aggregate { state });
                result_columns.push(ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("{}({argument_name})", function.name())),
                    data_type: aggregate_output_type(*function, input_type),
                    nullable: *function != AggregateFunction::Count && input_nullable,
                });
            }
        }
    }

    Ok((items, result_columns, aggregate_specs))
}

fn validate_aggregate(function: AggregateFunction, input_type: Option<DataType>) -> Result<()> {
    if matches!(function, AggregateFunction::Sum | AggregateFunction::Avg)
        && !matches!(input_type, Some(DataType::Int64 | DataType::Float64))
    {
        let actual = input_type.map_or_else(|| "*".to_owned(), |value| value.to_string());
        return Err(Error::TypeMismatch {
            context: format!("{} argument", function.name()),
            expected: "Int64 or Float64".to_owned(),
            actual,
        });
    }
    Ok(())
}

fn aggregate_output_type(function: AggregateFunction, input_type: Option<DataType>) -> DataType {
    match function {
        AggregateFunction::Count => DataType::Int64,
        AggregateFunction::Avg => DataType::Float64,
        AggregateFunction::Sum | AggregateFunction::Min | AggregateFunction::Max => {
            input_type.expect("validated column argument")
        }
    }
}

fn execute_projection(
    input: &QueryInput<'_>,
    matching_rows: &[usize],
    items: &[ResolvedItem],
) -> Vec<Vec<Value>> {
    matching_rows
        .iter()
        .map(|row| {
            items
                .iter()
                .map(|item| match item {
                    ResolvedItem::Column { source, .. } => input.value(*source, *row).to_owned(),
                    ResolvedItem::Aggregate { .. } => {
                        unreachable!("projection does not contain aggregates")
                    }
                })
                .collect()
        })
        .collect()
}

fn execute_grouped<'a>(
    input: &'a QueryInput<'_>,
    matching_rows: &[usize],
    group_columns: &[usize],
    aggregate_specs: &[AggregateSpec],
) -> Result<GroupedData<'a>> {
    let mut groups = GroupIndex::new(group_columns.len(), matching_rows.len());
    let mut group_count = usize::from(group_columns.is_empty());
    let initial_capacity = matching_rows.len().min(1_024);
    let mut aggregate_states = aggregate_specs
        .iter()
        .map(|spec| {
            let mut states = Vec::with_capacity(initial_capacity);
            if group_columns.is_empty() {
                states.push(AggregateState::new(spec));
            }
            states
        })
        .collect::<Vec<_>>();

    for row in matching_rows {
        let (group, inserted) = groups.find_or_insert(input, group_columns, *row, group_count);
        if inserted {
            group_count += 1;
            for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
                states.push(AggregateState::new(spec));
            }
        }
        for (states, spec) in aggregate_states.iter_mut().zip(aggregate_specs) {
            debug_assert_eq!(states.len(), group_count);
            states[group].update(spec, input, *row)?;
        }
    }

    let keys = groups.into_keys(group_count);
    let aggregates = aggregate_states
        .into_iter()
        .zip(aggregate_specs)
        .map(|(states, spec)| {
            states
                .into_iter()
                .map(|state| state.finish(spec))
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(GroupedData { keys, aggregates })
}

#[derive(Debug)]
enum GroupIndex<'a> {
    Global,
    One(HashMap<ValueRef<'a>, usize>),
    Multiple(HashMap<Box<[ValueRef<'a>]>, usize>),
}

impl<'a> GroupIndex<'a> {
    fn new(column_count: usize, row_count: usize) -> Self {
        let initial_capacity = row_count.min(1_024);
        match column_count {
            0 => Self::Global,
            1 => Self::One(HashMap::with_capacity(initial_capacity)),
            _ => Self::Multiple(HashMap::with_capacity(initial_capacity)),
        }
    }

    fn find_or_insert(
        &mut self,
        input: &'a QueryInput<'_>,
        columns: &[usize],
        row: usize,
        next_group: usize,
    ) -> (usize, bool) {
        match self {
            Self::Global => (0, false),
            Self::One(groups) => {
                let key = input.value(columns[0], row);
                if let Some(group) = groups.get(&key) {
                    (*group, false)
                } else {
                    groups.insert(key, next_group);
                    (next_group, true)
                }
            }
            Self::Multiple(groups) if columns.len() == 2 => {
                let key = [input.value(columns[0], row), input.value(columns[1], row)];
                find_or_insert_group(groups, &key, next_group)
            }
            Self::Multiple(groups) => {
                let key = columns
                    .iter()
                    .map(|column| input.value(*column, row))
                    .collect::<Vec<_>>();
                find_or_insert_group(groups, &key, next_group)
            }
        }
    }

    fn into_keys(self, group_count: usize) -> Vec<GroupKey<'a>> {
        let mut ordered = std::iter::repeat_with(|| None)
            .take(group_count)
            .collect::<Vec<_>>();
        match self {
            Self::Global => {
                debug_assert_eq!(group_count, 1);
                ordered[0] = Some(GroupKey::Empty);
            }
            Self::One(groups) => {
                for (key, group) in groups {
                    ordered[group] = Some(GroupKey::One(key));
                }
            }
            Self::Multiple(groups) => {
                for (key, group) in groups {
                    ordered[group] = Some(GroupKey::Multiple(key));
                }
            }
        }
        ordered
            .into_iter()
            .map(|key| key.expect("every group index has a key"))
            .collect()
    }
}

fn find_or_insert_group<'a>(
    groups: &mut HashMap<Box<[ValueRef<'a>]>, usize>,
    key: &[ValueRef<'a>],
    next_group: usize,
) -> (usize, bool) {
    if let Some(group) = groups.get(key) {
        (*group, false)
    } else {
        groups.insert(key.into(), next_group);
        (next_group, true)
    }
}

#[derive(Debug)]
enum GroupKey<'a> {
    Empty,
    One(ValueRef<'a>),
    Multiple(Box<[ValueRef<'a>]>),
}

impl GroupKey<'_> {
    fn value(&self, position: usize) -> ValueRef<'_> {
        match self {
            Self::Empty => unreachable!("a global aggregate has no grouped columns"),
            Self::One(value) if position == 0 => *value,
            Self::One(_) => unreachable!("single-column group position is zero"),
            Self::Multiple(values) => values[position],
        }
    }

    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Empty, Self::Empty) => Ordering::Equal,
            (Self::One(left), Self::One(right)) => left.cmp(right),
            (Self::Multiple(left), Self::Multiple(right)) => left.cmp(right),
            _ => unreachable!("all keys for a query have the same shape"),
        }
    }
}

#[derive(Debug)]
struct GroupedData<'a> {
    keys: Vec<GroupKey<'a>>,
    aggregates: Vec<Vec<Value>>,
}

impl GroupedData<'_> {
    fn len(&self) -> usize {
        self.keys.len()
    }

    fn project(&self, selected: &[usize], items: &[ResolvedItem]) -> Vec<Vec<Value>> {
        selected
            .iter()
            .map(|group| {
                items
                    .iter()
                    .map(|item| match item {
                        ResolvedItem::Column {
                            group_position: Some(position),
                            ..
                        } => self.keys[*group].value(*position).to_owned(),
                        ResolvedItem::Column {
                            group_position: None,
                            ..
                        } => unreachable!("grouped columns are validated"),
                        ResolvedItem::Aggregate { state } => {
                            self.aggregates[*state][*group].clone()
                        }
                    })
                    .collect()
            })
            .collect()
    }
}

#[derive(Debug)]
enum AggregateState {
    Count(i64),
    SumInt { sum: i64, count: u64 },
    SumFloat { sum: f64, count: u64 },
    Min(Option<Value>),
    Max(Option<Value>),
    AvgInt { sum: i128, count: u64 },
    AvgFloat { sum: f64, count: u64 },
}

impl AggregateState {
    fn new(spec: &AggregateSpec) -> Self {
        match spec.function {
            AggregateFunction::Count => Self::Count(0),
            AggregateFunction::Sum if spec.input_type == Some(DataType::Int64) => {
                Self::SumInt { sum: 0, count: 0 }
            }
            AggregateFunction::Sum => Self::SumFloat { sum: 0.0, count: 0 },
            AggregateFunction::Min => Self::Min(None),
            AggregateFunction::Max => Self::Max(None),
            AggregateFunction::Avg if spec.input_type == Some(DataType::Int64) => {
                Self::AvgInt { sum: 0, count: 0 }
            }
            AggregateFunction::Avg => Self::AvgFloat { sum: 0.0, count: 0 },
        }
    }

    fn update(&mut self, spec: &AggregateSpec, input: &QueryInput<'_>, row: usize) -> Result<()> {
        match self {
            Self::Count(count) => {
                if spec
                    .argument
                    .is_some_and(|argument| input.value(argument, row) == ValueRef::Null)
                {
                    return Ok(());
                }
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt { sum, count } => {
                let value = match input.value(spec.argument.expect("SUM argument"), row) {
                    ValueRef::Int64(value) => value,
                    ValueRef::Null => return Ok(()),
                    _ => unreachable!("SUM input type is resolved"),
                };
                *sum = sum
                    .checked_add(value)
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
                *count += 1;
            }
            Self::SumFloat { sum, count } => {
                let value = match input.value(spec.argument.expect("SUM argument"), row) {
                    ValueRef::Float64(value) => value,
                    ValueRef::Null => return Ok(()),
                    _ => unreachable!("SUM input type is resolved"),
                };
                *sum += value;
                *count += 1;
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            Self::Min(current) => {
                let candidate = input.value(spec.argument.expect("MIN argument"), row);
                if candidate == ValueRef::Null {
                    return Ok(());
                }
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate < existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::Max(current) => {
                let candidate = input.value(spec.argument.expect("MAX argument"), row);
                if candidate == ValueRef::Null {
                    return Ok(());
                }
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate > existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::AvgInt { sum, count } => {
                let value = match input.value(spec.argument.expect("AVG argument"), row) {
                    ValueRef::Int64(value) => value,
                    ValueRef::Null => return Ok(()),
                    _ => unreachable!("AVG input type is resolved"),
                };
                *sum = sum
                    .checked_add(i128::from(value))
                    .ok_or_else(|| Error::NumericOverflow("AVG(Int64) sum".to_owned()))?;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
            Self::AvgFloat { sum, count } => {
                let value = match input.value(spec.argument.expect("AVG argument"), row) {
                    ValueRef::Float64(value) => value,
                    ValueRef::Null => return Ok(()),
                    _ => unreachable!("AVG input type is resolved"),
                };
                *sum += value;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("AVG(Float64) sum".to_owned()));
                }
            }
        }
        Ok(())
    }

    fn finish(self, spec: &AggregateSpec) -> Result<Value> {
        match self {
            Self::Count(value) => Ok(Value::Int64(value)),
            Self::SumInt { sum, count } if count > 0 || !spec.input_nullable => {
                Ok(Value::Int64(sum))
            }
            Self::SumFloat { sum, count } if count > 0 || !spec.input_nullable => {
                Ok(Value::Float64(sum))
            }
            Self::Min(Some(value)) | Self::Max(Some(value)) => Ok(value),
            Self::AvgInt { sum, count } if count > 0 => {
                Ok(Value::Float64(sum as f64 / count as f64))
            }
            Self::AvgFloat { sum, count } if count > 0 => Ok(Value::Float64(sum / count as f64)),
            Self::SumInt { .. }
            | Self::SumFloat { .. }
            | Self::Min(None)
            | Self::Max(None)
            | Self::AvgInt { .. }
            | Self::AvgFloat { .. }
                if spec.input_nullable =>
            {
                Ok(Value::Null)
            }
            Self::Min(None) => Err(Error::InvalidQuery(
                "MIN is undefined for an empty input".to_owned(),
            )),
            Self::Max(None) => Err(Error::InvalidQuery(
                "MAX is undefined for an empty input".to_owned(),
            )),
            Self::AvgInt { .. } | Self::AvgFloat { .. } => Err(Error::InvalidQuery(
                "AVG is undefined for an empty input".to_owned(),
            )),
            Self::SumInt { .. } | Self::SumFloat { .. } => {
                unreachable!("non-nullable SUM always returns its zero state")
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ResolvedOrder {
    output: usize,
    descending: bool,
}

fn resolve_ordering(
    input: &QueryInput<'_>,
    items: &[ResolvedItem],
    columns: &[ResultColumn],
    requested: &[OrderBy],
) -> Result<Vec<ResolvedOrder>> {
    let mut ordering = Vec::with_capacity(requested.len());
    for order in requested {
        let matches = if order.name.contains('.') {
            let source = input.column_index(&order.name)?;
            items
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    matches!(item, ResolvedItem::Column { source: item_source, .. } if *item_source == source)
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        } else {
            columns
                .iter()
                .enumerate()
                .filter(|(_, column)| column.name.eq_ignore_ascii_case(&order.name))
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        };
        match matches.as_slice() {
            [index] => ordering.push(ResolvedOrder {
                output: *index,
                descending: order.descending,
            }),
            [] => {
                return Err(Error::InvalidQuery(format!(
                    "ORDER BY column or alias '{}' is not in the SELECT output",
                    order.name
                )));
            }
            _ => {
                return Err(Error::InvalidQuery(format!(
                    "ORDER BY name '{}' is ambiguous",
                    order.name
                )));
            }
        }
    }
    Ok(ordering)
}

fn order_source_rows(
    rows: &mut Vec<usize>,
    input: &QueryInput<'_>,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
) {
    if ordering.is_empty() {
        if let Some(limit) = limit {
            rows.truncate(limit);
        }
        return;
    }

    sort_and_limit(rows, limit, |left, right| {
        for order in ordering {
            let ResolvedItem::Column { source, .. } = items[order.output] else {
                unreachable!("ungrouped projections cannot contain aggregates")
            };
            let comparison = input.cmp_at(source, left, right);
            if comparison != Ordering::Equal {
                return if order.descending {
                    comparison.reverse()
                } else {
                    comparison
                };
            }
        }
        left.cmp(&right)
    });
}

fn order_grouped_rows(
    groups: &mut Vec<usize>,
    data: &GroupedData<'_>,
    items: &[ResolvedItem],
    ordering: &[ResolvedOrder],
    limit: Option<usize>,
) {
    sort_and_limit(groups, limit, |left, right| {
        for order in ordering {
            let comparison = match items[order.output] {
                ResolvedItem::Column {
                    group_position: Some(position),
                    ..
                } => data.keys[left]
                    .value(position)
                    .cmp(&data.keys[right].value(position)),
                ResolvedItem::Column {
                    group_position: None,
                    ..
                } => unreachable!("grouped columns are validated"),
                ResolvedItem::Aggregate { state } => {
                    data.aggregates[state][left].cmp(&data.aggregates[state][right])
                }
            };
            if comparison != Ordering::Equal {
                return if order.descending {
                    comparison.reverse()
                } else {
                    comparison
                };
            }
        }
        data.keys[left].cmp(&data.keys[right])
    });
}

fn sort_and_limit(
    indices: &mut Vec<usize>,
    limit: Option<usize>,
    compare: impl Fn(usize, usize) -> Ordering,
) {
    if let Some(0) = limit {
        indices.clear();
        return;
    }
    if let Some(limit) = limit.filter(|limit| *limit < indices.len()) {
        indices.select_nth_unstable_by(limit, |left, right| compare(*left, *right));
        indices.truncate(limit);
    }
    indices.sort_unstable_by(|left, right| compare(*left, *right));
}

#[derive(Debug)]
enum CompiledPredicate {
    Comparison {
        left: CompiledOperand,
        operator: ComparisonOperator,
        right: CompiledOperand,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl CompiledPredicate {
    fn evaluate(&self, input: &QueryInput<'_>, row: usize) -> bool {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.value(input, row);
                let right = right.value(input, row);
                let Some(comparison) = left.sql_cmp(right) else {
                    return false;
                };
                match operator {
                    ComparisonOperator::Equal => comparison == Ordering::Equal,
                    ComparisonOperator::NotEqual => comparison != Ordering::Equal,
                    ComparisonOperator::Less => comparison == Ordering::Less,
                    ComparisonOperator::LessOrEqual => comparison != Ordering::Greater,
                    ComparisonOperator::Greater => comparison == Ordering::Greater,
                    ComparisonOperator::GreaterOrEqual => comparison != Ordering::Less,
                }
            }
            Self::And(left, right) => left.evaluate(input, row) && right.evaluate(input, row),
            Self::Or(left, right) => left.evaluate(input, row) || right.evaluate(input, row),
        }
    }

    fn evaluate_join(
        &self,
        input: &QueryInput<'_>,
        old_rows: Option<&[usize]>,
        old_width: usize,
        left_row: usize,
        right_relation: usize,
        right_row: usize,
    ) -> bool {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let left = left.join_value(
                    input,
                    old_rows,
                    old_width,
                    left_row,
                    right_relation,
                    right_row,
                );
                let right = right.join_value(
                    input,
                    old_rows,
                    old_width,
                    left_row,
                    right_relation,
                    right_row,
                );
                let Some(comparison) = left.sql_cmp(right) else {
                    return false;
                };
                match operator {
                    ComparisonOperator::Equal => comparison == Ordering::Equal,
                    ComparisonOperator::NotEqual => comparison != Ordering::Equal,
                    ComparisonOperator::Less => comparison == Ordering::Less,
                    ComparisonOperator::LessOrEqual => comparison != Ordering::Greater,
                    ComparisonOperator::Greater => comparison == Ordering::Greater,
                    ComparisonOperator::GreaterOrEqual => comparison != Ordering::Less,
                }
            }
            Self::And(left, right) => {
                left.evaluate_join(
                    input,
                    old_rows,
                    old_width,
                    left_row,
                    right_relation,
                    right_row,
                ) && right.evaluate_join(
                    input,
                    old_rows,
                    old_width,
                    left_row,
                    right_relation,
                    right_row,
                )
            }
            Self::Or(left, right) => {
                left.evaluate_join(
                    input,
                    old_rows,
                    old_width,
                    left_row,
                    right_relation,
                    right_row,
                ) || right.evaluate_join(
                    input,
                    old_rows,
                    old_width,
                    left_row,
                    right_relation,
                    right_row,
                )
            }
        }
    }
}

#[derive(Debug)]
enum CompiledOperand {
    Column { index: usize, data_type: DataType },
    Literal(Value),
}

impl CompiledOperand {
    fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Column { data_type, .. } => Some(*data_type),
            Self::Literal(value) => value.data_type(),
        }
    }

    fn value<'a>(&'a self, input: &'a QueryInput<'_>, row: usize) -> ValueRef<'a> {
        match self {
            Self::Column { index, .. } => input.value(*index, row),
            Self::Literal(value) => value.as_ref(),
        }
    }

    fn join_value<'a>(
        &'a self,
        input: &'a QueryInput<'_>,
        old_rows: Option<&[usize]>,
        old_width: usize,
        left_row: usize,
        right_relation: usize,
        right_row: usize,
    ) -> ValueRef<'a> {
        match self {
            Self::Column { index, .. } if input.columns[*index].relation == right_relation => {
                input.right_value(*index, right_row)
            }
            Self::Column { index, .. } => input.old_value(old_rows, old_width, *index, left_row),
            Self::Literal(value) => value.as_ref(),
        }
    }
}

fn compile_predicate(input: &QueryInput<'_>, predicate: &Predicate) -> Result<CompiledPredicate> {
    compile_predicate_for(input, predicate, "WHERE")
}

fn compile_predicate_for(
    input: &QueryInput<'_>,
    predicate: &Predicate,
    context: &str,
) -> Result<CompiledPredicate> {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let left = compile_operand(input, left)?;
            let right = compile_operand(input, right)?;
            if let (Some(left_type), Some(right_type)) = (left.data_type(), right.data_type())
                && !comparable(left_type, right_type)
            {
                return Err(Error::TypeMismatch {
                    context: format!("{context} comparison"),
                    expected: left_type.to_string(),
                    actual: right_type.to_string(),
                });
            }
            Ok(CompiledPredicate::Comparison {
                left,
                operator: *operator,
                right,
            })
        }
        Predicate::And(left, right) => Ok(CompiledPredicate::And(
            Box::new(compile_predicate_for(input, left, context)?),
            Box::new(compile_predicate_for(input, right, context)?),
        )),
        Predicate::Or(left, right) => Ok(CompiledPredicate::Or(
            Box::new(compile_predicate_for(input, left, context)?),
            Box::new(compile_predicate_for(input, right, context)?),
        )),
    }
}

fn compile_operand(input: &QueryInput<'_>, operand: &Operand) -> Result<CompiledOperand> {
    match operand {
        Operand::Column(name) => {
            let index = input.column_index(name)?;
            Ok(CompiledOperand::Column {
                index,
                data_type: input.columns[index].data_type,
            })
        }
        Operand::Literal(value) => Ok(CompiledOperand::Literal(value.clone())),
    }
}

fn comparable(left: DataType, right: DataType) -> bool {
    left == right
        || matches!(
            (left, right),
            (DataType::Int64, DataType::Float64) | (DataType::Float64, DataType::Int64)
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(database: &mut Database, sql: &str) -> QueryResult {
        let results = database.execute(sql).expect("query succeeds");
        match results.into_iter().last().expect("one result") {
            StatementResult::Query(result) => result,
            StatementResult::Command { .. } => panic!("expected query result"),
        }
    }

    #[test]
    fn aggregates_groups_and_orders() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE sales (region String, amount Int64); \
                 INSERT INTO sales VALUES ('west', 10), ('east', 4), ('west', 7);",
            )
            .expect("setup");

        let result = query(
            &mut database,
            "SELECT region, COUNT(*) AS n, SUM(amount) AS total, AVG(amount) AS mean \
             FROM sales GROUP BY region ORDER BY total DESC",
        );
        assert_eq!(
            result.rows,
            vec![
                vec![
                    Value::String("west".to_owned()),
                    Value::Int64(2),
                    Value::Int64(17),
                    Value::Float64(8.5),
                ],
                vec![
                    Value::String("east".to_owned()),
                    Value::Int64(1),
                    Value::Int64(4),
                    Value::Float64(4.0),
                ],
            ]
        );
    }

    #[test]
    fn filters_with_boolean_precedence() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE valueset (id Int64, enabled Bool); \
                 INSERT INTO valueset VALUES (1, false), (2, true), (3, false);",
            )
            .expect("setup");
        let result = query(
            &mut database,
            "SELECT id FROM valueset WHERE id = 1 OR id >= 2 AND enabled = true",
        );
        assert_eq!(
            result.rows,
            vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
        );
    }
}
