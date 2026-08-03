//! Full-sort and bounded top-k row ordering.

use std::cmp::Ordering;

use crate::scan::RowSelection;
use crate::sql::{OrderByClause, OrderDirection};
use crate::storage::{Column, Table};

use super::CatalogError;

pub(super) fn resolve_order(
    table: &Table,
    order_by: Vec<OrderByClause>,
) -> Result<Vec<(usize, OrderDirection)>, CatalogError> {
    order_by
        .into_iter()
        .map(|order_key| {
            table
                .fields()
                .iter()
                .position(|field| field.name() == order_key.column)
                .map(|index| (index, order_key.direction))
                .ok_or(CatalogError::OrderFieldNotFound {
                    name: order_key.column,
                })
        })
        .collect()
}

pub(super) fn ordered_row_indices(
    table: &Table,
    order_keys: &[(usize, OrderDirection)],
    selection: Option<&RowSelection>,
    limit: Option<usize>,
) -> Result<Vec<usize>, CatalogError> {
    match limit {
        Some(limit) => bounded_ordered_row_indices(table, order_keys, selection, limit),
        None => fully_ordered_row_indices(table, order_keys, selection),
    }
}

fn fully_ordered_row_indices(
    table: &Table,
    order_keys: &[(usize, OrderDirection)],
    selection: Option<&RowSelection>,
) -> Result<Vec<usize>, CatalogError> {
    let row_count = selection.map_or(table.len(), RowSelection::selected_count);
    let mut rows = try_order_row_buffer(row_count)?;
    match selection {
        Some(selection) => rows.extend(selection.selected_rows()),
        None => rows.extend(0..table.len()),
    }

    let order = RowOrder::new(table, order_keys);
    rows.sort_unstable_by(|left, right| order.compare(*left, *right));
    Ok(rows)
}

fn bounded_ordered_row_indices(
    table: &Table,
    order_keys: &[(usize, OrderDirection)],
    selection: Option<&RowSelection>,
    limit: usize,
) -> Result<Vec<usize>, CatalogError> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let row_count = selection.map_or(table.len(), RowSelection::selected_count);
    let retained_count = row_count.min(limit);
    let mut rows = try_order_row_buffer(retained_count)?;
    let order = RowOrder::new(table, order_keys);

    match selection {
        Some(selection) => {
            for row in selection.selected_rows() {
                retain_top_row(&mut rows, row, retained_count, order);
            }
        }
        None => {
            for row in 0..table.len() {
                retain_top_row(&mut rows, row, retained_count, order);
            }
        }
    }

    rows.sort_unstable_by(|left, right| order.compare(*left, *right));
    Ok(rows)
}

fn try_order_row_buffer(row_count: usize) -> Result<Vec<usize>, CatalogError> {
    let mut rows = Vec::new();
    rows.try_reserve_exact(row_count)
        .map_err(|_| CatalogError::OrderAllocationFailed { row_count })?;
    Ok(rows)
}

#[derive(Clone, Copy)]
struct RowOrder<'a> {
    table: &'a Table,
    order_keys: &'a [(usize, OrderDirection)],
}

impl<'a> RowOrder<'a> {
    const fn new(table: &'a Table, order_keys: &'a [(usize, OrderDirection)]) -> Self {
        Self { table, order_keys }
    }

    fn compare(self, left: usize, right: usize) -> Ordering {
        self.order_keys
            .iter()
            .map(|(column_index, direction)| {
                let value_order =
                    compare_column_rows(&self.table.columns()[*column_index], left, right);
                match direction {
                    OrderDirection::Ascending => value_order,
                    OrderDirection::Descending => value_order.reverse(),
                }
            })
            .find(|order| *order != Ordering::Equal)
            .unwrap_or_else(|| left.cmp(&right))
    }
}

fn retain_top_row(rows: &mut Vec<usize>, row: usize, limit: usize, order: RowOrder<'_>) {
    // This max-heap keeps the worst retained row at its root for replacement.
    if rows.len() < limit {
        rows.push(row);
        sift_order_heap_up(rows, order);
    } else if order.compare(row, rows[0]).is_lt() {
        rows[0] = row;
        sift_order_heap_down(rows, order);
    }
}

fn sift_order_heap_up(rows: &mut [usize], order: RowOrder<'_>) {
    let mut child = rows.len() - 1;
    while child > 0 {
        let parent = (child - 1) / 2;
        if !order.compare(rows[parent], rows[child]).is_lt() {
            break;
        }
        rows.swap(parent, child);
        child = parent;
    }
}

fn sift_order_heap_down(rows: &mut [usize], order: RowOrder<'_>) {
    let mut parent = 0;
    loop {
        let left = parent * 2 + 1;
        if left >= rows.len() {
            break;
        }
        let right = left + 1;
        let greater_child = if right < rows.len() && order.compare(rows[left], rows[right]).is_lt()
        {
            right
        } else {
            left
        };
        if !order.compare(rows[parent], rows[greater_child]).is_lt() {
            break;
        }
        rows.swap(parent, greater_child);
        parent = greater_child;
    }
}

fn compare_column_rows(column: &Column, left: usize, right: usize) -> Ordering {
    match column {
        Column::Int64(values) => values[left].cmp(&values[right]),
        Column::Float64(values) => values[left].total_cmp(&values[right]),
        Column::Bool(values) => values[left].cmp(&values[right]),
        Column::String(values) => values[left].cmp(&values[right]),
    }
}

#[cfg(test)]
mod ordered_row_tests {
    use super::*;
    use crate::{DataType, Field, Value};

    #[test]
    fn zero_and_empty_bounded_orders_have_no_row_buffer() {
        let mut table = Table::new(vec![Field::new("id", DataType::Int64)]).unwrap();
        table
            .insert_batch((0..4).map(|id| vec![Value::Int64(id)]))
            .unwrap();
        let order_keys = [(0, OrderDirection::Ascending)];

        let zero = bounded_ordered_row_indices(&table, &order_keys, None, 0).unwrap();
        assert!(zero.is_empty());
        assert_eq!(zero.capacity(), 0);

        let empty = Table::new(vec![Field::new("id", DataType::Int64)]).unwrap();
        let empty_rows = bounded_ordered_row_indices(&empty, &order_keys, None, 25).unwrap();
        assert!(empty_rows.is_empty());
        assert_eq!(empty_rows.capacity(), 0);
    }

    #[test]
    fn order_row_buffer_reports_capacity_overflow() {
        assert_eq!(
            try_order_row_buffer(usize::MAX),
            Err(CatalogError::OrderAllocationFailed {
                row_count: usize::MAX,
            })
        );
    }
}
