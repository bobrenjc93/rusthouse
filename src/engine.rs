use crate::catalog::Catalog;
use crate::error::Result;
use crate::executor;
use crate::plan::LogicalPlan;
use crate::sql::{self, Select, Statement};
use crate::value::{DataType, Value};

pub use crate::plan::ResultColumn;

/// A reusable in-memory SQL database.
#[derive(Debug, Default)]
pub struct Database {
    catalog: Catalog,
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
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Resolve a parsed SELECT into a typed plan without executing it.
    pub fn plan(&self, select: &Select) -> Result<LogicalPlan> {
        LogicalPlan::build(self.catalog.table(&select.table)?, select)
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
            Statement::Select(select) => self.execute_select(&select).map(StatementResult::Query),
            Statement::Explain { select, analyze } => self
                .execute_explain(&select, analyze)
                .map(StatementResult::Query),
        }
    }

    fn execute_select(&self, select: &Select) -> Result<QueryResult> {
        let table = self.catalog.table(&select.table)?;
        let plan = LogicalPlan::build(table, select)?;
        let output = executor::execute(table, &plan)?;
        Ok(QueryResult {
            columns: plan.output_columns,
            rows: output.rows,
        })
    }

    fn execute_explain(&self, select: &Select, analyze: bool) -> Result<QueryResult> {
        let table = self.catalog.table(&select.table)?;
        let plan = LogicalPlan::build(table, select)?;
        let lines = if analyze {
            let output = executor::execute(table, &plan)?;
            plan.explain_analyze(&output.metrics)
        } else {
            plan.explain()
        };
        Ok(QueryResult {
            columns: vec![ResultColumn {
                name: "plan".to_owned(),
                data_type: DataType::String,
            }],
            rows: lines
                .into_iter()
                .map(|line| vec![Value::String(line)])
                .collect(),
        })
    }
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
    fn plans_and_executes_groups_filters_and_ordering() {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE sales (region String, amount Int64, active Bool); \
                 INSERT INTO sales VALUES \
                 ('west', 10, true), ('east', 4, false), ('west', 7, true);",
            )
            .expect("setup");

        let result = query(
            &mut database,
            "SELECT region, COUNT(*) AS n, SUM(amount) AS total \
             FROM sales WHERE active = true \
             GROUP BY region ORDER BY total DESC LIMIT 1",
        );
        assert_eq!(
            result.rows,
            vec![vec![
                Value::String("west".to_owned()),
                Value::Int64(2),
                Value::Int64(17),
            ]]
        );
    }
}
