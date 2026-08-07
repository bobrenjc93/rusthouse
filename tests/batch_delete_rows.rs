use rusthouse::TableLimits;
use rusthouse::batch::error::Error;
use rusthouse::batch::storage::{Column, ColumnDef, Table};
use rusthouse::batch::value::{DataType, Value};

fn four_type_table(limits: TableLimits) -> Table {
    Table::with_limits(
        "events".to_owned(),
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
                name: "active".to_owned(),
                data_type: DataType::Bool,
            },
            ColumnDef {
                name: "label".to_owned(),
                data_type: DataType::String,
            },
        ],
        limits,
    )
    .expect("valid four-type table")
}

fn row(id: i64) -> Vec<Value> {
    vec![
        Value::Int64(id),
        Value::Float64(id as f64 + 0.25),
        Value::Bool(id % 2 == 0),
        Value::String(format!("row-{id}")),
    ]
}

fn assert_rows(table: &Table, ids: &[i64]) {
    assert_eq!(table.row_count(), ids.len());
    assert!(matches!(&table.columns()[0], Column::Int64(values) if values == &ids.to_vec()));
    assert!(matches!(&table.columns()[1], Column::Float64(values)
        if values == &ids.iter().map(|id| *id as f64 + 0.25).collect::<Vec<_>>()));
    assert!(matches!(&table.columns()[2], Column::Bool(values)
        if values == &ids.iter().map(|id| id % 2 == 0).collect::<Vec<_>>()));
    assert!(matches!(&table.columns()[3], Column::String(values)
        if values == &ids.iter().map(|id| format!("row-{id}")).collect::<Vec<_>>()));
}

#[test]
fn empty_and_sparse_deletions_preserve_every_physical_type_and_metadata() {
    let limits = TableLimits::new(8, 4, 32);
    let mut table = four_type_table(limits);

    assert_eq!(table.delete_rows(&[]), Ok(0));
    assert!(table.columns().iter().all(Column::is_empty));

    table
        .insert_rows((0..6).map(row).collect())
        .expect("rows fit");
    let schema = table.schema().to_vec();

    assert_eq!(table.delete_rows(&[]), Ok(0));
    assert_rows(&table, &[0, 1, 2, 3, 4, 5]);

    assert_eq!(table.delete_rows(&[0, 2, 5]), Ok(3));
    assert_rows(&table, &[1, 3, 4]);
    assert_eq!(table.name(), "events");
    assert_eq!(table.schema(), schema);
    assert_eq!(table.limits(), limits);
    assert_eq!(table.retained_cell_count(), 12);
}

#[test]
fn complete_deletion_empties_every_physical_column() {
    let mut table = four_type_table(TableLimits::new(3, 4, 12));
    table
        .insert_rows((0..3).map(row).collect())
        .expect("rows fit");

    assert_eq!(table.delete_rows(&[0, 1, 2]), Ok(3));
    assert_eq!(table.row_count(), 0);
    assert_eq!(table.retained_cell_count(), 0);
    assert!(table.columns().iter().all(Column::is_empty));
    assert_eq!(table.schema().len(), 4);
    assert_eq!(table.limits(), TableLimits::new(3, 4, 12));
}

#[test]
fn invalid_selections_are_typed_and_completely_atomic() {
    let mut table = four_type_table(TableLimits::new(4, 4, 16));
    table
        .insert_rows((0..4).map(row).collect())
        .expect("rows fit");

    assert_eq!(
        table.delete_rows(&[1, 4]),
        Err(Error::SelectionIndexOutOfBounds {
            selection_position: 1,
            row_index: 4,
            input_rows: 4,
        })
    );
    assert_rows(&table, &[0, 1, 2, 3]);

    assert_eq!(
        table.delete_rows(&[1, 1]),
        Err(Error::SelectionNotStrictlyIncreasing {
            selection_position: 1,
            previous_row_index: 1,
            row_index: 1,
        })
    );
    assert_rows(&table, &[0, 1, 2, 3]);

    assert_eq!(
        table.delete_rows(&[2, 1]),
        Err(Error::SelectionNotStrictlyIncreasing {
            selection_position: 1,
            previous_row_index: 2,
            row_index: 1,
        })
    );
    assert_rows(&table, &[0, 1, 2, 3]);
}

#[test]
fn deleted_rows_release_both_row_and_cell_capacity_for_reuse() {
    let limits = TableLimits::new(3, 4, 12);
    let mut table = four_type_table(limits);
    table
        .insert_rows((0..3).map(row).collect())
        .expect("both capacity limits are full");
    assert_eq!(
        table.insert_row(row(3)),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 4,
            max: 3,
        })
    );

    assert_eq!(table.delete_rows(&[0, 2]), Ok(2));
    table
        .insert_rows(vec![row(3), row(4)])
        .expect("deleted rows release row and cell capacity");
    assert_rows(&table, &[1, 3, 4]);
    assert_eq!(table.retained_cell_count(), limits.max_cells);
}

#[test]
fn deletion_releases_cell_capacity_when_the_row_cap_is_not_binding() {
    let limits = TableLimits::new(10, 4, 8);
    let mut table = four_type_table(limits);
    table
        .insert_rows(vec![row(0), row(1)])
        .expect("cell capacity is full");
    assert_eq!(
        table.insert_row(row(2)),
        Err(Error::ResourceLimitExceeded {
            resource: "table cells",
            actual: 12,
            max: 8,
        })
    );

    assert_eq!(table.delete_rows(&[1]), Ok(1));
    table
        .insert_row(row(2))
        .expect("deleted cells are reusable independently of row capacity");
    assert_rows(&table, &[0, 2]);
}
