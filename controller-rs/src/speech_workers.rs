//! Per-device speech worker threads (docs/rust-controller-evaluation.md §5.1, §5.5).
//!
//! Every device runtime owns one [`SpeechWorkers`]: four named OS threads, one per
//! [`SpeechWorkerKind`]. Each worker runs one job at a time, synchronously on its own
//! thread, in the order its bounded queue received them. Model sessions and stream
//! state are meant to live inside the closures and state a worker's jobs own, so
//! nothing here is shared between devices: there is no cross-device queue.
//!
//! Admission never waits. A job is refused at submit when the worker is stopped, its
//! scope is already cancelled, its retained bytes do not fit the worker's byte credits,
//! or the worker's message queue is full; the last two cancel the submitting scope
//! (an enqueue that cannot succeed invalidates the operation that needed it, §5.2).
//! Byte credits stay held from submit until the job's work returns.
//!
//! Before running a job the worker drops it unexecuted if its [`Work`] was dropped, and
//! rejects it with [`RuntimeError::Cancelled`] if its scope was cancelled while queued.
//! A job never expires by waiting: it belongs to a request, and the request's own
//! deadlines and cancellation decide when its work stops mattering (§5.2).
//! After the work returns, a result whose scope or worker set was cancelled meanwhile
//! is discarded on the worker thread and never delivered.
//!
//! Cancelling a scope does not stop work that is already running; it only makes
//! [`Work::result`] return at once and guarantees the late value is never delivered.
//! A panic in a job (or anywhere on a worker thread) publishes
//! [`RuntimeError::WorkerPanicked`] on [`SpeechWorkers::failure`] and cancels the stop
//! token, so the damaged worker runs nothing further and the others drain out.
//!
//! Every job's call is reported to the native-call [`Watchdog`] when it starts and when
//! it returns. A value that comes back after the watchdog's call deadline is rejected
//! with [`RuntimeError::Overdue`]; while a call is past that deadline the device's
//! [`SpeechPathState`] names the worker, so only that device refuses wakes (§5.5).

use std::{
    any::Any,
    io,
    ops::Index,
    panic::{self, AssertUnwindSafe},
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use tokio::sync::{
    OwnedSemaphorePermit, Semaphore,
    mpsc::{self, error::TrySendError},
    oneshot, watch,
};
use tokio_util::sync::CancellationToken;

use echomuse_core::types::{RuntimeError, WorkScope};
use echomuse_voice::speech::{SpanJob, SttGain};

use crate::supervision::{SpeechPathState, Watchdog, WorkerOwner};

/// How much each speech worker's queue may hold.
///
/// There is deliberately no limit on how long a job may wait. Dropping a waiting job
/// leaves a hole in its request's evidence and ends the request, while the request's
/// own deadlines and cancellation already decide when its work stops mattering. A
/// worker that stays behind fills these bounds instead, and the device's session is
/// closed as overloaded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerLimits {
    /// Jobs each worker may hold queued.
    pub messages: usize,
    /// Bytes of retained input the microphone analysis, reference scoring and wake
    /// verification workers may each hold, queued or running.
    pub bytes: u32,
    /// Bytes of retained input the span decoding worker may hold, queued or running.
    /// Its jobs are whole committed utterances, so it is sized by the longest span a
    /// turn can commit rather than by a worker's backlog of live audio.
    pub span_bytes: u32,
}

impl Default for WorkerLimits {
    fn default() -> Self {
        Self {
            // A wake candidate's backfill arrives at once: mic from 300 ms before its
            // support (about 26 packets), reference from 2.5 s before that (about 57)
            // and the cells, each a job for the microphone worker.
            messages: 256,
            // 256 KiB: about 8 s of 16 kHz mic.
            bytes: 256 * 1024,
            // Two of the largest span jobs at once: the next turn's commit can arrive
            // while a superseded turn's decode is still running, and that job keeps its
            // credits until its work returns. One turn submits its re-decode and its STT
            // copy one after the other, never together.
            span_bytes: u32::try_from(2 * longest_span_job_bytes())
                .expect("the mic retention's span jobs fit a semaphore"),
        }
    }
}

impl WorkerLimits {
    /// Refuses empty bounds and bounds a semaphore cannot hold.
    pub fn validate(self) -> Result<Self, RuntimeError> {
        if self.messages == 0
            || self.bytes == 0
            || self.span_bytes == 0
            || self.messages > Semaphore::MAX_PERMITS
            || self.bytes as usize > Semaphore::MAX_PERMITS
            || self.span_bytes as usize > Semaphore::MAX_PERMITS
        {
            return Err(RuntimeError::InvalidLimits);
        }
        Ok(self)
    }

    /// The byte credits of worker `kind`.
    const fn bytes_for(self, kind: SpeechWorkerKind) -> u32 {
        match kind {
            SpeechWorkerKind::SpanDecoding => self.span_bytes,
            SpeechWorkerKind::MicrophoneAnalysis
            | SpeechWorkerKind::ReferenceScoring
            | SpeechWorkerKind::WakeVerification => self.bytes,
        }
    }
}

/// The PCM bytes of the longest span a turn can commit.
///
/// A committed span is read whole from the session's mic retention
/// ([`echomuse_uplink::Retention`], 40 s), so no longer span reaches the worker. The
/// retention covers every span the endpoint commits: the longest is a reply's, its
/// 780 ms lead and 7 s wait for command speech before the longest utterance (15 s, or
/// 30 s with `extendedUtterances`) measured from that speech; a wake or press turn adds
/// only its 300 ms pre-roll and the wake word to the same limit.
fn longest_span_bytes() -> usize {
    let samples = usize::try_from(echomuse_uplink::Retention::default().mic)
        .expect("the mic retention fits in memory");
    samples * std::mem::size_of::<i16>()
}

/// The most retained input one span decoding job can be charged: the span, and for an
/// STT copy of a wake turn the wake support it measures its gain from, which is read
/// from the same retention.
fn longest_span_job_bytes() -> usize {
    2 * longest_span_bytes()
}

/// The input a span decoding job keeps alive until it returns, charged against the
/// worker's byte credits.
pub(crate) fn span_job_bytes(job: &SpanJob) -> usize {
    let samples = match job {
        SpanJob::Redecode { pcm, .. } => pcm.len(),
        SpanJob::SttCopy { pcm, gain, .. } => {
            let support = match gain {
                SttGain::FromWake(support) => support.len(),
                SttGain::Nominal => 0,
            };
            pcm.len() + support
        }
    };
    samples * std::mem::size_of::<i16>()
}

/// The four speech workers each device runtime owns.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SpeechWorkerKind {
    /// VAD, streaming ASR and echo attribution over the microphone.
    MicrophoneAnalysis,
    /// Scoring of the device's own render reference.
    ReferenceScoring,
    /// Controller-side verification of wake candidates.
    WakeVerification,
    /// Re-decoding of committed spans.
    SpanDecoding,
}

impl SpeechWorkerKind {
    /// Stable name, used as the thread-name suffix.
    pub const fn name(self) -> &'static str {
        match self {
            Self::MicrophoneAnalysis => "microphone_analysis",
            Self::ReferenceScoring => "reference_scoring",
            Self::WakeVerification => "wake_verification",
            Self::SpanDecoding => "span_decoding",
        }
    }

    /// The worker's name in plain words, for reasons shown to people.
    pub const fn label(self) -> &'static str {
        match self {
            Self::MicrophoneAnalysis => "microphone analysis",
            Self::ReferenceScoring => "reference scoring",
            Self::WakeVerification => "wake verification",
            Self::SpanDecoding => "span decoding",
        }
    }
}

/// One value for each speech worker, addressed by the worker's kind.
#[derive(Clone)]
pub(crate) struct PerWorker<T> {
    microphone_analysis: T,
    reference_scoring: T,
    wake_verification: T,
    span_decoding: T,
}

impl<T> PerWorker<T> {
    pub(crate) fn from_fn(mut value: impl FnMut(SpeechWorkerKind) -> T) -> Self {
        Self {
            microphone_analysis: value(SpeechWorkerKind::MicrophoneAnalysis),
            reference_scoring: value(SpeechWorkerKind::ReferenceScoring),
            wake_verification: value(SpeechWorkerKind::WakeVerification),
            span_decoding: value(SpeechWorkerKind::SpanDecoding),
        }
    }

    /// Like [`PerWorker::from_fn`], stopping at the first error.
    fn try_from_fn<E>(mut value: impl FnMut(SpeechWorkerKind) -> Result<T, E>) -> Result<Self, E> {
        Ok(Self {
            microphone_analysis: value(SpeechWorkerKind::MicrophoneAnalysis)?,
            reference_scoring: value(SpeechWorkerKind::ReferenceScoring)?,
            wake_verification: value(SpeechWorkerKind::WakeVerification)?,
            span_decoding: value(SpeechWorkerKind::SpanDecoding)?,
        })
    }
}

impl<T> Index<SpeechWorkerKind> for PerWorker<T> {
    type Output = T;

    fn index(&self, kind: SpeechWorkerKind) -> &T {
        match kind {
            SpeechWorkerKind::MicrophoneAnalysis => &self.microphone_analysis,
            SpeechWorkerKind::ReferenceScoring => &self.reference_scoring,
            SpeechWorkerKind::WakeVerification => &self.wake_verification,
            SpeechWorkerKind::SpanDecoding => &self.span_decoding,
        }
    }
}

/// One device's speech worker threads.
///
/// Dropping it cancels the stop token without waiting; the idle threads then exit on
/// their own. [`SpeechWorkers::shutdown`] is the waiting form.
pub struct SpeechWorkers {
    handles: PerWorker<SpeechWorkerHandle>,
    threads: Vec<WorkerThread>,
    stop: CancellationToken,
    failure: Arc<watch::Sender<Option<RuntimeError>>>,
    path: Arc<watch::Sender<SpeechPathState>>,
}

struct WorkerThread {
    join: JoinHandle<()>,
    /// Closed (never sent) when the thread's body has returned or unwound.
    exited: oneshot::Receiver<()>,
}

type ThreadBody = Box<dyn FnOnce() + Send + 'static>;

/// Who watches a worker set's calls, and whose workers they are.
#[derive(Clone, Debug)]
pub struct WorkerWatch {
    pub watchdog: Watchdog,
    pub owner: WorkerOwner,
}

/// One worker's link to the watchdog.
struct CallTimer {
    watch: WorkerWatch,
    kind: SpeechWorkerKind,
    path: Arc<watch::Sender<SpeechPathState>>,
}

impl SpeechWorkers {
    /// Starts the four worker threads, named `{thread_prefix}-{kind name}`, whose calls
    /// `watch` tracks.
    ///
    /// `limits` applies to each worker separately, the span decoding worker with its
    /// own byte credits ([`WorkerLimits::bytes_for`]). `stop` is the owning runtime's
    /// token: cancelling it stops every worker, and a worker panic, a failed start or
    /// dropping the returned value cancels it.
    pub fn start(
        thread_prefix: &str,
        limits: WorkerLimits,
        stop: CancellationToken,
        watch: WorkerWatch,
    ) -> Result<Self, RuntimeError> {
        Self::start_with(
            thread_prefix,
            limits,
            stop,
            watch,
            &mut |builder: thread::Builder, body: ThreadBody| builder.spawn(body),
        )
    }

    fn start_with(
        thread_prefix: &str,
        limits: WorkerLimits,
        stop: CancellationToken,
        watch: WorkerWatch,
        spawn: &mut dyn FnMut(thread::Builder, ThreadBody) -> io::Result<JoinHandle<()>>,
    ) -> Result<Self, RuntimeError> {
        // Every bound fits a semaphore once validated.
        let limits = limits.validate()?;

        let failure = Arc::new(watch::Sender::new(None));
        let path = Arc::new(watch::Sender::new(SpeechPathState::default()));
        let mut threads = Vec::with_capacity(4);
        let started = PerWorker::try_from_fn(|kind| {
            let (handle, thread) = spawn_worker(
                kind,
                thread_prefix,
                limits,
                &stop,
                &failure,
                CallTimer {
                    watch: watch.clone(),
                    kind,
                    path: Arc::clone(&path),
                },
                &mut *spawn,
            )?;
            threads.push(thread);
            Ok(handle)
        });
        let handles = match started {
            Ok(handles) => handles,
            Err(error) => {
                stop.cancel();
                // The started workers have run no job and are idle on their queues, so
                // each exits as soon as it sees the stop token.
                for thread in threads {
                    let _ = thread.join.join();
                }
                return Err(error);
            }
        };
        Ok(Self {
            handles,
            threads,
            stop,
            failure,
            path,
        })
    }

    /// A submitter for one worker; clones share its queue and byte credits.
    pub fn handle(&self, kind: SpeechWorkerKind) -> SpeechWorkerHandle {
        self.handles[kind].clone()
    }

    /// The first failure of any worker; `None` while all are healthy.
    pub fn failure(&self) -> watch::Receiver<Option<RuntimeError>> {
        self.failure.subscribe()
    }

    /// Which workers are stuck past the watchdog's call deadline; empty while the
    /// speech path works.
    pub fn speech_path(&self) -> watch::Receiver<SpeechPathState> {
        self.path.subscribe()
    }

    /// Cancels the stop token and waits until `timeout` for every worker thread to exit.
    ///
    /// A worker exits after its current job returns; queued jobs are dropped unrun and
    /// their [`Work`] reports [`RuntimeError::Closed`]. Only threads that have exited are
    /// joined. If any is still inside a job at the deadline it is left running and this
    /// returns [`RuntimeError::ShutdownTimeout`]: a worker that cannot stop is a
    /// process-level failure (§5.5), and the caller must fire the fatal hook rather than
    /// start a replacement. A thread whose join reports a panic that escaped
    /// supervision returns [`RuntimeError::WorkerPanicked`]. A panic already published on
    /// [`SpeechWorkers::failure`] does not fail an otherwise clean shutdown.
    pub async fn shutdown(mut self, timeout: Duration) -> Result<(), RuntimeError> {
        self.stop.cancel();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut timed_out = false;
        let mut panicked = false;
        for thread in std::mem::take(&mut self.threads) {
            let WorkerThread { join, exited } = thread;
            if tokio::time::timeout_at(deadline, exited).await.is_err() {
                timed_out = true;
                continue;
            }
            // The body has returned; only the thread's own teardown remains.
            if join.join().is_err() {
                panicked = true;
                publish_failure(&self.failure, RuntimeError::WorkerPanicked);
            }
        }
        if timed_out {
            Err(RuntimeError::ShutdownTimeout)
        } else if panicked {
            Err(RuntimeError::WorkerPanicked)
        } else {
            Ok(())
        }
    }
}

impl Drop for SpeechWorkers {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn spawn_worker(
    kind: SpeechWorkerKind,
    thread_prefix: &str,
    limits: WorkerLimits,
    stop: &CancellationToken,
    failure: &Arc<watch::Sender<Option<RuntimeError>>>,
    calls: CallTimer,
    spawn: &mut dyn FnMut(thread::Builder, ThreadBody) -> io::Result<JoinHandle<()>>,
) -> Result<(SpeechWorkerHandle, WorkerThread), RuntimeError> {
    let thread_start = |error: io::Error| {
        RuntimeError::ThreadStart(format!("speech worker {}: {error}", kind.name()))
    };
    // Waits on the queue and the stop token together; jobs run outside it.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(thread_start)?;
    let (sender, receiver) = mpsc::channel(limits.messages);
    let (exited_tx, exited) = oneshot::channel();
    let worker = Worker {
        runtime,
        receiver,
        stop: stop.clone(),
        failure: Arc::clone(failure),
        slot: WorkerSlot::default(),
        calls,
    };
    let body: ThreadBody = Box::new(move || {
        // Declared first so it is dropped last, after the worker has torn down.
        let _exited = exited_tx;
        worker.run();
    });
    let builder = thread::Builder::new().name(format!("{thread_prefix}-{}", kind.name()));
    let join = spawn(builder, body).map_err(thread_start)?;
    let handle = SpeechWorkerHandle {
        sender,
        // `start_with` validated that the byte limits fit a semaphore.
        credits: Arc::new(Semaphore::new(limits.bytes_for(kind) as usize)),
        stop: stop.clone(),
    };
    Ok((handle, WorkerThread { join, exited }))
}

fn publish_failure(failure: &watch::Sender<Option<RuntimeError>>, error: RuntimeError) {
    failure.send_if_modified(|current| {
        if current.is_some() {
            return false;
        }
        *current = Some(error);
        true
    });
}

/// What one worker keeps between its jobs: the model sessions and stream state of the
/// work it runs (spec §5.1). Only that worker's jobs ever see it, one at a time.
#[derive(Default)]
pub struct WorkerSlot(Option<Box<dyn Any + Send>>);

impl WorkerSlot {
    /// The worker's state of type `S`, made by `init` the first time it is needed.
    ///
    /// # Panics
    ///
    /// A worker keeps one type of state for its life. Asking for another is a dispatch
    /// bug, and replacing the state would silently drop the streams it holds, so it
    /// panics; the worker reports that as [`RuntimeError::WorkerPanicked`].
    pub fn state<S: Send + 'static>(&mut self, init: impl FnOnce() -> S) -> &mut S {
        self.0
            .get_or_insert_with(|| Box::new(init()))
            .downcast_mut::<S>()
            .expect("a speech worker keeps one type of state")
    }
}

/// Submits jobs to one speech worker.
#[derive(Clone)]
pub struct SpeechWorkerHandle {
    sender: mpsc::Sender<Job>,
    credits: Arc<Semaphore>,
    stop: CancellationToken,
}

impl SpeechWorkerHandle {
    /// Queues `work` to run on this worker's thread, without waiting. `work` gets the
    /// state this worker keeps between jobs (its model sessions and stream state, §5.1).
    ///
    /// `retained_bytes` is what the job keeps alive (its input buffers) until `work`
    /// returns; it is charged against the worker's byte credits for that long.
    ///
    /// Errors: [`RuntimeError::Closed`] when the workers are stopped,
    /// [`RuntimeError::Cancelled`] when `scope` is already cancelled, and
    /// [`RuntimeError::Overloaded`] when the bytes do not fit or the queue is full, in
    /// which case `scope` is cancelled before returning.
    pub fn submit<T, F>(
        &self,
        scope: WorkScope,
        retained_bytes: usize,
        work: F,
    ) -> Result<Work<T>, RuntimeError>
    where
        T: Send + 'static,
        F: FnOnce(&mut WorkerSlot) -> T + Send + 'static,
    {
        if self.stop.is_cancelled() {
            return Err(RuntimeError::Closed);
        }
        if scope.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        let credit = u32::try_from(retained_bytes)
            .ok()
            .and_then(|bytes| Arc::clone(&self.credits).try_acquire_many_owned(bytes).ok());
        let Some(credit) = credit else {
            scope.cancel();
            return Err(RuntimeError::Overloaded);
        };
        let (reply_tx, reply) = oneshot::channel();
        let job = Job {
            scope: scope.clone(),
            credit,
            task: Box::new(Task {
                work,
                reply: reply_tx,
            }),
        };
        match self.sender.try_send(job) {
            Ok(()) => Ok(Work {
                scope,
                stop: self.stop.clone(),
                reply,
            }),
            Err(TrySendError::Full(_)) => {
                scope.cancel();
                Err(RuntimeError::Overloaded)
            }
            Err(TrySendError::Closed(_)) => Err(RuntimeError::Closed),
        }
    }
}

/// The pending result of one submitted job. Dropping it abandons the job: if it has not
/// started yet, the worker drops it without running it.
pub struct Work<T> {
    scope: WorkScope,
    stop: CancellationToken,
    reply: oneshot::Receiver<Result<T, RuntimeError>>,
}

impl<T> Work<T> {
    /// Waits for the job's value.
    ///
    /// Returns [`RuntimeError::Cancelled`] as soon as the scope is cancelled, even while
    /// the work is still running, and [`RuntimeError::Closed`] as soon as the workers
    /// stop. Otherwise it returns the worker's verdict: the value, `Cancelled`, or
    /// `WorkerPanicked` when this job panicked.
    pub async fn result(self) -> Result<T, RuntimeError> {
        let Self {
            scope,
            stop,
            mut reply,
        } = self;
        tokio::select! {
            biased;
            () = scope.cancelled() => Err(RuntimeError::Cancelled),
            delivered = &mut reply => match delivered {
                Ok(Ok(_)) if scope.is_cancelled() => Err(RuntimeError::Cancelled),
                Ok(Ok(_)) if stop.is_cancelled() => Err(RuntimeError::Closed),
                Ok(verdict) => verdict,
                Err(_) => Err(RuntimeError::Closed),
            },
            () = stop.cancelled() => Err(RuntimeError::Closed),
        }
    }
}

struct Job {
    scope: WorkScope,
    credit: OwnedSemaphorePermit,
    task: Box<dyn ErasedTask>,
}

type PanicPayload = Box<dyn Any + Send + 'static>;

trait ErasedTask: Send {
    fn abandoned(&self) -> bool;
    fn reject(self: Box<Self>, error: RuntimeError);
    /// Runs the work against the worker's state with the call reported to the
    /// watchdog, releases `credit`, and delivers the value unless `scope` or `stop` was
    /// cancelled meanwhile or the call ran past its deadline. A panic is reported to the
    /// submitter and returned to the worker.
    fn run(
        self: Box<Self>,
        credit: OwnedSemaphorePermit,
        scope: &WorkScope,
        stop: &CancellationToken,
        slot: &mut WorkerSlot,
        calls: &CallTimer,
    ) -> Result<(), PanicPayload>;
}

struct Task<T, F> {
    work: F,
    reply: oneshot::Sender<Result<T, RuntimeError>>,
}

impl<T, F> ErasedTask for Task<T, F>
where
    T: Send + 'static,
    F: FnOnce(&mut WorkerSlot) -> T + Send + 'static,
{
    fn abandoned(&self) -> bool {
        self.reply.is_closed()
    }

    fn reject(self: Box<Self>, error: RuntimeError) {
        let _ = self.reply.send(Err(error));
    }

    fn run(
        self: Box<Self>,
        credit: OwnedSemaphorePermit,
        scope: &WorkScope,
        stop: &CancellationToken,
        slot: &mut WorkerSlot,
        calls: &CallTimer,
    ) -> Result<(), PanicPayload> {
        let Self { work, reply } = *self;
        let call = calls
            .watch
            .watchdog
            .begin(&calls.watch.owner, calls.kind, &calls.path);
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| work(slot)));
        let late = call.finish();
        drop(credit);
        match outcome {
            Ok(value) => {
                let verdict = if scope.is_cancelled() {
                    Err(RuntimeError::Cancelled)
                } else if stop.is_cancelled() {
                    Err(RuntimeError::Closed)
                } else if late {
                    Err(RuntimeError::Overdue)
                } else {
                    Ok(value)
                };
                // A refused or undeliverable value is dropped here, on the worker.
                let _ = reply.send(verdict);
                Ok(())
            }
            Err(payload) => {
                let _ = reply.send(Err(RuntimeError::WorkerPanicked));
                Err(payload)
            }
        }
    }
}

impl Job {
    fn execute(
        self,
        stop: &CancellationToken,
        slot: &mut WorkerSlot,
        calls: &CallTimer,
    ) -> Result<(), PanicPayload> {
        let Self {
            scope,
            credit,
            task,
        } = self;
        if task.abandoned() {
            return Ok(());
        }
        if scope.is_cancelled() {
            task.reject(RuntimeError::Cancelled);
            return Ok(());
        }
        task.run(credit, &scope, stop, slot, calls)
    }
}

enum WorkerExit {
    Stopped,
    Panicked,
}

struct Worker {
    runtime: tokio::runtime::Runtime,
    receiver: mpsc::Receiver<Job>,
    stop: CancellationToken,
    failure: Arc<watch::Sender<Option<RuntimeError>>>,
    /// What this worker's jobs keep between them. A job that panicked ends the worker,
    /// so a state whose operation was interrupted is never used again (§5.5).
    slot: WorkerSlot,
    /// Reports this worker's calls to the watchdog.
    calls: CallTimer,
}

impl Worker {
    fn run(mut self) {
        let exit = panic::catch_unwind(AssertUnwindSafe(|| self.serve()));
        if !matches!(exit, Ok(WorkerExit::Stopped)) {
            publish_failure(&self.failure, RuntimeError::WorkerPanicked);
            self.stop.cancel();
        }
        // Refuse new jobs, then drop the queued ones unrun; their submitters see
        // `Closed` and their byte credits return.
        self.receiver.close();
        while let Ok(job) = self.receiver.try_recv() {
            drop(job);
        }
    }

    fn serve(&mut self) -> WorkerExit {
        loop {
            let Self {
                runtime,
                receiver,
                stop,
                ..
            } = &mut *self;
            let next = runtime.block_on(async {
                tokio::select! {
                    biased;
                    () = stop.cancelled() => None,
                    job = receiver.recv() => job,
                }
            });
            let Some(job) = next else {
                return WorkerExit::Stopped;
            };
            if job
                .execute(&self.stop, &mut self.slot, &self.calls)
                .is_err()
            {
                return WorkerExit::Panicked;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc as std_mpsc,
    };

    use tokio::time::timeout;

    use super::*;
    use echomuse_core::types::{DeviceId, RuntimeId, SessionId, WorkIdentity};
    use echomuse_turn::{EndpointPolicy, Sample, Samples, TurnSpec};
    use echomuse_voice::speech::{
        SpanOutput,
        models::ModelSet,
        span::SpanDecoding,
        testing::{ToneModels, silence, utterance},
    };

    const PATIENCE: Duration = Duration::from_secs(5);

    fn scope() -> WorkScope {
        WorkScope::new(
            WorkIdentity {
                device: DeviceId("kitchen".to_owned()),
                runtime: RuntimeId(1),
                session: SessionId(1),
                turn: None,
            },
            CancellationToken::new(),
        )
    }

    fn limits(messages: usize, bytes: u32) -> WorkerLimits {
        WorkerLimits {
            messages,
            bytes,
            span_bytes: bytes,
        }
    }

    fn roomy() -> WorkerLimits {
        limits(8, 1024)
    }

    /// A watchdog whose deadlines no test job comes near.
    fn test_watch() -> WorkerWatch {
        watch_with(Duration::from_secs(60), Duration::from_secs(120))
    }

    fn watch_with(call_deadline: Duration, recovery_deadline: Duration) -> WorkerWatch {
        let watchdog = Watchdog::start(
            crate::supervision::WatchdogConfig {
                call_deadline,
                recovery_deadline,
            },
            crate::supervision::FatalHook::new(|reason| {
                eprintln!("speech worker test: the watchdog would end the process: {reason}");
            }),
        )
        .unwrap();
        WorkerWatch {
            watchdog,
            owner: WorkerOwner {
                device: DeviceId("kitchen".to_owned()),
                runtime: RuntimeId(1),
            },
        }
    }

    #[tokio::test]
    async fn a_late_result_is_rejected_and_the_speech_path_recovers_when_it_returns() {
        let workers = SpeechWorkers::start(
            "kitchen",
            roomy(),
            CancellationToken::new(),
            watch_with(Duration::from_millis(50), Duration::from_secs(60)),
        )
        .unwrap();
        let mut path = workers.speech_path();
        let verify = workers.handle(SpeechWorkerKind::WakeVerification);
        let (late, started, release) = submit_blocking(&verify, scope(), 0, 7);
        started.await.unwrap();
        let stuck = timeout(PATIENCE, path.wait_for(|state| !state.overdue.is_empty()))
            .await
            .unwrap()
            .unwrap()
            .clone();
        assert_eq!(
            stuck.overdue.into_iter().collect::<Vec<_>>(),
            [SpeechWorkerKind::WakeVerification]
        );
        release.send(()).unwrap();
        assert_eq!(late.result().await, Err(RuntimeError::Overdue));
        timeout(PATIENCE, path.wait_for(|state| state.overdue.is_empty()))
            .await
            .unwrap()
            .unwrap();
        // The worker keeps running: a quick job afterwards is answered.
        assert_eq!(
            verify.submit(scope(), 0, |_| 8_u32).unwrap().result().await,
            Ok(8)
        );
        workers.shutdown(PATIENCE).await.unwrap();
    }

    /// Submits a job that reports when it starts and then blocks its worker thread
    /// until released.
    fn submit_blocking(
        handle: &SpeechWorkerHandle,
        scope: WorkScope,
        bytes: usize,
        value: u32,
    ) -> (Work<u32>, oneshot::Receiver<()>, std_mpsc::Sender<()>) {
        let (started_tx, started) = oneshot::channel();
        let (release, gate) = std_mpsc::channel::<()>();
        let work = handle
            .submit(scope, bytes, move |_| {
                let _ = started_tx.send(());
                let _ = gate.recv();
                value
            })
            .expect("blocking job admitted");
        (work, started, release)
    }

    #[tokio::test]
    async fn result_returns_on_cancellation_while_work_still_runs() {
        let workers =
            SpeechWorkers::start("kitchen", roomy(), CancellationToken::new(), test_watch())
                .unwrap();
        let mic = workers.handle(SpeechWorkerKind::MicrophoneAnalysis);
        let turn = scope();
        let (finished_tx, mut finished) = oneshot::channel();
        let (started_tx, started) = oneshot::channel();
        let (release, gate) = std_mpsc::channel::<()>();
        let work = mic
            .submit(turn.clone(), 0, move |_| {
                let _ = started_tx.send(());
                let _ = gate.recv();
                let _ = finished_tx.send(());
                1_u32
            })
            .unwrap();
        started.await.unwrap();
        turn.cancel();
        assert_eq!(
            timeout(PATIENCE, work.result()).await.unwrap(),
            Err(RuntimeError::Cancelled)
        );
        assert!(finished.try_recv().is_err(), "the work is still running");

        release.send(()).unwrap();
        finished.await.unwrap();
        let next = mic.submit(scope(), 0, |_| 2_u32).unwrap();
        assert_eq!(next.result().await, Ok(2));
        workers.shutdown(PATIENCE).await.unwrap();
    }

    #[tokio::test]
    async fn worker_refuses_to_deliver_a_result_whose_scope_ended_during_execution() {
        let workers =
            SpeechWorkers::start("kitchen", roomy(), CancellationToken::new(), test_watch())
                .unwrap();
        let verify = workers.handle(SpeechWorkerKind::WakeVerification);
        let candidate = scope();
        let (work, started, release) = submit_blocking(&verify, candidate.clone(), 0, 7);
        started.await.unwrap();
        candidate.cancel();
        release.send(()).unwrap();
        // Read the worker's own verdict, not `Work::result`'s early return.
        assert_eq!(work.reply.await, Ok(Err(RuntimeError::Cancelled)));
        workers.shutdown(PATIENCE).await.unwrap();
    }

    #[tokio::test]
    async fn worker_refuses_to_deliver_a_result_after_its_runtime_stopped() {
        let stop = CancellationToken::new();
        let workers = SpeechWorkers::start("kitchen", roomy(), stop.clone(), test_watch()).unwrap();
        let span = workers.handle(SpeechWorkerKind::SpanDecoding);
        let (work, started, release) = submit_blocking(&span, scope(), 0, 7);
        started.await.unwrap();
        stop.cancel();
        release.send(()).unwrap();
        assert_eq!(work.reply.await, Ok(Err(RuntimeError::Closed)));
        workers.shutdown(PATIENCE).await.unwrap();
    }

    #[tokio::test]
    async fn queued_jobs_that_were_abandoned_or_cancelled_never_run() {
        let workers =
            SpeechWorkers::start("kitchen", roomy(), CancellationToken::new(), test_watch())
                .unwrap();
        let mic = workers.handle(SpeechWorkerKind::MicrophoneAnalysis);
        let (first, started, release) = submit_blocking(&mic, scope(), 0, 1);
        started.await.unwrap();

        let abandoned_ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&abandoned_ran);
        drop(
            mic.submit(scope(), 0, move |_| flag.store(true, Ordering::SeqCst))
                .unwrap(),
        );

        let cancelled_ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled_ran);
        let ended = scope();
        let cancelled = mic
            .submit(ended.clone(), 0, move |_| {
                flag.store(true, Ordering::SeqCst)
            })
            .unwrap();
        ended.cancel();

        let last = mic.submit(scope(), 0, |_| 4_u32).unwrap();
        release.send(()).unwrap();
        assert_eq!(first.result().await, Ok(1));
        assert_eq!(cancelled.reply.await, Ok(Err(RuntimeError::Cancelled)));
        // The worker is in order, so both earlier jobs were taken before this one.
        assert_eq!(last.result().await, Ok(4));
        assert!(!abandoned_ran.load(Ordering::SeqCst));
        assert!(!cancelled_ran.load(Ordering::SeqCst));
        workers.shutdown(PATIENCE).await.unwrap();
    }

    #[tokio::test]
    async fn a_full_queue_refuses_at_once_and_cancels_the_submitter() {
        let workers = SpeechWorkers::start(
            "kitchen",
            limits(1, 1024),
            CancellationToken::new(),
            test_watch(),
        )
        .unwrap();
        let mic = workers.handle(SpeechWorkerKind::MicrophoneAnalysis);
        let (running, started, release) = submit_blocking(&mic, scope(), 0, 1);
        started.await.unwrap();
        let queued_scope = scope();
        let queued = mic.submit(queued_scope.clone(), 0, |_| 2_u32).unwrap();

        let refused_scope = scope();
        assert!(matches!(
            mic.submit(refused_scope.clone(), 0, |_| 3_u32),
            Err(RuntimeError::Overloaded)
        ));
        assert!(refused_scope.is_cancelled());
        assert!(!queued_scope.is_cancelled());

        // Another worker of the same device has its own queue.
        let reference = workers.handle(SpeechWorkerKind::ReferenceScoring);
        assert_eq!(
            reference
                .submit(scope(), 0, |_| 4_u32)
                .unwrap()
                .result()
                .await,
            Ok(4)
        );

        release.send(()).unwrap();
        assert_eq!(running.result().await, Ok(1));
        assert_eq!(queued.result().await, Ok(2));
        workers.shutdown(PATIENCE).await.unwrap();
    }

    #[tokio::test]
    async fn byte_credits_are_held_until_the_work_returns() {
        let workers = SpeechWorkers::start(
            "kitchen",
            limits(8, 100),
            CancellationToken::new(),
            test_watch(),
        )
        .unwrap();
        let span = workers.handle(SpeechWorkerKind::SpanDecoding);

        let oversized = scope();
        assert!(matches!(
            span.submit(oversized.clone(), 101, |_| ()),
            Err(RuntimeError::Overloaded)
        ));
        assert!(oversized.is_cancelled());

        let (running, started, release) = submit_blocking(&span, scope(), 60, 1);
        started.await.unwrap();
        // The queue is empty, but the running job still holds 60 bytes.
        let refused = scope();
        assert!(matches!(
            span.submit(refused.clone(), 50, |_| 2_u32),
            Err(RuntimeError::Overloaded)
        ));
        assert!(refused.is_cancelled());
        let fits = span.submit(scope(), 40, |_| 3_u32).unwrap();

        release.send(()).unwrap();
        assert_eq!(running.result().await, Ok(1));
        assert_eq!(fits.result().await, Ok(3));
        let whole = span.submit(scope(), 100, |_| 4_u32).unwrap();
        assert_eq!(whole.result().await, Ok(4));
        workers.shutdown(PATIENCE).await.unwrap();
    }

    #[test]
    fn endpoint_spans_fit_the_microphone_retention() {
        let policy = EndpointPolicy::default();
        let retention = echomuse_uplink::Retention::default().mic;
        // A reply's span: its lead before the trigger, the wait for command speech, then
        // the longest utterance measured from that speech.
        let trigger = Sample(1_000_000);
        let reply_lead = trigger.0 - TurnSpec::reply_start(trigger, Sample(0)).0;
        let longest_reply = reply_lead + policy.reply_no_input.0 + policy.max_extended_utterance.0;
        assert!(
            longest_reply <= retention,
            "a {longest_reply}-sample reply span outgrows the {retention}-sample retention"
        );
        // A wake turn's: its pre-roll and a long (2 s) wake word, then the same limit.
        let support = Sample(1_000_000);
        let wake_lead = support.0 - TurnSpec::wake_start(support).0 + Samples::from_ms(2_000).0;
        assert!(wake_lead + policy.max_extended_utterance.0 <= retention);
    }

    const WORDS: &[&str] = &["ophelia", "turn", "on", "the", "kitchen", "light"];
    const COMMAND: &[&str] = &["turn", "on", "the", "kitchen", "light"];

    /// A committed span of `len` samples: `lead`, then `phrases` repeats of the command,
    /// then quiet to its end.
    fn spoken_span(m: &ToneModels, len: u64, lead: Vec<i16>, phrases: usize) -> Arc<[i16]> {
        let mut pcm = lead;
        for _ in 0..phrases {
            pcm.extend(utterance(m, COMMAND, 400, 250, 1_000));
        }
        assert!(pcm.len() as u64 <= len, "the speech fits the span");
        pcm.resize(len as usize, 0);
        pcm.into()
    }

    /// Runs `job` as a voice session submits it: charged by [`span_job_bytes`] and run
    /// by the worker's [`SpanDecoding`] state.
    async fn decode_span(
        span: &SpeechWorkerHandle,
        m: &ToneModels,
        job: SpanJob,
    ) -> Result<SpanOutput, RuntimeError> {
        let models: Arc<dyn ModelSet> = Arc::new(m.clone());
        let bytes = span_job_bytes(&job);
        let work = span.submit(scope(), bytes, move |slot| {
            slot.state(|| SpanDecoding::new(models)).run(job)
        })?;
        work.result().await
    }

    fn redecoded_text(output: SpanOutput) -> String {
        let SpanOutput::Redecode { result, .. } = output else {
            panic!("asked for a re-decode");
        };
        result.unwrap().text
    }

    fn stt_copy_len(output: SpanOutput) -> usize {
        let SpanOutput::SttCopy { result, .. } = output else {
            panic!("asked for an STT copy");
        };
        result.unwrap().pcm.len()
    }

    #[tokio::test]
    async fn a_15_s_wake_utterance_and_the_extended_maximum_decode_on_the_default_workers() {
        let policy = EndpointPolicy::default();
        let m = ToneModels::new(WORDS);
        let workers = SpeechWorkers::start(
            "kitchen",
            WorkerLimits::default(),
            CancellationToken::new(),
            test_watch(),
        )
        .unwrap();
        let span = workers.handle(SpeechWorkerKind::SpanDecoding);

        // A wake turn at the normal limit: 300 ms of pre-roll and a 1 s wake word before
        // the trigger, then 15 s of speech after it.
        let support = Sample(160_000);
        let start = TurnSpec::wake_start(support);
        let trigger = support + Samples::from_ms(1_000);
        let len = (trigger + policy.max_utterance).0 - start.0;
        let mut lead = silence(300);
        lead.extend(utterance(&m, &["ophelia"], 750, 250, 1_000));
        assert_eq!(lead.len() as u64, trigger.0 - start.0);
        let wake: Arc<[i16]> = lead[4_800..].into();
        let pcm = spoken_span(&m, len, lead, 4);
        let redecode = SpanJob::Redecode {
            turn: uuid::Uuid::new_v4(),
            pcm: Arc::clone(&pcm),
        };
        let text = redecoded_text(decode_span(&span, &m, redecode).await.unwrap());
        assert_eq!(
            text.matches("turn on the kitchen light").count(),
            4,
            "{text}"
        );
        let copy = SpanJob::SttCopy {
            turn: uuid::Uuid::new_v4(),
            pcm,
            gain: SttGain::FromWake(wake),
            denoise: false,
        };
        let copied = stt_copy_len(decode_span(&span, &m, copy).await.unwrap());
        assert_eq!(copied as u64, len + 8_000);

        // A reply at the extended limit, the longest span a turn commits: its lead before
        // the trigger, 7 s without command speech, then 30 s from the command's start.
        let trigger = Sample(160_000);
        let start = TurnSpec::reply_start(trigger, Sample(0));
        let command = trigger + policy.reply_no_input;
        let len = (command + policy.max_extended_utterance).0 - start.0;
        let quiet = vec![0; (command.0 - start.0) as usize];
        let pcm = spoken_span(&m, len, quiet, 9);
        let redecode = SpanJob::Redecode {
            turn: uuid::Uuid::new_v4(),
            pcm: Arc::clone(&pcm),
        };
        let text = redecoded_text(decode_span(&span, &m, redecode).await.unwrap());
        assert_eq!(
            text.matches("turn on the kitchen light").count(),
            9,
            "{text}"
        );
        let copy = SpanJob::SttCopy {
            turn: uuid::Uuid::new_v4(),
            pcm,
            gain: SttGain::Nominal,
            denoise: false,
        };
        let copied = stt_copy_len(decode_span(&span, &m, copy).await.unwrap());
        assert_eq!(copied as u64, len + 8_000);
        workers.shutdown(PATIENCE).await.unwrap();
    }

    #[tokio::test]
    async fn a_superseded_decode_leaves_room_for_the_next_commit_and_more_is_overload() {
        let limits = WorkerLimits::default();
        let workers =
            SpeechWorkers::start("kitchen", limits, CancellationToken::new(), test_watch())
                .unwrap();
        let span = workers.handle(SpeechWorkerKind::SpanDecoding);
        let largest = longest_span_job_bytes();

        let excessive = scope();
        assert!(matches!(
            span.submit(excessive.clone(), limits.span_bytes as usize + 1, |_| ()),
            Err(RuntimeError::Overloaded)
        ));
        assert!(excessive.is_cancelled());

        // The superseded turn's decode keeps running, and its credits, after its scope
        // is cancelled.
        let superseded = scope();
        let (running, started, release) = submit_blocking(&span, superseded.clone(), largest, 1);
        started.await.unwrap();
        superseded.cancel();
        assert_eq!(running.result().await, Err(RuntimeError::Cancelled));

        // The next turn's commit still fits; a third at once is overload, and only its
        // submitter is cancelled.
        let next = scope();
        let queued = span.submit(next.clone(), largest, |_| 2_u32).unwrap();
        let third = scope();
        assert!(matches!(
            span.submit(third.clone(), largest, |_| 3_u32),
            Err(RuntimeError::Overloaded)
        ));
        assert!(third.is_cancelled());
        assert!(!next.is_cancelled());

        release.send(()).unwrap();
        assert_eq!(queued.result().await, Ok(2));
        // Every credit came back.
        let whole = span
            .submit(scope(), limits.span_bytes as usize, |_| 4_u32)
            .unwrap();
        assert_eq!(whole.result().await, Ok(4));
        workers.shutdown(PATIENCE).await.unwrap();
    }

    #[tokio::test]
    async fn a_panic_is_reported_and_stops_every_worker() {
        let stop = CancellationToken::new();
        let workers = SpeechWorkers::start("kitchen", roomy(), stop.clone(), test_watch()).unwrap();
        let mut failure = workers.failure();
        assert_eq!(*failure.borrow(), None);
        let verify = workers.handle(SpeechWorkerKind::WakeVerification);

        let (started_tx, started) = oneshot::channel();
        let (release, gate) = std_mpsc::channel::<()>();
        let panicking = verify
            .submit(scope(), 0, move |_| -> u32 {
                let _ = started_tx.send(());
                let _ = gate.recv();
                panic!("injected speech worker panic");
            })
            .unwrap();
        started.await.unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let behind = verify
            .submit(scope(), 0, move |_| flag.store(true, Ordering::SeqCst))
            .unwrap();

        release.send(()).unwrap();
        assert_eq!(panicking.result().await, Err(RuntimeError::WorkerPanicked));
        assert_eq!(behind.result().await, Err(RuntimeError::Closed));
        let published = timeout(PATIENCE, failure.wait_for(Option::is_some))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*published, Some(RuntimeError::WorkerPanicked));
        drop(published);
        assert!(stop.is_cancelled());
        assert!(!ran.load(Ordering::SeqCst));
        assert!(matches!(
            verify.submit(scope(), 0, |_| ()),
            Err(RuntimeError::Closed)
        ));
        let mic = workers.handle(SpeechWorkerKind::MicrophoneAnalysis);
        assert!(matches!(
            mic.submit(scope(), 0, |_| ()),
            Err(RuntimeError::Closed)
        ));
        workers.shutdown(PATIENCE).await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_wakes_idle_workers_while_handles_are_still_held() {
        let stop = CancellationToken::new();
        let workers = SpeechWorkers::start("kitchen", roomy(), stop.clone(), test_watch()).unwrap();
        let held = PerWorker::from_fn(|kind| workers.handle(kind));
        assert_eq!(workers.shutdown(PATIENCE).await, Ok(()));
        assert!(stop.is_cancelled());
        PerWorker::from_fn(|kind| {
            assert!(matches!(
                held[kind].submit(scope(), 0, |_| ()),
                Err(RuntimeError::Closed)
            ));
        });
    }

    #[tokio::test]
    async fn a_job_asking_for_another_state_type_fails_the_worker_instead_of_replacing_it() {
        let workers =
            SpeechWorkers::start("kitchen", roomy(), CancellationToken::new(), test_watch())
                .unwrap();
        let mic = workers.handle(SpeechWorkerKind::MicrophoneAnalysis);
        let first = mic
            .submit(scope(), 0, |slot| *slot.state(|| 7_u32))
            .unwrap();
        assert_eq!(first.result().await, Ok(7));
        let other = mic
            .submit(scope(), 0, |slot| slot.state(String::new).len())
            .unwrap();
        assert_eq!(other.result().await, Err(RuntimeError::WorkerPanicked));
        assert_eq!(workers.shutdown(PATIENCE).await, Ok(()));
    }

    #[tokio::test]
    async fn shutdown_reports_a_worker_stuck_in_a_job() {
        let workers =
            SpeechWorkers::start("kitchen", roomy(), CancellationToken::new(), test_watch())
                .unwrap();
        let mic = workers.handle(SpeechWorkerKind::MicrophoneAnalysis);
        let (stuck, started, release) = submit_blocking(&mic, scope(), 0, 1);
        started.await.unwrap();
        assert_eq!(
            workers.shutdown(Duration::from_millis(50)).await,
            Err(RuntimeError::ShutdownTimeout)
        );
        assert_eq!(stuck.result().await, Err(RuntimeError::Closed));
        // Let the detached thread finish so it does not outlive the test binary's work.
        release.send(()).unwrap();
    }

    #[tokio::test]
    async fn dropping_the_workers_cancels_the_stop_token() {
        let stop = CancellationToken::new();
        let workers = SpeechWorkers::start("kitchen", roomy(), stop.clone(), test_watch()).unwrap();
        let mic = workers.handle(SpeechWorkerKind::MicrophoneAnalysis);
        drop(workers);
        assert!(stop.is_cancelled());
        assert!(matches!(
            mic.submit(scope(), 0, |_| ()),
            Err(RuntimeError::Closed)
        ));
    }

    #[test]
    fn invalid_limits_start_nothing() {
        let stop = CancellationToken::new();
        let refused = SpeechWorkers::start("kitchen", limits(0, 1024), stop.clone(), test_watch());
        assert!(matches!(refused, Err(RuntimeError::InvalidLimits)));
        let no_span_credits = WorkerLimits {
            span_bytes: 0,
            ..roomy()
        };
        let refused = SpeechWorkers::start("kitchen", no_span_credits, stop.clone(), test_watch());
        assert!(matches!(refused, Err(RuntimeError::InvalidLimits)));
        assert!(!stop.is_cancelled());
    }

    #[test]
    fn a_failed_start_stops_and_joins_the_workers_already_started() {
        let stop = CancellationToken::new();
        let exited = Arc::new(AtomicUsize::new(0));
        let mut spawned = 0;
        let mut spawn = |builder: thread::Builder, body: ThreadBody| {
            spawned += 1;
            if spawned == 3 {
                return Err(io::Error::other("injected spawn failure"));
            }
            let exited = Arc::clone(&exited);
            builder.spawn(move || {
                body();
                exited.fetch_add(1, Ordering::SeqCst);
            })
        };
        let refused =
            SpeechWorkers::start_with("kitchen", roomy(), stop.clone(), test_watch(), &mut spawn);
        match refused {
            Err(RuntimeError::ThreadStart(_)) => {}
            Err(other) => panic!("unexpected error {other}"),
            Ok(_) => panic!("start succeeded"),
        }
        assert!(stop.is_cancelled());
        assert_eq!(exited.load(Ordering::SeqCst), 2);
    }
}
