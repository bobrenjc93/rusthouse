use std::fs::{self, Permissions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use parquet::basic::{LogicalType, Repetition, Type as PhysicalType};
use parquet::data_type::{BoolType, ByteArray, ByteArrayType, DoubleType, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::Type;
use tempfile::NamedTempFile;

use crate::{DataType, QueryResult, Value};

/// Write one query result to a Parquet file using an atomic same-directory rename.
///
/// Every RustHouse type maps to its corresponding Parquet physical type. Because
/// RustHouse does not support nulls, all fields are required.
pub fn write_parquet(result: &QueryResult, path: &Path) -> Result<(), String> {
    let schema = parquet_schema(result)?;
    let existing_permissions = existing_file_permissions(path)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = create_temporary_file(parent).map_err(|error| {
        format!(
            "could not create temporary Parquet file in '{}': {error}",
            parent.display()
        )
    })?;

    write_parquet_data(temporary.as_file_mut(), schema, result)?;
    if let Some(permissions) = existing_permissions {
        temporary
            .as_file()
            .set_permissions(permissions)
            .map_err(|error| {
                format!(
                    "could not preserve permissions for Parquet output '{}': {error}",
                    path.display()
                )
            })?;
    }
    temporary.as_file_mut().sync_all().map_err(|error| {
        format!(
            "could not sync temporary Parquet file for '{}': {error}",
            path.display()
        )
    })?;
    temporary.persist(path).map_err(|error| {
        format!(
            "could not atomically replace Parquet output '{}': {}",
            path.display(),
            error.error
        )
    })?;
    Ok(())
}

fn existing_file_permissions(path: &Path) -> Result<Option<Permissions>, String> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.permissions())),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "could not inspect Parquet output '{}': {error}",
            path.display()
        )),
    }
}

#[cfg(unix)]
fn create_temporary_file(parent: &Path) -> std::io::Result<NamedTempFile> {
    tempfile::Builder::new()
        .permissions(Permissions::from_mode(0o666))
        .tempfile_in(parent)
}

#[cfg(not(unix))]
fn create_temporary_file(parent: &Path) -> std::io::Result<NamedTempFile> {
    NamedTempFile::new_in(parent)
}

fn parquet_schema(result: &QueryResult) -> Result<Arc<Type>, String> {
    let fields = result
        .columns
        .iter()
        .map(|column| {
            let physical_type = match column.data_type {
                DataType::Int64 => PhysicalType::INT64,
                DataType::Float64 => PhysicalType::DOUBLE,
                DataType::Bool => PhysicalType::BOOLEAN,
                DataType::String => PhysicalType::BYTE_ARRAY,
            };
            let logical_type =
                (column.data_type == DataType::String).then_some(LogicalType::String);
            Type::primitive_type_builder(&column.name, physical_type)
                .with_repetition(Repetition::REQUIRED)
                .with_logical_type(logical_type)
                .build()
                .map(Arc::new)
        })
        .collect::<parquet::errors::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;

    Type::group_type_builder("rusthouse")
        .with_fields(fields)
        .build()
        .map(Arc::new)
        .map_err(|error| error.to_string())
}

fn write_parquet_data<W: Write + Send>(
    output: W,
    schema: Arc<Type>,
    result: &QueryResult,
) -> Result<(), String> {
    let properties = Arc::new(
        WriterProperties::builder()
            .set_created_by(format!("RustHouse {}", env!("CARGO_PKG_VERSION")))
            .build(),
    );
    let mut file = SerializedFileWriter::new(output, schema, properties)
        .map_err(|error| format!("could not start Parquet output: {error}"))?;
    for (row_index, row) in result.rows.iter().enumerate() {
        if row.len() != result.columns.len() {
            return Err(format!(
                "query result row {} has {} values, expected {}",
                row_index + 1,
                row.len(),
                result.columns.len()
            ));
        }
    }
    let mut row_group = file
        .next_row_group()
        .map_err(|error| format!("could not start Parquet row group: {error}"))?;

    for (column_index, column) in result.columns.iter().enumerate() {
        let mut writer = row_group
            .next_column()
            .map_err(|error| format!("could not start Parquet column '{}': {error}", column.name))?
            .ok_or_else(|| format!("Parquet schema is missing column '{}'", column.name))?;

        match column.data_type {
            DataType::Int64 => {
                let values =
                    typed_values(result, column_index, &column.name, |value| match value {
                        Value::Int64(value) => Some(*value),
                        _ => None,
                    })?;
                writer
                    .typed::<Int64Type>()
                    .write_batch(&values, None, None)
                    .map_err(|error| parquet_column_error(&column.name, error))?;
            }
            DataType::Float64 => {
                let values =
                    typed_values(result, column_index, &column.name, |value| match value {
                        Value::Float64(value) => Some(*value),
                        _ => None,
                    })?;
                writer
                    .typed::<DoubleType>()
                    .write_batch(&values, None, None)
                    .map_err(|error| parquet_column_error(&column.name, error))?;
            }
            DataType::Bool => {
                let values =
                    typed_values(result, column_index, &column.name, |value| match value {
                        Value::Bool(value) => Some(*value),
                        _ => None,
                    })?;
                writer
                    .typed::<BoolType>()
                    .write_batch(&values, None, None)
                    .map_err(|error| parquet_column_error(&column.name, error))?;
            }
            DataType::String => {
                let values =
                    typed_values(result, column_index, &column.name, |value| match value {
                        Value::String(value) => Some(ByteArray::from(value.as_str())),
                        _ => None,
                    })?;
                writer
                    .typed::<ByteArrayType>()
                    .write_batch(&values, None, None)
                    .map_err(|error| parquet_column_error(&column.name, error))?;
            }
        }

        writer
            .close()
            .map_err(|error| parquet_column_error(&column.name, error))?;
    }

    row_group
        .close()
        .map_err(|error| format!("could not finish Parquet row group: {error}"))?;
    file.close()
        .map_err(|error| format!("could not finish Parquet output: {error}"))?;
    Ok(())
}

fn typed_values<T>(
    result: &QueryResult,
    column_index: usize,
    column_name: &str,
    convert: impl Fn(&Value) -> Option<T>,
) -> Result<Vec<T>, String> {
    result
        .rows
        .iter()
        .enumerate()
        .map(|(row_index, row)| {
            let value = row.get(column_index).ok_or_else(|| {
                format!(
                    "query result row {} has no value for column '{}'",
                    row_index + 1,
                    column_name
                )
            })?;
            convert(value).ok_or_else(|| {
                format!(
                    "query result row {} has type {} for column '{}', expected {}",
                    row_index + 1,
                    value.data_type(),
                    column_name,
                    result.columns[column_index].data_type
                )
            })
        })
        .collect()
}

fn parquet_column_error(column_name: &str, error: parquet::errors::ParquetError) -> String {
    format!("could not write Parquet column '{column_name}': {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResultColumn;

    #[test]
    fn removes_temporary_file_when_result_data_does_not_match_schema() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let output = directory.path().join("result.parquet");
        std::fs::write(&output, b"existing output").expect("seed existing output");
        let result = QueryResult {
            columns: vec![ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            }],
            rows: vec![vec![Value::String("wrong".to_owned())]],
        };

        let error = write_parquet(&result, &output).expect_err("mismatched result type");

        assert!(error.contains("expected Int64"));
        assert_eq!(
            std::fs::read(&output).expect("read existing output"),
            b"existing output"
        );
        assert_eq!(
            directory
                .path()
                .read_dir()
                .expect("read temporary directory")
                .count(),
            1
        );
    }
}
