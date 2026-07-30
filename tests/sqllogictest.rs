use std::error::Error as StdError;
use std::fmt;
use std::path::PathBuf;

use rusthouse::{DataType, Database, Error, StatementResult};
use sqllogictest::{ColumnType, DB, DBOutput, Runner};

#[derive(Debug, Clone, PartialEq, Eq)]
enum RustHouseColumnType {
    Int64,
    Float64,
    Bool,
    String,
}

impl ColumnType for RustHouseColumnType {
    fn from_char(value: char) -> Option<Self> {
        match value {
            'I' => Some(Self::Int64),
            'R' => Some(Self::Float64),
            'B' => Some(Self::Bool),
            'T' => Some(Self::String),
            _ => None,
        }
    }

    fn to_char(&self) -> char {
        match self {
            Self::Int64 => 'I',
            Self::Float64 => 'R',
            Self::Bool => 'B',
            Self::String => 'T',
        }
    }
}

impl From<DataType> for RustHouseColumnType {
    fn from(value: DataType) -> Self {
        match value {
            DataType::Int64 => Self::Int64,
            DataType::Float64 => Self::Float64,
            DataType::Bool => Self::Bool,
            DataType::String => Self::String,
        }
    }
}

#[derive(Debug)]
enum AdapterError {
    Database(Error),
    ResultCount(usize),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => error.fmt(formatter),
            Self::ResultCount(count) => write!(
                formatter,
                "SQLLogicTest records must produce one result, but this record produced {count}"
            ),
        }
    }
}

impl StdError for AdapterError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::ResultCount(_) => None,
        }
    }
}

impl From<Error> for AdapterError {
    fn from(value: Error) -> Self {
        Self::Database(value)
    }
}

#[derive(Debug, Default)]
struct RustHouseAdapter {
    database: Database,
}

impl DB for RustHouseAdapter {
    type Error = AdapterError;
    type ColumnType = RustHouseColumnType;

    fn run(&mut self, sql: &str) -> Result<DBOutput<Self::ColumnType>, Self::Error> {
        let results = self.database.execute(sql)?;
        let [result] = results.as_slice() else {
            return Err(AdapterError::ResultCount(results.len()));
        };

        match result {
            StatementResult::Command { affected_rows, .. } => {
                Ok(DBOutput::StatementComplete(*affected_rows as u64))
            }
            StatementResult::Query(result) => Ok(DBOutput::Rows {
                types: result
                    .columns
                    .iter()
                    .map(|column| column.data_type.into())
                    .collect(),
                rows: result
                    .rows
                    .iter()
                    .map(|row| row.iter().map(|value| value.as_display_string()).collect())
                    .collect(),
            }),
        }
    }

    fn engine_name(&self) -> &str {
        "rusthouse"
    }
}

#[test]
fn conformance_corpus() {
    let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("slt")
        .join("conformance.slt");
    let mut runner = Runner::new(|| async { Ok::<_, AdapterError>(RustHouseAdapter::default()) });

    runner
        .run_file(corpus)
        .unwrap_or_else(|error| panic!("{}", error.display(false)));
}
