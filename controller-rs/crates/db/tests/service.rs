//! The database service: one writer in order, commit-acknowledged replies, read-only
//! connections, bounded queues and shutdown (§5.1).

mod common;

use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{Mutex, mpsc},
    task::Poll,
    time::Duration,
};

use common::{db_path, start, start_with};
use echomuse_core::services::PersistError;
use echomuse_db::{
    DatabaseService, OpenedDatabase, ReadQuery, ServiceConfig, ServiceError, ShutdownError,
    StartError, WriteCommand,
};
use rusqlite::{Connection, Transaction};

#[derive(Debug)]
enum TestError {
    Sqlite(rusqlite::Error),
    Refused,
}

impl From<rusqlite::Error> for TestError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl fmt::Display for TestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => e.fmt(f),
            Self::Refused => f.write_str("refused"),
        }
    }
}

fn insert(tx: &Transaction<'_>, id: &str) -> rusqlite::Result<i64> {
    tx.execute(
        "INSERT INTO alert_scripts (object_id, sha256, revision) VALUES (?1, 'x', 1)",
        [id],
    )?;
    Ok(tx.last_insert_rowid())
}

struct Insert(String);

impl WriteCommand for Insert {
    type Output = i64;
    type Error = TestError;

    fn apply(self, tx: &Transaction<'_>) -> Result<i64, TestError> {
        Ok(insert(tx, &self.0)?)
    }
}

/// Inserts, then fails: the insert must not survive.
struct InsertThenRefuse(String);

impl WriteCommand for InsertThenRefuse {
    type Output = ();
    type Error = TestError;

    fn apply(self, tx: &Transaction<'_>) -> Result<(), TestError> {
        insert(tx, &self.0)?;
        Err(TestError::Refused)
    }
}

struct InsertThenPanic(String);

impl WriteCommand for InsertThenPanic {
    type Output = ();
    type Error = TestError;

    fn apply(self, tx: &Transaction<'_>) -> Result<(), TestError> {
        insert(tx, &self.0)?;
        panic!("a command bug");
    }
}

/// Holds the writer until the test sends on the channel, then inserts.
struct InsertWhenReleased {
    id: String,
    release: Mutex<mpsc::Receiver<()>>,
}

impl WriteCommand for InsertWhenReleased {
    type Output = i64;
    type Error = TestError;

    fn apply(self, tx: &Transaction<'_>) -> Result<i64, TestError> {
        self.release.lock().unwrap().recv().unwrap();
        Ok(insert(tx, &self.id)?)
    }
}

fn blocker(id: &str) -> (InsertWhenReleased, mpsc::Sender<()>) {
    let (release, wait) = mpsc::channel();
    (
        InsertWhenReleased {
            id: id.to_owned(),
            release: Mutex::new(wait),
        },
        release,
    )
}

struct ListIds;

impl ReadQuery for ListIds {
    type Output = Vec<(i64, String)>;
    type Error = TestError;

    fn read(self, conn: &Connection) -> Result<Self::Output, TestError> {
        let mut statement =
            conn.prepare("SELECT rowid, object_id FROM alert_scripts ORDER BY rowid")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// A query that tries to write.
struct WriteFromReader;

impl ReadQuery for WriteFromReader {
    type Output = ();
    type Error = TestError;

    fn read(self, conn: &Connection) -> Result<(), TestError> {
        conn.execute(
            "INSERT INTO alert_scripts (object_id, sha256, revision) VALUES ('r', 'x', 1)",
            [],
        )?;
        Ok(())
    }
}

/// A log line for a device that does not exist.
struct OrphanLog;

impl WriteCommand for OrphanLog {
    type Output = ();
    type Error = TestError;

    fn apply(self, tx: &Transaction<'_>) -> Result<(), TestError> {
        tx.execute(
            "INSERT INTO device_logs (device_id, ts, level, source, message) \
             VALUES ('NOSUCHDEVICE0000', 1, 'info', 'controller', 'x')",
            [],
        )?;
        Ok(())
    }
}

/// Poll a future once with the task's real waker.
async fn poll_once<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    let mut future = Some(future);
    std::future::poll_fn(|cx| Poll::Ready(future.take().unwrap().poll(cx))).await
}

const DEADLINE: Duration = Duration::from_secs(10);

#[tokio::test]
async fn writes_apply_in_the_order_they_were_queued() {
    let dir = tempfile::tempdir().unwrap();
    let (service, db) = start(&db_path(&dir));

    let mut pending = Vec::new();
    for n in 0..50 {
        pending.push(db.submit(Insert(format!("s{n:02}"))).await.unwrap());
    }
    let mut row_ids = Vec::new();
    for write in pending {
        row_ids.push(write.outcome().await.unwrap());
    }
    assert!(row_ids.windows(2).all(|pair| pair[0] < pair[1]));
    let listed = db.read(ListIds).await.unwrap();
    let ids: Vec<String> = listed.into_iter().map(|(_, id)| id).collect();
    let expected: Vec<String> = (0..50).map(|n| format!("s{n:02}")).collect();
    assert_eq!(ids, expected);
    service.shutdown(DEADLINE).await.unwrap();
}

#[tokio::test]
async fn a_reply_means_the_write_committed() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let (service, db) = start(&path);

    let row_id = db.write(Insert("one".into())).await.unwrap();
    assert_eq!(
        db.read(ListIds).await.unwrap(),
        [(row_id, "one".to_owned())]
    );
    // Another process sees it too: it is in the WAL, committed.
    let outside: i64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT count(*) FROM alert_scripts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(outside, 1);
    service.shutdown(DEADLINE).await.unwrap();
}

#[tokio::test]
async fn a_failed_command_rolls_back_and_the_writer_continues() {
    let dir = tempfile::tempdir().unwrap();
    let (service, db) = start(&db_path(&dir));

    let error = db.write(InsertThenRefuse("lost".into())).await.unwrap_err();
    assert!(matches!(error, ServiceError::Failed(TestError::Refused)));
    assert_eq!(
        PersistError::from(error),
        PersistError::Failed("refused".into())
    );
    assert!(matches!(
        db.write(InsertThenPanic("lost too".into())).await,
        Err(ServiceError::Aborted)
    ));
    db.write(Insert("kept".into())).await.unwrap();
    let ids: Vec<String> = db
        .read(ListIds)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, id)| id)
        .collect();
    assert_eq!(ids, ["kept"]);
    service.shutdown(DEADLINE).await.unwrap();
}

#[tokio::test]
async fn foreign_keys_are_enforced_and_readers_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let (service, db) = start(&db_path(&dir));

    match db.write(OrphanLog).await {
        Err(ServiceError::Failed(TestError::Sqlite(e))) => {
            assert_eq!(
                e.sqlite_error_code(),
                Some(rusqlite::ErrorCode::ConstraintViolation)
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    match db.read(WriteFromReader).await {
        Err(ServiceError::Failed(TestError::Sqlite(e))) => {
            assert_eq!(e.sqlite_error_code(), Some(rusqlite::ErrorCode::ReadOnly));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(db.read(ListIds).await.unwrap().is_empty());
    service.shutdown(DEADLINE).await.unwrap();
}

#[tokio::test]
async fn a_full_queue_makes_callers_wait_for_room() {
    let dir = tempfile::tempdir().unwrap();
    let config = ServiceConfig {
        write_queue: 1,
        ..ServiceConfig::default()
    };
    let (service, db) = start_with(&db_path(&dir), config);

    let (held, release) = blocker("first");
    let first = db.submit(held).await.unwrap();
    let mut second = Box::pin(db.submit(Insert("second".into())));
    assert!(poll_once(second.as_mut()).await.is_pending());

    release.send(()).unwrap();
    let second = second.await.unwrap();
    first.outcome().await.unwrap();
    second.outcome().await.unwrap();
    assert_eq!(db.read(ListIds).await.unwrap().len(), 2);
    service.shutdown(DEADLINE).await.unwrap();
}

#[tokio::test]
async fn shutdown_finishes_queued_writes_then_refuses_new_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let (service, db) = start(&path);

    let mut pending = Vec::new();
    for n in 0..10 {
        pending.push(db.submit(Insert(format!("q{n}"))).await.unwrap());
    }
    service.shutdown(DEADLINE).await.unwrap();
    for write in pending {
        write.outcome().await.unwrap();
    }
    assert!(matches!(
        db.write(Insert("late".into())).await,
        Err(ServiceError::Closed)
    ));
    assert!(matches!(db.read(ListIds).await, Err(ServiceError::Closed)));

    let count: i64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT count(*) FROM alert_scripts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 10);
}

#[tokio::test]
async fn callers_waiting_for_room_are_refused_at_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let config = ServiceConfig {
        write_queue: 1,
        ..ServiceConfig::default()
    };
    let (service, db) = start_with(&db_path(&dir), config);

    let (held, release) = blocker("running");
    let running = db.submit(held).await.unwrap();
    let mut waiting = Box::pin(db.submit(Insert("waiting".into())));
    assert!(poll_once(waiting.as_mut()).await.is_pending());

    let shutdown = tokio::spawn(service.shutdown(DEADLINE));
    assert!(matches!(waiting.await, Err(ServiceError::Closed)));
    release.send(()).unwrap();
    running.outcome().await.unwrap();
    shutdown.await.unwrap().unwrap();
}

#[tokio::test]
async fn shutdown_gives_up_at_its_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let (service, db) = start(&db_path(&dir));

    let (held, release) = blocker("slow");
    let slow = db.submit(held).await.unwrap();
    assert_eq!(
        service.shutdown(Duration::from_millis(50)).await,
        Err(ShutdownError::Timeout)
    );
    // The thread was left to finish on its own, and still commits what it accepted.
    release.send(()).unwrap();
    slow.outcome().await.unwrap();
}

#[test]
fn zero_sized_queues_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let opened = OpenedDatabase::open(db_path(&dir)).unwrap();
    let config = ServiceConfig {
        read_connections: 0,
        ..ServiceConfig::default()
    };
    assert!(matches!(
        DatabaseService::start(opened, config),
        Err(StartError::InvalidConfig)
    ));
}
