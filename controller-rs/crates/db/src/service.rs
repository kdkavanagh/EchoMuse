//! The database service (§5.1): one dedicated thread owns the read-write connection and
//! applies write commands one at a time, in the order they were queued, each in its own
//! transaction; a few more threads each own a read-only connection and answer queries.
//!
//! Callers on any Tokio runtime queue a command and await a reply that arrives only
//! after the command's transaction committed, or with the reason it did not. Both queues
//! are bounded: a caller waits for room instead of growing memory without limit.

use std::{
    fmt,
    panic::{self, AssertUnwindSafe},
    path::Path,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};

use echomuse_core::services::PersistError;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::open::{BUSY_TIMEOUT, OpenedDatabase};

/// A change to the database, applied in one transaction on the writer thread.
pub trait WriteCommand: Send + 'static {
    type Output: Send + 'static;
    /// The command's own failure. Any error rolls the whole transaction back.
    type Error: From<rusqlite::Error> + Send + 'static;

    fn apply(self, tx: &Transaction<'_>) -> Result<Self::Output, Self::Error>;
}

/// A query answered on a read-only connection. All its statements see one snapshot.
pub trait ReadQuery: Send + 'static {
    type Output: Send + 'static;
    type Error: From<rusqlite::Error> + Send + 'static;

    fn read(self, conn: &Connection) -> Result<Self::Output, Self::Error>;
}

/// Why a command or query has no result.
#[derive(Debug)]
pub enum ServiceError<E> {
    /// The service is not accepting work; nothing was queued or written.
    Closed,
    /// The command or query failed; a write's transaction rolled back.
    Failed(E),
    /// The command panicked; its transaction rolled back.
    Aborted,
    /// The thread stopped before replying. A write may or may not have committed; a query
    /// changed nothing.
    Unknown,
}

impl<E: fmt::Display> fmt::Display for ServiceError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("database service is closed"),
            Self::Failed(e) => e.fmt(f),
            Self::Aborted => f.write_str("database command panicked and was rolled back"),
            Self::Unknown => f.write_str("database thread stopped before reporting"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for ServiceError<E> {}

impl<E: fmt::Display> From<ServiceError<E>> for PersistError {
    fn from(error: ServiceError<E>) -> Self {
        match error {
            ServiceError::Closed => Self::Closed,
            ServiceError::Failed(e) => Self::Failed(e.to_string()),
            ServiceError::Aborted => Self::Failed("the command panicked".to_owned()),
            ServiceError::Unknown => Self::Unknown,
        }
    }
}

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send>;
type ReadJob = Box<dyn FnOnce(&Connection) + Send>;

enum Message<J> {
    /// The permit is the job's place in the bounded queue; it is released once the job ran.
    Job(J, OwnedSemaphorePermit),
    /// Accept nothing more, finish what is queued, then stop.
    Stop,
}

/// A bounded queue: the semaphore counts free places and the channel carries jobs in
/// order. Closing the semaphore refuses new jobs and wakes every caller waiting for room.
struct Queue<J> {
    places: Arc<Semaphore>,
    sender: mpsc::UnboundedSender<Message<J>>,
}

impl<J> Clone for Queue<J> {
    fn clone(&self) -> Self {
        Self {
            places: self.places.clone(),
            sender: self.sender.clone(),
        }
    }
}

impl<J> Queue<J> {
    fn new(capacity: usize) -> (Self, mpsc::UnboundedReceiver<Message<J>>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let queue = Self {
            places: Arc::new(Semaphore::new(capacity)),
            sender,
        };
        (queue, receiver)
    }

    /// Ok once the job is queued; Err when the queue is closed.
    async fn push(&self, job: J) -> Result<(), ()> {
        let place = self.places.clone().acquire_owned().await.map_err(|_| ())?;
        self.sender.send(Message::Job(job, place)).map_err(|_| ())
    }

    fn close(&self) {
        if !self.places.is_closed() {
            self.places.close();
            let _ = self.sender.send(Message::Stop);
        }
    }
}

/// Queue sizes and the number of read-only connections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceConfig {
    /// Write commands queued or running at most.
    pub write_queue: usize,
    /// Read-only connections, each on its own thread.
    pub read_connections: usize,
    /// Queries queued or running at most, across all read-only connections.
    pub read_queue: usize,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            write_queue: 256,
            read_connections: 2,
            read_queue: 64,
        }
    }
}

/// Why the service did not start. The database was not changed.
#[derive(Debug)]
pub enum StartError {
    /// A queue size or the connection count is zero or too large.
    InvalidConfig,
    /// A read-only connection could not be opened.
    OpenReader(rusqlite::Error),
    /// A thread could not be started.
    Thread(std::io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig => {
                f.write_str("database queue sizes and the read connection count must be positive")
            }
            Self::OpenReader(e) => write!(f, "could not open a read-only connection: {e}"),
            Self::Thread(e) => write!(f, "could not start a database thread: {e}"),
        }
    }
}

impl std::error::Error for StartError {}

/// Why shutdown did not end cleanly.
#[derive(Debug, Eq, PartialEq)]
pub enum ShutdownError {
    /// Some threads were still running at the deadline; they were left to finish alone.
    Timeout,
    /// A database thread panicked outside any command; its name.
    Panicked(String),
}

impl fmt::Display for ShutdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("database threads did not stop before the deadline"),
            Self::Panicked(name) => write!(f, "database thread {name} panicked"),
        }
    }
}

impl std::error::Error for ShutdownError {}

/// How callers reach the database service. Cheap to clone.
#[derive(Clone)]
pub struct DbHandle {
    writes: Queue<WriteJob>,
    reads: Queue<ReadJob>,
}

/// A write that is queued. Commands apply in the order their [`DbHandle::submit`]
/// completed; dropping this does not withdraw the command.
#[must_use]
pub struct PendingWrite<T, E> {
    reply: oneshot::Receiver<Result<T, ServiceError<E>>>,
}

impl<T, E> PendingWrite<T, E> {
    /// The result, once the transaction committed or rolled back.
    pub async fn outcome(self) -> Result<T, ServiceError<E>> {
        self.reply.await.unwrap_or(Err(ServiceError::Unknown))
    }
}

impl DbHandle {
    /// Queue a write command, waiting for room if the queue is full.
    pub async fn submit<C: WriteCommand>(
        &self,
        command: C,
    ) -> Result<PendingWrite<C::Output, C::Error>, ServiceError<C::Error>> {
        let (reply, receiver) = oneshot::channel();
        let job: WriteJob = Box::new(move |conn: &mut Connection| {
            let result = match panic::catch_unwind(AssertUnwindSafe(|| run_write(conn, command))) {
                Ok(result) => result,
                Err(_) => {
                    end_open_transaction(conn);
                    Err(ServiceError::Aborted)
                }
            };
            let _ = reply.send(result);
        });
        self.writes
            .push(job)
            .await
            .map_err(|()| ServiceError::Closed)?;
        Ok(PendingWrite { reply: receiver })
    }

    /// Queue a write command and wait until its transaction committed.
    pub async fn write<C: WriteCommand>(
        &self,
        command: C,
    ) -> Result<C::Output, ServiceError<C::Error>> {
        self.submit(command).await?.outcome().await
    }

    /// Answer a query on one of the read-only connections.
    pub async fn read<Q: ReadQuery>(&self, query: Q) -> Result<Q::Output, ServiceError<Q::Error>> {
        let (reply, receiver) = oneshot::channel();
        let job: ReadJob = Box::new(move |conn: &Connection| {
            let result = match panic::catch_unwind(AssertUnwindSafe(|| run_read(conn, query))) {
                Ok(result) => result,
                Err(_) => {
                    end_open_transaction(conn);
                    Err(ServiceError::Aborted)
                }
            };
            let _ = reply.send(result);
        });
        self.reads
            .push(job)
            .await
            .map_err(|()| ServiceError::Closed)?;
        receiver.await.unwrap_or(Err(ServiceError::Unknown))
    }
}

fn run_write<C: WriteCommand>(
    conn: &mut Connection,
    command: C,
) -> Result<C::Output, ServiceError<C::Error>> {
    let failed = |e: rusqlite::Error| ServiceError::Failed(C::Error::from(e));
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(failed)?;
    let output = command.apply(&tx).map_err(ServiceError::Failed)?;
    // A failed commit leaves the transaction open, and dropping it rolls it back.
    tx.commit().map_err(failed)?;
    Ok(output)
}

fn run_read<Q: ReadQuery>(
    conn: &Connection,
    query: Q,
) -> Result<Q::Output, ServiceError<Q::Error>> {
    let failed = |e: rusqlite::Error| ServiceError::Failed(Q::Error::from(e));
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred).map_err(failed)?;
    let output = query.read(&tx).map_err(ServiceError::Failed)?;
    tx.rollback().map_err(failed)?;
    Ok(output)
}

/// After a panic, make sure no transaction is left open on the connection. Unwinding
/// normally rolls it back already, when the transaction is dropped.
fn end_open_transaction(conn: &Connection) {
    if !conn.is_autocommit() {
        let _ = conn.execute_batch("ROLLBACK");
    }
}

struct Worker {
    name: String,
    thread: JoinHandle<()>,
    /// Resolves when the thread's sender is dropped, that is when the thread ends.
    exited: oneshot::Receiver<()>,
}

/// The running database service. Dropping it closes the queues and lets its threads
/// finish what was queued; [`DatabaseService::shutdown`] also waits for them.
pub struct DatabaseService {
    handle: DbHandle,
    workers: Vec<Worker>,
}

impl fmt::Debug for DatabaseService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseService")
            .field("threads", &self.workers.len())
            .finish_non_exhaustive()
    }
}

fn open_reader(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

impl DatabaseService {
    /// Start the writer thread with the opened database's connection and
    /// `config.read_connections` reader threads.
    pub fn start(database: OpenedDatabase, config: ServiceConfig) -> Result<Self, StartError> {
        let fits = |n: usize| n > 0 && n <= Semaphore::MAX_PERMITS;
        if !fits(config.write_queue) || !fits(config.read_queue) || config.read_connections == 0 {
            return Err(StartError::InvalidConfig);
        }
        let readers = (0..config.read_connections)
            .map(|_| open_reader(&database.path))
            .collect::<Result<Vec<_>, _>>()
            .map_err(StartError::OpenReader)?;

        let (writes, write_jobs) = Queue::new(config.write_queue);
        let (reads, read_jobs) = Queue::new(config.read_queue);
        let mut service = Self {
            handle: DbHandle { writes, reads },
            workers: Vec::with_capacity(readers.len() + 1),
        };
        // On an error below, dropping `service` closes the queues so started threads end.
        let writer = database.connection;
        service.spawn("echomuse-db-writer".to_owned(), move || {
            write_loop(writer, write_jobs)
        })?;
        let read_jobs = Arc::new(Mutex::new(read_jobs));
        for (index, conn) in readers.into_iter().enumerate() {
            let jobs = read_jobs.clone();
            service.spawn(format!("echomuse-db-reader-{index}"), move || {
                read_loop(conn, &jobs)
            })?;
        }
        Ok(service)
    }

    fn spawn(
        &mut self,
        name: String,
        body: impl FnOnce() + Send + 'static,
    ) -> Result<(), StartError> {
        let (exit_signal, exited) = oneshot::channel::<()>();
        let thread = thread::Builder::new()
            .name(name.clone())
            .spawn(move || {
                let _exit_signal = exit_signal;
                body();
            })
            .map_err(StartError::Thread)?;
        self.workers.push(Worker {
            name,
            thread,
            exited,
        });
        Ok(())
    }

    /// A handle for queuing commands and queries.
    pub fn handle(&self) -> DbHandle {
        self.handle.clone()
    }

    fn close(&self) {
        self.handle.writes.close();
        self.handle.reads.close();
    }

    /// Refuse new work, let the threads finish everything already queued, and wait for
    /// them until `deadline`. Callers still waiting for room get [`ServiceError::Closed`].
    pub async fn shutdown(mut self, deadline: Duration) -> Result<(), ShutdownError> {
        self.close();
        let mut workers = std::mem::take(&mut self.workers);
        let all_exited = async {
            for worker in &mut workers {
                let _ = (&mut worker.exited).await;
            }
        };
        if tokio::time::timeout(deadline, all_exited).await.is_err() {
            return Err(ShutdownError::Timeout);
        }
        let mut result = Ok(());
        for worker in workers {
            if worker.thread.join().is_err() && result.is_ok() {
                result = Err(ShutdownError::Panicked(worker.name));
            }
        }
        result
    }
}

impl Drop for DatabaseService {
    fn drop(&mut self) {
        self.close();
    }
}

fn write_loop(mut conn: Connection, mut jobs: mpsc::UnboundedReceiver<Message<WriteJob>>) {
    while let Some(message) = jobs.blocking_recv() {
        match message {
            Message::Job(job, place) => {
                job(&mut conn);
                drop(place);
            }
            // Jobs queued before the stop still arrive; then the channel ends.
            Message::Stop => jobs.close(),
        }
    }
    // Closing the last connection checkpoints the WAL into the database file.
    let _ = conn.close();
}

fn read_loop(conn: Connection, jobs: &Mutex<mpsc::UnboundedReceiver<Message<ReadJob>>>) {
    loop {
        let message = {
            let Ok(mut jobs) = jobs.lock() else { break };
            match jobs.blocking_recv() {
                Some(Message::Stop) => {
                    jobs.close();
                    continue;
                }
                Some(Message::Job(job, place)) => (job, place),
                None => break,
            }
        };
        let (job, place) = message;
        job(&conn);
        drop(place);
    }
    let _ = conn.close();
}
