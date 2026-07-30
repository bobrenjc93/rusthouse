use std::fmt::Display;
use std::fs::File;

use parquet::basic::{ConvertedType, LogicalType, Type as PhysicalType};
use parquet::column::reader::ColumnReader;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::schema::types::ColumnDescriptor;

use crate::error::{Error, Result};
use crate::storage::Table;
use crate::value::{DataType, Value};

// Both the decoder and the pending table append are capped at this row count.
const COPY_BATCH_ROWS: usize = 1_024;

struct SourceColumn {
    name: String,
    target_index: usize,
    target_name: String,
    max_definition_level: i16,
}

pub(crate) fn ingest(target: &mut Table, columns: Option<&[String]>, path: &str) -> Result<usize> {
    let target_indices = resolve_target_columns(target, columns)?;
    let file = File::open(path).map_err(|error| copy_error(path, "cannot open file", error))?;
    let reader = SerializedFileReader::new(file)
        .map_err(|error| copy_error(path, "cannot read Parquet metadata", error))?;
    let source_columns = validate_schema(target, &target_indices, &reader, path)?;

    let mut affected_rows = 0;
    for row_group_index in 0..reader.num_row_groups() {
        let row_group = reader
            .get_row_group(row_group_index)
            .map_err(|error| copy_error(path, "cannot open Parquet row group", error))?;
        let expected_rows = usize::try_from(row_group.metadata().num_rows()).map_err(|_| {
            copy_message(
                path,
                format!("row group {row_group_index} has an invalid row count"),
            )
        })?;
        let mut column_readers = (0..source_columns.len())
            .map(|index| {
                row_group.get_column_reader(index).map_err(|error| {
                    copy_error(
                        path,
                        format!("cannot open column {index} in row group {row_group_index}"),
                        error,
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut row_group_offset = 0;
        while row_group_offset < expected_rows {
            let batch_limit = COPY_BATCH_ROWS.min(expected_rows - row_group_offset);
            let mut decoded_columns = Vec::with_capacity(source_columns.len());
            let mut decoded_rows = None;

            for (source, column_reader) in source_columns.iter().zip(column_readers.iter_mut()) {
                let (record_count, values) =
                    read_column_batch(column_reader, source, batch_limit, affected_rows, path)?;
                if let Some(expected) = decoded_rows {
                    if record_count != expected {
                        return Err(copy_message(
                            path,
                            format!(
                                "columns in row group {row_group_index} decoded different row counts"
                            ),
                        ));
                    }
                } else {
                    decoded_rows = Some(record_count);
                }
                decoded_columns.push(values);
            }

            let record_count = decoded_rows.unwrap_or(0);
            if record_count == 0 {
                return Err(copy_message(
                    path,
                    format!(
                        "row group {row_group_index} ended after {row_group_offset} of {expected_rows} rows"
                    ),
                ));
            }

            let mut rows = (0..record_count)
                .map(|_| {
                    std::iter::repeat_with(|| None)
                        .take(target.schema().len())
                        .collect::<Vec<Option<Value>>>()
                })
                .collect::<Vec<_>>();
            for (source, values) in source_columns.iter().zip(decoded_columns) {
                for (row, value) in rows.iter_mut().zip(values) {
                    row[source.target_index] = Some(value);
                }
            }
            let rows = rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| value.expect("COPY plan covers every target column"))
                        .collect()
                })
                .collect();

            target.insert_batch(rows)?;
            affected_rows += record_count;
            row_group_offset += record_count;
        }
    }

    Ok(affected_rows)
}

fn resolve_target_columns(target: &Table, requested: Option<&[String]>) -> Result<Vec<usize>> {
    let indices = if let Some(columns) = requested {
        let mut indices = Vec::with_capacity(columns.len());
        let mut seen = vec![false; target.schema().len()];
        for name in columns {
            let index = target.column_index(name)?;
            if seen[index] {
                return Err(Error::InvalidQuery(format!(
                    "COPY column '{name}' is listed more than once"
                )));
            }
            seen[index] = true;
            indices.push(index);
        }
        indices
    } else {
        (0..target.schema().len()).collect()
    };

    if indices.len() != target.schema().len() {
        return Err(Error::InvalidQuery(format!(
            "COPY into table '{}' must provide all {} columns because table columns are non-nullable",
            target.name(),
            target.schema().len()
        )));
    }
    Ok(indices)
}

fn validate_schema(
    target: &Table,
    target_indices: &[usize],
    reader: &SerializedFileReader<File>,
    path: &str,
) -> Result<Vec<SourceColumn>> {
    let schema = reader.metadata().file_metadata().schema_descr();
    if schema.num_columns() != target_indices.len() {
        return Err(copy_message(
            path,
            format!(
                "Parquet file has {} columns but COPY expects {}",
                schema.num_columns(),
                target_indices.len()
            ),
        ));
    }

    let mut source_columns = Vec::with_capacity(schema.num_columns());
    for (source_index, target_index) in target_indices.iter().copied().enumerate() {
        let descriptor = schema.column(source_index);
        let source_name = descriptor.path().string();
        let target_field = &target.schema()[target_index];

        if descriptor.path().parts().len() != 1 || descriptor.max_rep_level() != 0 {
            return Err(copy_message(
                path,
                format!(
                    "Parquet column '{source_name}' is nested or repeated; COPY supports only flat scalar columns"
                ),
            ));
        }
        if descriptor.max_def_level() > 1 {
            return Err(copy_message(
                path,
                format!("Parquet column '{source_name}' has unsupported nested nullability"),
            ));
        }
        if !type_matches(&descriptor, target_field.data_type) {
            return Err(copy_message(
                path,
                format!(
                    "Parquet column '{source_name}' has type {}; target column '{}.{}' requires {}",
                    parquet_type_name(&descriptor),
                    target.name(),
                    target_field.name,
                    target_field.data_type
                ),
            ));
        }

        source_columns.push(SourceColumn {
            name: source_name,
            target_index,
            target_name: target_field.name.clone(),
            max_definition_level: descriptor.max_def_level(),
        });
    }
    Ok(source_columns)
}

fn type_matches(descriptor: &ColumnDescriptor, target_type: DataType) -> bool {
    let physical = descriptor.physical_type();
    let converted = descriptor.converted_type();
    let logical = descriptor.logical_type_ref();
    match target_type {
        DataType::Int64 => {
            physical == PhysicalType::INT64
                && match logical {
                    None => matches!(converted, ConvertedType::NONE | ConvertedType::INT_64),
                    Some(LogicalType::Integer(integer)) => {
                        integer.bit_width == 64
                            && integer.is_signed
                            && matches!(converted, ConvertedType::NONE | ConvertedType::INT_64)
                    }
                    _ => false,
                }
        }
        DataType::Float64 => physical == PhysicalType::DOUBLE && unannotated(converted, logical),
        DataType::Bool => physical == PhysicalType::BOOLEAN && unannotated(converted, logical),
        DataType::String => {
            physical == PhysicalType::BYTE_ARRAY
                && match logical {
                    None => converted == ConvertedType::UTF8,
                    Some(LogicalType::String) => {
                        matches!(converted, ConvertedType::NONE | ConvertedType::UTF8)
                    }
                    _ => false,
                }
        }
    }
}

fn unannotated(converted: ConvertedType, logical: Option<&LogicalType>) -> bool {
    converted == ConvertedType::NONE && logical.is_none()
}

fn parquet_type_name(descriptor: &ColumnDescriptor) -> String {
    match descriptor.logical_type_ref() {
        Some(logical) => format!("{:?} ({logical:?})", descriptor.physical_type()),
        None if descriptor.converted_type() != ConvertedType::NONE => format!(
            "{:?} ({:?})",
            descriptor.physical_type(),
            descriptor.converted_type()
        ),
        None => format!("{:?}", descriptor.physical_type()),
    }
}

fn read_column_batch(
    reader: &mut ColumnReader,
    source: &SourceColumn,
    limit: usize,
    first_row: usize,
    path: &str,
) -> Result<(usize, Vec<Value>)> {
    macro_rules! read_values {
        ($reader:expr, $convert:expr) => {{
            let mut definition_levels = Vec::with_capacity(limit);
            let mut values = Vec::with_capacity(limit);
            let (records, value_count, level_count) = $reader
                .read_records(limit, Some(&mut definition_levels), None, &mut values)
                .map_err(|error| {
                    copy_error(
                        path,
                        format!("cannot decode Parquet column '{}'", source.name),
                        error,
                    )
                })?;
            let converted = assemble_values(
                records,
                value_count,
                level_count,
                definition_levels,
                values,
                source,
                first_row,
                path,
                $convert,
            )?;
            (records, converted)
        }};
    }

    let batch = match reader {
        ColumnReader::BoolColumnReader(reader) => {
            read_values!(reader, |value, _row| Ok(Value::Bool(value)))
        }
        ColumnReader::Int64ColumnReader(reader) => {
            read_values!(reader, |value, _row| Ok(Value::Int64(value)))
        }
        ColumnReader::DoubleColumnReader(reader) => read_values!(reader, |value: f64, row| {
            if value.is_finite() {
                Ok(Value::Float64(value))
            } else {
                Err(copy_message(
                    path,
                    format!(
                        "Parquet column '{}' contains a non-finite DOUBLE at row {}",
                        source.name, row
                    ),
                ))
            }
        }),
        ColumnReader::ByteArrayColumnReader(reader) => read_values!(reader, |value, row| {
            value
                .as_utf8()
                .map(|value| Value::String(value.to_owned()))
                .map_err(|error| {
                    copy_error(
                        path,
                        format!(
                            "Parquet column '{}' contains invalid UTF-8 at row {row}",
                            source.name
                        ),
                        error,
                    )
                })
        }),
        _ => {
            return Err(copy_message(
                path,
                format!(
                    "Parquet decoder for column '{}' did not match its validated schema",
                    source.name
                ),
            ));
        }
    };
    Ok(batch)
}

#[allow(clippy::too_many_arguments)]
fn assemble_values<T, F>(
    records: usize,
    value_count: usize,
    level_count: usize,
    definition_levels: Vec<i16>,
    values: Vec<T>,
    source: &SourceColumn,
    first_row: usize,
    path: &str,
    mut convert: F,
) -> Result<Vec<Value>>
where
    F: FnMut(T, usize) -> Result<Value>,
{
    if values.len() != value_count || level_count != records {
        return Err(copy_message(
            path,
            format!("invalid level counts in Parquet column '{}'", source.name),
        ));
    }

    if source.max_definition_level == 0 {
        if value_count != records {
            return Err(copy_message(
                path,
                format!(
                    "missing values in required Parquet column '{}'",
                    source.name
                ),
            ));
        }
        return values
            .into_iter()
            .enumerate()
            .map(|(offset, value)| convert(value, first_row + offset + 1))
            .collect();
    }

    if definition_levels.len() != records {
        return Err(copy_message(
            path,
            format!(
                "invalid definition levels in nullable Parquet column '{}'",
                source.name
            ),
        ));
    }
    let mut values = values.into_iter();
    let mut converted = Vec::with_capacity(records);
    for (offset, definition_level) in definition_levels.into_iter().enumerate() {
        if definition_level != source.max_definition_level {
            return Err(copy_message(
                path,
                format!(
                    "NULL at row {} in Parquet column '{}' cannot be loaded into non-nullable target column '{}'",
                    first_row + offset + 1,
                    source.name,
                    source.target_name
                ),
            ));
        }
        let value = values.next().ok_or_else(|| {
            copy_message(
                path,
                format!("missing value in Parquet column '{}'", source.name),
            )
        })?;
        converted.push(convert(value, first_row + offset + 1)?);
    }
    if values.next().is_some() {
        return Err(copy_message(
            path,
            format!("extra values in Parquet column '{}'", source.name),
        ));
    }
    Ok(converted)
}

fn copy_error(path: &str, context: impl Display, error: impl Display) -> Error {
    copy_message(path, format!("{context}: {error}"))
}

fn copy_message(path: &str, message: impl Into<String>) -> Error {
    Error::Copy {
        path: path.to_owned(),
        message: message.into(),
    }
}
