//! The table builder: one background thread that loads the dictionary, compiles tables and
//! speakers' preferences, and publishes each new table set whole.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

use echomuse_settings::settings::TranscriptSettings;
use tokio::sync::{oneshot, watch};

use crate::{
    lexicon::Lexicon,
    tables::{Preferences, TableInputs, Tables},
};

enum Request {
    Build {
        revision: u64,
        inputs: TableInputs,
    },
    Preferences {
        tables: Arc<Tables>,
        settings: TranscriptSettings,
        reply: oneshot::Sender<Arc<Preferences>>,
    },
}

/// The builder thread stopped by panicking.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct BuilderPanicked;

impl std::fmt::Display for BuilderPanicked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the correction table builder panicked")
    }
}

impl std::error::Error for BuilderPanicked {}

/// The handle to the table builder thread. The current tables are published on a `watch`
/// channel as `Option<Arc<Tables>>`: `None` while none are usable.
///
/// The builder works on one request at a time, newest build first: when several builds
/// are queued only the latest is compiled, and a build is published only if no newer one
/// was requested meanwhile. Dropping every handle stops the thread after its current job.
#[derive(Debug)]
pub struct TableBuilder {
    requests: mpsc::Sender<Request>,
    published: Arc<watch::Sender<Option<Arc<Tables>>>>,
    revision: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl TableBuilder {
    /// Starts the builder thread. The dictionary is parsed on that thread, before its first
    /// job.
    pub fn spawn() -> std::io::Result<(Self, watch::Receiver<Option<Arc<Tables>>>)> {
        Self::spawn_with(Lexicon::cmu)
    }

    /// Starts the builder with another dictionary source (tests use a small one).
    pub(crate) fn spawn_with(
        lexicon: impl FnOnce() -> Lexicon + Send + 'static,
    ) -> std::io::Result<(Self, watch::Receiver<Option<Arc<Tables>>>)> {
        let (published, receiver) = watch::channel(None);
        let published = Arc::new(published);
        let revision = Arc::new(AtomicU64::new(0));
        let (requests, queue) = mpsc::channel::<Request>();
        let thread_published = published.clone();
        let thread_revision = revision.clone();
        let thread = std::thread::Builder::new()
            .name("correction-tables".to_owned())
            .spawn(move || {
                let lexicon = Arc::new(lexicon());
                serve(&queue, &lexicon, &thread_published, &thread_revision);
            })?;
        Ok((
            Self {
                requests,
                published,
                revision,
                thread: Some(thread),
            },
            receiver,
        ))
    }

    /// Withdraws the current tables at once (a change in Home Assistant made them wrong,
    /// or the connection was lost): corrections report `not_ready` until a build publishes.
    /// A build already queued or running is not published either.
    pub fn withdraw(&self) {
        self.revision.fetch_add(1, Ordering::SeqCst);
        self.published.send_replace(None);
    }

    /// Queues a build from `inputs` and returns its revision. The current tables stay
    /// published until it completes; call [`TableBuilder::withdraw`] first when they must
    /// not be used meanwhile.
    pub fn rebuild(&self, inputs: TableInputs) -> u64 {
        let revision = self.revision.fetch_add(1, Ordering::SeqCst) + 1;
        // A send fails only once the thread has stopped; `shutdown` reports why.
        let _ = self.requests.send(Request::Build { revision, inputs });
        revision
    }

    /// Compiles a speaker's preferences against `tables` on the builder thread. The
    /// receiver yields nothing if the thread stopped.
    pub fn compile_preferences(
        &self,
        tables: Arc<Tables>,
        settings: TranscriptSettings,
    ) -> oneshot::Receiver<Arc<Preferences>> {
        let (reply, receiver) = oneshot::channel();
        let _ = self.requests.send(Request::Preferences {
            tables,
            settings,
            reply,
        });
        receiver
    }

    /// Stops the thread after its current job and waits for it.
    pub fn shutdown(mut self) -> Result<(), BuilderPanicked> {
        let thread = self.thread.take();
        drop(self);
        match thread {
            Some(t) => t.join().map_err(|_| BuilderPanicked),
            None => Ok(()),
        }
    }
}

fn serve(
    queue: &mpsc::Receiver<Request>,
    lexicon: &Arc<Lexicon>,
    published: &watch::Sender<Option<Arc<Tables>>>,
    revision: &AtomicU64,
) {
    while let Ok(first) = queue.recv() {
        // Drain what is queued: compile only the newest build, answer every preference
        // request.
        let mut pending = vec![first];
        pending.extend(queue.try_iter());
        let mut newest: Option<(u64, TableInputs)> = None;
        let mut preferences = Vec::new();
        for request in pending {
            match request {
                Request::Build { revision, inputs } => {
                    if newest.as_ref().is_none_or(|(r, _)| revision > *r) {
                        newest = Some((revision, inputs));
                    }
                }
                Request::Preferences {
                    tables,
                    settings,
                    reply,
                } => preferences.push((tables, settings, reply)),
            }
        }
        for (tables, settings, reply) in preferences {
            let _ = reply.send(Arc::new(Preferences::compile(&tables, &settings)));
        }
        if let Some((build, inputs)) = newest {
            if revision.load(Ordering::SeqCst) != build {
                continue;
            }
            let tables = Arc::new(Tables::build(build, &inputs, lexicon.clone()));
            // Publish only if nothing newer was requested while building.
            published.send_if_modified(|current| {
                if revision.load(Ordering::SeqCst) == build {
                    *current = Some(tables);
                    true
                } else {
                    false
                }
            });
        }
    }
}
