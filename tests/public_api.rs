use rusthouse::catalog::Catalog;
use rusthouse::sql::{
    AggregateArgument, AggregateFunction, ComparisonOperator, Operand, Predicate, Select,
    SelectItem, Statement, parse,
};
use rusthouse::storage::{Column, ColumnDef, Table};
use rusthouse::{DataType, Error, Value};

#[test]
fn legacy_public_value_statement_and_column_shapes_remain_usable() {
    assert_eq!(Value::Int64(42).data_type(), DataType::Int64);

    let columns = [
        Column::Int64(vec![42]),
        Column::Float64(vec![2.5]),
        Column::Bool(vec![true]),
        Column::String(vec!["ok".to_owned()]),
    ];
    assert_eq!(columns[0].value(0), Value::Int64(42));
    assert_eq!(columns[1].value(0), Value::Float64(2.5));
    assert_eq!(columns[2].value(0), Value::Bool(true));
    assert_eq!(columns[3].value(0), Value::String("ok".to_owned()));

    let Statement::Select(select) = parse("SELECT id FROM events")
        .expect("valid SELECT")
        .remove(0)
    else {
        panic!("expected SELECT");
    };
    let _: Select = select;
}

#[test]
fn nullable_column_validity_cannot_diverge_from_values() {
    let mut table = Table::new(
        "events".to_owned(),
        vec![ColumnDef {
            name: "id".to_owned(),
            data_type: DataType::Int64,
        }],
    )
    .expect("valid table");

    table.insert_row(vec![Value::Int64(1)]).expect("row");
    table.insert_row(vec![Value::Null]).expect("NULL row");
    table.insert_row(vec![Value::Int64(3)]).expect("row");

    let column = &table.columns()[0];
    assert_eq!(column.len(), 3);
    assert_eq!(column.value(0), Value::Int64(1));
    assert_eq!(column.value(1), Value::Null);
    assert_eq!(column.value(2), Value::Int64(3));
}

#[test]
fn versioned_public_api_exposes_filtered_aggregate_and_having_ast() {
    assert_eq!(env!("CARGO_PKG_VERSION"), "0.2.0");

    let filter = Predicate::Comparison {
        left: Operand::Column("qualified".to_owned()),
        operator: ComparisonOperator::Equal,
        right: Operand::Literal(Value::Bool(true)),
    };
    let select = Select {
        items: vec![SelectItem::Aggregate {
            function: AggregateFunction::Count,
            argument: AggregateArgument::Wildcard,
            filter: Some(filter.clone()),
            alias: Some("qualified_count".to_owned()),
        }],
        table: "events".to_owned(),
        predicate: None,
        group_by: Vec::new(),
        having: Some(Predicate::Comparison {
            left: Operand::Aggregate {
                function: AggregateFunction::Count,
                argument: AggregateArgument::Wildcard,
                filter: Some(Box::new(filter)),
            },
            operator: ComparisonOperator::Greater,
            right: Operand::Literal(Value::Int64(0)),
        }),
        order_by: Vec::new(),
        limit: None,
    };

    assert!(matches!(Statement::Select(select), Statement::Select(_)));
    assert_eq!(Value::Null.data_type(), DataType::Null);
    assert_eq!(Column::Null(vec![()]).value(0), Value::Null);
}

#[test]
fn null_type_marker_is_rejected_in_public_schemas() {
    let schema = vec![ColumnDef {
        name: "value".to_owned(),
        data_type: DataType::Null,
    }];

    let table_error = Table::new("invalid".to_owned(), schema.clone())
        .expect_err("Null is not a physical schema type");
    assert!(matches!(
        &table_error,
        Error::InvalidQuery(message)
            if message.contains("cannot use Null as a schema type")
    ));

    let mut catalog = Catalog::new();
    let catalog_error = catalog
        .create_table("invalid".to_owned(), schema)
        .expect_err("catalog delegates schema validation to Table");
    assert_eq!(catalog_error, table_error);
    assert!(matches!(
        catalog.table("invalid"),
        Err(Error::TableNotFound(name)) if name == "invalid"
    ));
}
