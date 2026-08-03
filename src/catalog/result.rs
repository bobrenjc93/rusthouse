//! Ownership and iteration for SELECT result shapes.

use crate::grouping::GroupedCount;
use crate::scan::RowSelection;
use crate::storage::{Column, Field, Table, Value};

#[derive(Debug)]
pub(super) struct ScalarResult {
    pub(super) field: Field,
    pub(super) value: Value,
}

#[derive(Debug)]
pub(super) struct GroupedResult {
    pub(super) fields: [Field; 2],
    pub(super) groups: Vec<GroupedCount>,
}

/// The output of one projection, scalar aggregate, or grouped count and
/// optional comparison scans.
///
/// A row projection owns only projected column indexes and, for a filtered
/// query, a compact row-selection bitmap. Ordered results instead own their
/// bounded row indexes. Schema and column values remain owned by the catalog's
/// source table. Scalar aggregates own their fields and single result row;
/// grouped counts own their output fields and sorted key/count rows.
#[derive(Debug)]
pub struct SelectResult<'a> {
    pub(super) table: &'a Table,
    pub(super) field_indices: Vec<usize>,
    pub(super) selection: Option<RowSelection>,
    pub(super) ordered_rows: Option<Vec<usize>>,
    pub(super) row_end: usize,
    pub(super) row_count: usize,
    pub(super) scalars: Vec<ScalarResult>,
    pub(super) grouped: Option<GroupedResult>,
}

impl<'a> SelectResult<'a> {
    /// Returns the table whose schema and column values back this result.
    #[must_use]
    pub const fn table(&self) -> &'a Table {
        self.table
    }

    /// Iterates over projected fields in statement order.
    pub fn projected_fields(
        &self,
    ) -> impl ExactSizeIterator<Item = &Field> + DoubleEndedIterator + '_ {
        let projected_count = self.field_indices.len();
        let grouped_fields = self
            .grouped
            .as_ref()
            .map_or(&[][..], |grouped| grouped.fields.as_slice());
        let scalar_count = self.scalars.len();
        (0..projected_count + scalar_count + grouped_fields.len()).map(move |index| {
            if index < projected_count {
                &self.table.fields()[self.field_indices[index]]
            } else if index < projected_count + scalar_count {
                &self.scalars[index - projected_count].field
            } else {
                &grouped_fields[index - projected_count - scalar_count]
            }
        })
    }

    /// Alias for [`Self::projected_fields`].
    pub fn fields(&self) -> impl ExactSizeIterator<Item = &Field> + DoubleEndedIterator + '_ {
        self.projected_fields()
    }

    pub(crate) fn projected_columns(
        &self,
    ) -> impl ExactSizeIterator<Item = &Column> + DoubleEndedIterator + '_ {
        self.field_indices
            .iter()
            .map(|index| &self.table.columns()[*index])
    }

    /// Iterates over selected zero-based indexes in result order.
    ///
    /// Projection row indexes are also source table indexes. A scalar
    /// aggregate has exactly one result row at index zero. Grouped result
    /// indexes address the owned, deterministically ordered rows.
    pub fn selected_rows(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        SelectedRows::new(self)
    }

    /// Iterates over the values in a scalar aggregate result row.
    ///
    /// Row projections and scalar rows suppressed by `LIMIT 0` are empty.
    pub fn scalar_values(
        &self,
    ) -> impl ExactSizeIterator<Item = &Value> + DoubleEndedIterator + '_ {
        let scalars = if self.row_count == 0 {
            &self.scalars[..0]
        } else {
            &self.scalars[..]
        };
        scalars.iter().map(|scalar| &scalar.value)
    }

    /// Returns the first value for a scalar aggregate result.
    ///
    /// Row projections and scalar rows suppressed by `LIMIT 0` return `None`.
    #[must_use]
    pub const fn scalar_value(&self) -> Option<&Value> {
        match (self.scalars.as_slice(), self.row_count) {
            (_, 0) => None,
            ([scalar, ..], _) => Some(&scalar.value),
            ([], _) => None,
        }
    }

    pub(crate) fn is_scalar(&self) -> bool {
        !self.scalars.is_empty()
    }

    pub(crate) fn is_grouped(&self) -> bool {
        self.grouped.is_some()
    }

    /// Iterates over owned key/count rows for a grouped count result.
    ///
    /// Other result shapes return an empty iterator. Counts have already been
    /// validated as representable by the SQL `Int64` result type.
    pub fn grouped_rows(
        &self,
    ) -> impl ExactSizeIterator<Item = (&Value, i64)> + DoubleEndedIterator + '_ {
        let groups = self
            .grouped
            .as_ref()
            .map_or(&[][..], |grouped| grouped.groups.as_slice());
        groups.iter().map(|group| {
            let count = i64::try_from(group.count())
                .expect("group counts are validated before SelectResult construction");
            (group.value(), count)
        })
    }

    /// Alias for [`Self::selected_rows`].
    pub fn row_indices(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        self.selected_rows()
    }

    /// Returns the number of output rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.row_count
    }

    /// Returns whether the result has no output rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct SelectedRows<'a> {
    source: SelectedRowSource<'a>,
}

enum SelectedRowSource<'a> {
    Ordered(std::slice::Iter<'a, usize>),
    Natural {
        rows: std::ops::Range<usize>,
        selection: Option<&'a RowSelection>,
    },
}

impl<'a> SelectedRows<'a> {
    fn new(result: &'a SelectResult<'_>) -> Self {
        let source = match &result.ordered_rows {
            Some(rows) => SelectedRowSource::Ordered(rows.iter()),
            None => SelectedRowSource::Natural {
                rows: 0..result.row_end,
                selection: result.selection.as_ref(),
            },
        };
        Self { source }
    }
}

impl Iterator for SelectedRows<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.source {
            SelectedRowSource::Ordered(rows) => rows.next().copied(),
            SelectedRowSource::Natural { rows, selection } => rows.find(|row| {
                selection.is_none_or(|selection| {
                    selection
                        .get(*row)
                        .expect("selection and source table have the same row count")
                })
            }),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.source {
            SelectedRowSource::Ordered(rows) => rows.size_hint(),
            SelectedRowSource::Natural { rows, selection } => match selection {
                None => rows.size_hint(),
                Some(_) => (0, Some(rows.len())),
            },
        }
    }
}

impl DoubleEndedIterator for SelectedRows<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match &mut self.source {
            SelectedRowSource::Ordered(rows) => rows.next_back().copied(),
            SelectedRowSource::Natural { rows, selection } => rows.rfind(|row| {
                selection.is_none_or(|selection| {
                    selection
                        .get(*row)
                        .expect("selection and source table have the same row count")
                })
            }),
        }
    }
}
