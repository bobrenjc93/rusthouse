use std::sync::{Arc, RwLock};

use crate::engine::{Database, StatementResult};
use crate::error::{Error, LockAccess, Result};
use crate::sql::{self, Statement};

/// A cloneable, synchronized handle to an in-memory [`Database`].
///
/// Each call parses its complete SQL batch before acquiring a lock. A batch
/// containing only `SELECT` statements holds one shared read lock throughout
/// execution. A batch containing any mutation holds one exclusive write lock
/// throughout execution, including its `SELECT` statements, so other handles
/// cannot observe intermediate batch state.
///
/// The standard library's [`RwLock`] does not guarantee a particular waiter
/// ordering. If a thread panics while holding the write lock, later calls
/// return [`Error::LockPoisoned`] instead of panicking.
#[derive(Debug, Clone)]
pub struct SharedDatabase {
    database: Arc<RwLock<Database>>,
}

impl SharedDatabase {
    /// Creates an empty shared database.
    #[must_use]
    pub fn new() -> Self {
        Self::from(Database::new())
    }

    /// Executes one or more semicolon-separated statements in order.
    ///
    /// Clones of this handle may execute read-only batches concurrently.
    /// Mutation-containing batches are serialized with all other execution.
    pub fn execute(&self, sql: &str) -> Result<Vec<StatementResult>> {
        self.execute_with_observer(sql, |_| {})
    }

    fn execute_with_observer(
        &self,
        sql: &str,
        mut after_statement: impl FnMut(usize),
    ) -> Result<Vec<StatementResult>> {
        let statements = sql::parse(sql)?;
        let read_only = statements
            .iter()
            .all(|statement| matches!(statement, Statement::Select(_)));

        if read_only {
            let database = self.database.read().map_err(|_| Error::LockPoisoned {
                access: LockAccess::Read,
            })?;
            let mut results = Vec::with_capacity(statements.len());
            for (index, statement) in statements.into_iter().enumerate() {
                let Statement::Select(select) = statement else {
                    unreachable!("read-only batches contain only SELECT statements")
                };
                results.push(StatementResult::Query(database.execute_select(select)?));
                after_statement(index);
            }
            Ok(results)
        } else {
            let mut database = self.database.write().map_err(|_| Error::LockPoisoned {
                access: LockAccess::Write,
            })?;
            let mut results = Vec::with_capacity(statements.len());
            for (index, statement) in statements.into_iter().enumerate() {
                results.push(database.execute_statement(statement)?);
                after_statement(index);
            }
            Ok(results)
        }
    }
}

impl Default for SharedDatabase {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Database> for SharedDatabase {
    fn from(database: Database) -> Self {
        Self {
            database: Arc::new(RwLock::new(database)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Barrier, mpsc};
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::value::Value;

    const TIMEOUT: Duration = Duration::from_secs(2);
    const BLOCKED_CHECK: Duration = Duration::from_millis(100);

    fn row_count(results: Vec<StatementResult>) -> i64 {
        let StatementResult::Query(result) = results.into_iter().last().expect("query result")
        else {
            panic!("expected query result");
        };
        let Value::Int64(count) = result.rows[0][0] else {
            panic!("expected Int64 count");
        };
        count
    }

    #[test]
    fn read_only_batches_can_hold_read_locks_concurrently() {
        let database = SharedDatabase::new();
        database
            .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (1);")
            .expect("setup succeeds");

        let release_first = Arc::new(Barrier::new(2));
        let (first_locked_tx, first_locked_rx) = mpsc::channel();
        let first = {
            let database = database.clone();
            let release_first = Arc::clone(&release_first);
            thread::spawn(move || {
                database.execute_with_observer("SELECT * FROM events", |_| {
                    first_locked_tx.send(()).expect("signal first reader");
                    release_first.wait();
                })
            })
        };
        first_locked_rx
            .recv_timeout(TIMEOUT)
            .expect("first reader acquired its lock");

        let (second_locked_tx, second_locked_rx) = mpsc::channel();
        let second = {
            let database = database.clone();
            thread::spawn(move || {
                database.execute_with_observer("SELECT * FROM events", |_| {
                    second_locked_tx.send(()).expect("signal second reader");
                })
            })
        };

        let shared = second_locked_rx.recv_timeout(TIMEOUT).is_ok();
        release_first.wait();
        first
            .join()
            .expect("first reader thread")
            .expect("first read");
        second
            .join()
            .expect("second reader thread")
            .expect("second read");
        assert!(
            shared,
            "second reader should acquire while first reader holds its lock"
        );
    }

    #[test]
    fn writer_excludes_readers_and_other_writers() {
        let database = SharedDatabase::new();
        database
            .execute("CREATE TABLE events (id Int64)")
            .expect("setup succeeds");

        let release_writer = Arc::new(Barrier::new(2));
        let (writer_locked_tx, writer_locked_rx) = mpsc::channel();
        let writer = {
            let database = database.clone();
            let release_writer = Arc::clone(&release_writer);
            thread::spawn(move || {
                database.execute_with_observer("INSERT INTO events VALUES (1)", |_| {
                    writer_locked_tx.send(()).expect("signal writer");
                    release_writer.wait();
                })
            })
        };
        writer_locked_rx
            .recv_timeout(TIMEOUT)
            .expect("writer acquired its lock");

        let contenders_ready = Arc::new(Barrier::new(3));
        let (reader_tx, reader_rx) = mpsc::channel();
        let reader = {
            let database = database.clone();
            let contenders_ready = Arc::clone(&contenders_ready);
            thread::spawn(move || {
                contenders_ready.wait();
                let result = database.execute("SELECT * FROM events");
                reader_tx.send(result).expect("send reader result");
            })
        };
        let (second_writer_tx, second_writer_rx) = mpsc::channel();
        let second_writer = {
            let database = database.clone();
            let contenders_ready = Arc::clone(&contenders_ready);
            thread::spawn(move || {
                contenders_ready.wait();
                let result = database.execute("INSERT INTO events VALUES (2)");
                second_writer_tx.send(result).expect("send writer result");
            })
        };
        contenders_ready.wait();

        assert_eq!(
            reader_rx.recv_timeout(BLOCKED_CHECK),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        assert_eq!(
            second_writer_rx.recv_timeout(BLOCKED_CHECK),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        release_writer.wait();

        writer.join().expect("writer thread").expect("first write");
        reader_rx
            .recv_timeout(TIMEOUT)
            .expect("reader unblocked")
            .expect("read succeeds");
        second_writer_rx
            .recv_timeout(TIMEOUT)
            .expect("writer unblocked")
            .expect("second write succeeds");
        reader.join().expect("reader thread");
        second_writer.join().expect("second writer thread");
    }

    #[test]
    fn readers_cannot_observe_intermediate_mutation_batch_state() {
        let database = SharedDatabase::new();
        database
            .execute("CREATE TABLE events (id Int64)")
            .expect("setup succeeds");

        let release_batch = Arc::new(Barrier::new(2));
        let (first_insert_tx, first_insert_rx) = mpsc::channel();
        let writer = {
            let database = database.clone();
            let release_batch = Arc::clone(&release_batch);
            thread::spawn(move || {
                database.execute_with_observer(
                    "INSERT INTO events VALUES (1); INSERT INTO events VALUES (2)",
                    |index| {
                        if index == 0 {
                            first_insert_tx.send(()).expect("signal first insert");
                            release_batch.wait();
                        }
                    },
                )
            })
        };
        first_insert_rx
            .recv_timeout(TIMEOUT)
            .expect("first insert completed");

        let reader_ready = Arc::new(Barrier::new(2));
        let (count_tx, count_rx) = mpsc::channel();
        let reader = {
            let database = database.clone();
            let reader_ready = Arc::clone(&reader_ready);
            thread::spawn(move || {
                reader_ready.wait();
                let results = database
                    .execute("SELECT COUNT(*) FROM events")
                    .expect("count succeeds");
                count_tx.send(row_count(results)).expect("send count");
            })
        };
        reader_ready.wait();
        assert_eq!(
            count_rx.recv_timeout(BLOCKED_CHECK),
            Err(mpsc::RecvTimeoutError::Timeout)
        );

        release_batch.wait();
        writer.join().expect("writer thread").expect("write batch");
        assert_eq!(count_rx.recv_timeout(TIMEOUT).expect("reader unblocked"), 2);
        reader.join().expect("reader thread");
    }

    #[test]
    fn poisoned_locks_return_structured_errors() {
        for (sql, expected_access) in [
            ("SELECT * FROM missing", LockAccess::Read),
            ("CREATE TABLE events (id Int64)", LockAccess::Write),
        ] {
            let database = SharedDatabase::new();
            let locked_database = Arc::clone(&database.database);
            assert!(
                thread::spawn(move || {
                    let _guard = locked_database.write().expect("initial write lock");
                    panic!("poison lock for test");
                })
                .join()
                .is_err()
            );

            assert_eq!(
                database.execute(sql),
                Err(Error::LockPoisoned {
                    access: expected_access
                })
            );
        }
    }
}
