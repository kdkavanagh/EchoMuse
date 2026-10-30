//! The sound catalog's own thread (spec §5.1 "Background work"). It owns the
//! [`SoundCatalog`], exports what is pending at start, takes uploads and deletions one at
//! a time, and publishes a fresh [`CatalogSnapshot`] after every change. Nothing it does
//! blocks a device runtime or the I/O runtime: callers await a reply, and readers take the
//! latest snapshot.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use echomuse_settings::values::SoundChoice;
use tokio::sync::{oneshot, watch};

use super::catalog::{CatalogSnapshot, DeleteError, SoundCatalog, SoundSummary, UploadError};
use super::{SoundId, SoundsDir, UploadFormat};

/// How many requests may wait for the thread.
pub const QUEUE_CAPACITY: usize = 4;
/// How often the idle thread looks at the stop flag.
const STOP_POLL: Duration = Duration::from_millis(50);

/// Why a request got no answer from the catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceError<E> {
    /// Too many requests are waiting; nothing was done.
    Busy,
    /// The service has stopped; nothing was done, or the outcome is unknown if it stopped
    /// while working on the request.
    Stopped,
    /// The catalog refused it.
    Refused(E),
}

impl<E: fmt::Display> fmt::Display for ServiceError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("the sound service is busy; try again"),
            Self::Stopped => f.write_str("the sound service has stopped"),
            Self::Refused(e) => write!(f, "{e}"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for ServiceError<E> {}

enum Command {
    Upload {
        id: SoundId,
        format: UploadFormat,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<SoundSummary, UploadError>>,
    },
    Delete {
        id: SoundId,
        selected: Vec<SoundChoice>,
        reply: oneshot::Sender<Result<(), DeleteError>>,
    },
}

/// What callers hold: requests and the latest snapshot.
#[derive(Clone, Debug)]
pub struct SoundHandle {
    commands: SyncSender<Command>,
    snapshot: watch::Receiver<Arc<CatalogSnapshot>>,
    dir: SoundsDir,
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Upload { id, .. } => write!(f, "Upload({id})"),
            Self::Delete { id, .. } => write!(f, "Delete({id})"),
        }
    }
}

impl SoundHandle {
    /// The catalog now.
    pub fn snapshot(&self) -> Arc<CatalogSnapshot> {
        self.snapshot.borrow().clone()
    }

    /// Follows the catalog: a new snapshot after every change.
    pub fn subscribe(&self) -> watch::Receiver<Arc<CatalogSnapshot>> {
        self.snapshot.clone()
    }

    /// The directory, for serving assets by hash.
    pub fn dir(&self) -> &SoundsDir {
        &self.dir
    }

    /// Stores and exports an upload (`POST /api/sounds/upload`).
    pub async fn upload(
        &self,
        id: SoundId,
        format: UploadFormat,
        bytes: Vec<u8>,
    ) -> Result<SoundSummary, ServiceError<UploadError>> {
        let (reply, answer) = oneshot::channel();
        self.request(Command::Upload {
            id,
            format,
            bytes,
            reply,
        })?;
        answer
            .await
            .map_err(|_| ServiceError::Stopped)?
            .map_err(ServiceError::Refused)
    }

    /// Deletes a sound unless one of `selected` (every stored `timerSound` and
    /// `alarmSound`) names it.
    pub async fn delete(
        &self,
        id: SoundId,
        selected: Vec<SoundChoice>,
    ) -> Result<(), ServiceError<DeleteError>> {
        let (reply, answer) = oneshot::channel();
        self.request(Command::Delete {
            id,
            selected,
            reply,
        })?;
        answer
            .await
            .map_err(|_| ServiceError::Stopped)?
            .map_err(ServiceError::Refused)
    }

    fn request<E>(&self, command: Command) -> Result<(), ServiceError<E>> {
        self.commands.try_send(command).map_err(|e| match e {
            TrySendError::Full(_) => ServiceError::Busy,
            TrySendError::Disconnected(_) => ServiceError::Stopped,
        })
    }
}

/// The running service. Dropping it stops the thread.
#[derive(Debug)]
pub struct SoundService {
    handle: SoundHandle,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SoundService {
    /// Reads the catalog and starts its thread, which first exports anything pending. The
    /// directory read blocks: call it during startup, off the async runtime.
    pub fn start(dir: SoundsDir, ffmpeg: PathBuf) -> io::Result<Self> {
        let catalog = SoundCatalog::open(dir.clone(), ffmpeg)?;
        let (published, snapshot) = watch::channel(Arc::new(catalog.snapshot()));
        let (commands, inbox) = mpsc::sync_channel(QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::Builder::new()
            .name("sounds".to_owned())
            .spawn(move || run(catalog, &inbox, &published, &flag))?;
        Ok(Self {
            handle: SoundHandle {
                commands,
                snapshot,
                dir,
            },
            stop,
            thread: Some(thread),
        })
    }

    /// A handle for callers.
    pub fn handle(&self) -> SoundHandle {
        self.handle.clone()
    }
}

impl Drop for SoundService {
    /// Stops the thread, killing a running `ffmpeg`, and waits for it. Requests still
    /// queued are answered [`ServiceError::Stopped`].
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    mut catalog: SoundCatalog,
    inbox: &mpsc::Receiver<Command>,
    published: &watch::Sender<Arc<CatalogSnapshot>>,
    stop: &AtomicBool,
) {
    let publish = |catalog: &SoundCatalog| {
        published.send_replace(Arc::new(catalog.snapshot()));
    };
    for id in catalog.pending() {
        // Requests go first, so an upload never waits behind a backlog of exports.
        while let Ok(command) = inbox.try_recv() {
            handle(&mut catalog, command, stop, &publish);
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // A failure stays pending in the catalog and is retried at the next start.
        let _ = catalog.export(&id, stop);
        publish(&catalog);
    }
    while !stop.load(Ordering::Relaxed) {
        match inbox.recv_timeout(STOP_POLL) {
            Ok(command) => handle(&mut catalog, command, stop, &publish),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Carries out one request and publishes the catalog it leaves before answering, so a
/// caller that reads the catalog after its answer sees its own change.
fn handle(
    catalog: &mut SoundCatalog,
    command: Command,
    stop: &AtomicBool,
    publish: &impl Fn(&SoundCatalog),
) {
    match command {
        Command::Upload {
            id,
            format,
            bytes,
            reply,
        } => {
            let result = catalog.upload(id, format, &bytes, stop);
            publish(catalog);
            let _ = reply.send(result);
        }
        Command::Delete {
            id,
            selected,
            reply,
        } => {
            let result = catalog.delete(&id, &selected);
            publish(catalog);
            let _ = reply.send(result);
        }
    }
}
