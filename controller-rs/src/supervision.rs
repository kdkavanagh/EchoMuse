//! What the controller does when something fails (docs/rust-controller-evaluation.md
//! §5.5, §5.7 step 5).
//!
//! Three kinds of failure are handled here:
//!
//! - **A shared service fails.** Tasks of the shared services are spawned through a
//!   [`TaskSupervisor`]: a panic in one is never lost in a dropped join handle, and
//!   neither is the `Err` a service that reports its own failure returns. Either fires
//!   the [`FatalHook`] at once, which in production logs the reason and ends the process
//!   with [`FATAL_EXIT_CODE`] so the container restarts it.
//! - **A native call does not return.** The [`Watchdog`] is one thread outside every
//!   device runtime. Each speech worker tells it when a job's call starts and ends. A
//!   call still running after the ordinary call deadline marks that device's speech path
//!   unavailable (its result is rejected as [`RuntimeError::Overdue`] when it returns, and
//!   the path is available again); a call still running after the recovery deadline fires
//!   the fatal hook. A stuck native call cannot be stopped from Rust, so the controller
//!   never detaches the thread and carries on.
//! - **A device runtime fails** (a speech worker or runtime task panicked, or its queues
//!   broke). The registry rebuilds it after a [`RestartBackoff`] delay: 1 s at first,
//!   doubling to 30 s, back to 1 s once a runtime has run for a stable minute.
//!
//! Each device's [`DeviceHealth`] (its availability and its error window) is published
//! in its snapshot and from there to the dashboard.

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fmt,
    future::Future,
    panic::{self, AssertUnwindSafe},
    pin::Pin,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    task::{Context, Poll},
    thread,
    time::{Duration, Instant, SystemTime},
};

use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

use echomuse_core::types::{DeviceId, RuntimeError, RuntimeId};

use crate::speech_workers::SpeechWorkerKind;

/// The exit status of a controller that stopped itself because it could not recover
/// (`EX_SOFTWARE`). Any non-zero status makes the container restart it.
pub const FATAL_EXIT_CODE: i32 = 70;

/// What happens when the controller cannot recover: a native call that never returns, a
/// panic in a shared service, a poisoned shared lock, a worker that cannot be stopped.
///
/// The production hook ([`FatalHook::exit_process`]) logs the reason and ends the process
/// at once. Tests install a hook that records the reason instead.
#[derive(Clone)]
pub struct FatalHook(Arc<dyn Fn(&str) + Send + Sync>);

impl FatalHook {
    /// Logs the reason, then ends the process with [`FATAL_EXIT_CODE`] without running
    /// destructors or exit handlers: native code may still be running on another thread,
    /// and tearing its libraries down under it could hang the exit.
    pub fn exit_process() -> Self {
        Self(Arc::new(|reason| exit_now(reason)))
    }

    /// A hook that runs `hook` with the reason. It may return; callers then carry on as
    /// if the process had stopped (nothing they guard is used again).
    pub fn new(hook: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self(Arc::new(hook))
    }

    /// Reports an unrecoverable failure.
    pub fn fire(&self, reason: &str) {
        (self.0)(reason);
    }
}

impl Default for FatalHook {
    fn default() -> Self {
        Self::exit_process()
    }
}

impl fmt::Debug for FatalHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FatalHook")
    }
}

fn exit_now(reason: &str) -> ! {
    tracing::error!(
        reason,
        exit_status = FATAL_EXIT_CODE,
        "the controller cannot recover and exits so its container restarts it"
    );
    // stderr is unbuffered, so this line is out before the process ends.
    eprintln!(
        "echomuse-controller: unrecoverable failure, exiting with status {FATAL_EXIT_CODE}: {reason}"
    );
    // SAFETY: `_exit` takes no pointers and only ends the process. Skipping exit
    // handlers is the point: a native library's static destructors must not run while
    // one of its calls is still in progress on another thread.
    unsafe { libc::_exit(FATAL_EXIT_CODE) }
}

/// The text of a panic payload, for logs.
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic without a message".to_owned()
    }
}

/// Polls a future and turns a panic inside it into an error.
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.get_mut().0.as_mut();
        match panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

/// Spawns the shared services' tasks so that none of them can fail unseen. Cheap to
/// clone.
#[derive(Clone, Debug, Default)]
pub struct TaskSupervisor {
    fatal: FatalHook,
}

impl TaskSupervisor {
    pub fn new(fatal: FatalHook) -> Self {
        Self { fatal }
    }

    /// The hook a failure the supervisor sees fires.
    pub fn fatal(&self) -> &FatalHook {
        &self.fatal
    }

    /// Spawns `task`, a shared service's work, on the current Tokio runtime. A panic in
    /// it fires the fatal hook, naming `name`: a shared service that panicked has left
    /// its state unknown, so the process stops (§5.5). The returned handle still reports
    /// the panic to whoever waits on it. A task that reports its own failure as an `Err`
    /// is spawned with [`spawn_fallible`](Self::spawn_fallible) instead.
    pub fn spawn<F>(&self, name: &'static str, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let fatal = self.fatal.clone();
        tokio::spawn(async move {
            match CatchUnwind(Box::pin(task)).await {
                Ok(value) => value,
                Err(payload) => {
                    let reason = format!(
                        "the shared service task \"{name}\" panicked: {}",
                        panic_message(payload.as_ref())
                    );
                    fatal.fire(&reason);
                    panic::resume_unwind(payload)
                }
            }
        })
    }

    /// Spawns `task` as [`spawn`](Self::spawn) does, for a shared service that ends
    /// with `Ok` when it is stopped on purpose and returns `Err` when it fails. An `Err`
    /// fires the fatal hook the moment the task returns it, naming `name` and the error:
    /// the service is gone, and nothing restarts a shared service but the process
    /// (§5.5). The returned handle still gives the `Err` to whoever waits on it.
    pub fn spawn_fallible<F, T, E>(&self, name: &'static str, task: F) -> JoinHandle<Result<T, E>>
    where
        F: Future<Output = Result<T, E>> + Send + 'static,
        T: Send + 'static,
        E: fmt::Display + Send + 'static,
    {
        let fatal = self.fatal.clone();
        self.spawn(name, async move {
            let outcome = task.await;
            if let Err(error) = &outcome {
                fatal.fire(&format!(
                    "the shared service task \"{name}\" failed: {error}"
                ));
            }
            outcome
        })
    }

    /// Watches a task another crate spawned (`handle`) the same way as
    /// [`spawn`](Self::spawn): a panic in it fires the fatal hook, naming `name`. The
    /// returned handle stands in for `handle`, giving its value.
    pub fn observe<T: Send + 'static>(
        &self,
        name: &'static str,
        handle: JoinHandle<T>,
    ) -> JoinHandle<T> {
        self.spawn(name, joined(handle))
    }

    /// Watches a task another crate spawned (`handle`) the same way as
    /// [`spawn_fallible`](Self::spawn_fallible): a panic in it, or an `Err` it returns,
    /// fires the fatal hook, naming `name`. The returned handle stands in for `handle`,
    /// giving its value.
    pub fn observe_fallible<T, E>(
        &self,
        name: &'static str,
        handle: JoinHandle<Result<T, E>>,
    ) -> JoinHandle<Result<T, E>>
    where
        T: Send + 'static,
        E: fmt::Display + Send + 'static,
    {
        self.spawn_fallible(name, joined(handle))
    }
}

/// The value of a task another crate spawned. Its panic resumes here, so the watcher
/// that awaits this sees it as its own.
async fn joined<T>(handle: JoinHandle<T>) -> T {
    match handle.await {
        Ok(value) => value,
        Err(error) if error.is_panic() => panic::resume_unwind(error.into_panic()),
        // Only the runtime's own shutdown cancels a task nobody else can abort, and that
        // drops this watcher too.
        Err(_) => std::future::pending().await,
    }
}

/// Tasks a device runtime spawns outside its own join set (its ESPHome nodes' servers):
/// a panic in one is sent to the runtime, which then fails and is rebuilt.
#[derive(Clone, Debug)]
pub struct RuntimeTasks {
    failures: mpsc::UnboundedSender<String>,
}

impl RuntimeTasks {
    /// A spawner and the receiver its panics arrive on.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (failures, receiver) = mpsc::unbounded_channel();
        (Self { failures }, receiver)
    }

    /// Spawns `task` on the current runtime. A panic in it is reported to the runtime,
    /// and the returned handle still reports it to whoever waits on it.
    pub fn spawn<F>(&self, name: &'static str, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let failures = self.failures.clone();
        tokio::spawn(async move {
            match CatchUnwind(Box::pin(task)).await {
                Ok(value) => value,
                Err(payload) => {
                    let reason = format!(
                        "the device task \"{name}\" panicked: {}",
                        panic_message(payload.as_ref())
                    );
                    // A runtime that is gone has nothing left to fail.
                    let _ = failures.send(reason);
                    panic::resume_unwind(payload)
                }
            }
        })
    }
}

// ── Health ──────────────────────────────────────────────────────────────────────────

/// Whether a device can be served now.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Availability {
    /// Its runtime serves it and its speech path works.
    #[default]
    Available,
    /// Its runtime serves it, but its speech path does not work: wakes and button turns
    /// are refused as `speech_unavailable` until it does again.
    SpeechUnavailable { reason: String },
    /// Its runtime failed and is rebuilt after `retry_in`; its sessions are refused
    /// until then.
    Restarting {
        reason: String,
        /// The delay chosen when it failed (the restart backoff).
        retry_in: Duration,
    },
}

impl Availability {
    /// Why it is unavailable; `None` when it is available.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Available => None,
            Self::SpeechUnavailable { reason } | Self::Restarting { reason, .. } => Some(reason),
        }
    }
}

/// One failure in a device's error window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthFailure {
    pub reason: String,
    /// Wall time, for display.
    pub at: SystemTime,
}

/// A device's failures since it last ran a stable minute without one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ErrorWindow {
    /// Failures in the current window: runtime failures and speech-path stalls.
    pub failures: u32,
    /// The latest failure, kept after its window closes; `None` if there was none.
    pub last: Option<HealthFailure>,
}

impl ErrorWindow {
    /// Counts a failure.
    pub fn record(&mut self, reason: &str) {
        self.failures = self.failures.saturating_add(1);
        self.last = Some(HealthFailure {
            reason: reason.to_owned(),
            at: SystemTime::now(),
        });
    }

    /// The device ran a stable minute: a new window starts. The last failure stays.
    pub fn close(&mut self) {
        self.failures = 0;
    }
}

/// A device's health (§5.5): its own availability and error window, independent of
/// every other device's.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeviceHealth {
    pub availability: Availability,
    pub errors: ErrorWindow,
}

// ── Restart backoff ─────────────────────────────────────────────────────────────────

/// How a failed device runtime is rebuilt (§5.7 step 5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartPolicy {
    /// The delay before the first rebuild.
    pub initial: Duration,
    /// The longest delay; each failure doubles the last one up to this.
    pub max: Duration,
    /// How long a runtime must run without failing for the delay to start over.
    pub stable: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            stable: Duration::from_secs(60),
        }
    }
}

impl RestartPolicy {
    pub fn validate(self) -> Result<Self, RuntimeError> {
        if self.initial.is_zero() || self.max < self.initial || self.stable.is_zero() {
            return Err(RuntimeError::InvalidLimits);
        }
        Ok(self)
    }
}

/// The delays between rebuilds of one device's runtime.
#[derive(Clone, Debug)]
pub struct RestartBackoff {
    policy: RestartPolicy,
    /// Failures since the last stable run.
    failures: u32,
}

impl RestartBackoff {
    pub fn new(policy: RestartPolicy) -> Self {
        Self {
            policy,
            failures: 0,
        }
    }

    /// A runtime failed after running for `ran_for`: the delay before rebuilding it. A
    /// run of at least the stable time starts the doubling over.
    pub fn failed(&mut self, ran_for: Duration) -> Duration {
        if ran_for >= self.policy.stable {
            self.failures = 0;
        }
        let doubled = 2_u32
            .checked_pow(self.failures)
            .and_then(|factor| self.policy.initial.checked_mul(factor))
            .unwrap_or(self.policy.max);
        self.failures = self.failures.saturating_add(1);
        doubled.min(self.policy.max)
    }

    /// The runtime ran the stable time: the next failure waits the initial delay.
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    /// Failures since the last stable run.
    pub fn failures(&self) -> u32 {
        self.failures
    }
}

// ── The native-call watchdog ────────────────────────────────────────────────────────

/// The watchdog's deadlines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WatchdogConfig {
    /// A call running longer than this marks its device's speech path unavailable, and
    /// its result is rejected when it returns. Longer than any one model call takes: a
    /// cell or a verification takes milliseconds, and a commit re-decode of the longest
    /// turn about a second on the slowest supported host.
    pub call_deadline: Duration,
    /// A call running longer than this ends the process (§5.5).
    pub recovery_deadline: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            call_deadline: Duration::from_secs(10),
            recovery_deadline: Duration::from_secs(30),
        }
    }
}

impl WatchdogConfig {
    pub fn validate(self) -> Result<Self, RuntimeError> {
        if self.call_deadline.is_zero() || self.recovery_deadline <= self.call_deadline {
            return Err(RuntimeError::InvalidLimits);
        }
        Ok(self)
    }
}

/// Which of a device's speech workers are inside a call past the call deadline. Empty
/// while the speech path works.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SpeechPathState {
    pub overdue: BTreeSet<SpeechWorkerKind>,
}

impl SpeechPathState {
    /// Why the speech path is unavailable; `None` while it works.
    pub fn unavailable(&self) -> Option<String> {
        let names: Vec<&str> = self.overdue.iter().map(|kind| kind.label()).collect();
        match names.as_slice() {
            [] => None,
            [one] => Some(format!(
                "the {one} speech worker is stuck in a model call past its deadline"
            )),
            many => Some(format!(
                "the {} speech workers are stuck in model calls past their deadline",
                many.join(", ")
            )),
        }
    }
}

/// Whose worker a call runs on, for the speech-path state and the log.
#[derive(Clone, Debug)]
pub struct WorkerOwner {
    pub device: DeviceId,
    pub runtime: RuntimeId,
}

/// Watches native calls from outside the device runtimes. Cheap to clone; the watching
/// thread ends when the last clone is dropped.
#[derive(Clone)]
pub struct Watchdog {
    owner: Arc<WatchdogOwner>,
}

struct WatchdogOwner {
    shared: Arc<WatchShared>,
}

impl Drop for WatchdogOwner {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.stopped = true;
        drop(state);
        self.shared.wake.notify_all();
    }
}

struct WatchShared {
    state: Mutex<WatchState>,
    wake: Condvar,
    config: WatchdogConfig,
    fatal: FatalHook,
}

#[derive(Default)]
struct WatchState {
    next: u64,
    calls: BTreeMap<u64, Call>,
    stopped: bool,
}

struct Call {
    owner: WorkerOwner,
    kind: SpeechWorkerKind,
    started: Instant,
    path: Arc<watch::Sender<SpeechPathState>>,
    /// The call deadline passed and the speech path was marked unavailable.
    overdue: bool,
    /// The recovery deadline passed and the fatal hook fired.
    fatal: bool,
}

impl WatchShared {
    /// The state; a poisoned lock is a shared lock poisoned by a panic (§5.5).
    fn lock(&self) -> MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            self.fatal
                .fire("the native-call watchdog's lock was poisoned by a panic");
            poisoned.into_inner()
        })
    }
}

impl fmt::Debug for Watchdog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watchdog")
            .field("config", &self.owner.shared.config)
            .finish_non_exhaustive()
    }
}

impl Watchdog {
    /// Starts the watching thread.
    pub fn start(config: WatchdogConfig, fatal: FatalHook) -> Result<Self, RuntimeError> {
        let config = config.validate()?;
        let shared = Arc::new(WatchShared {
            state: Mutex::new(WatchState::default()),
            wake: Condvar::new(),
            config,
            fatal,
        });
        let watched = Arc::clone(&shared);
        thread::Builder::new()
            .name("native-watchdog".to_owned())
            .spawn(move || watch_calls(&watched))
            .map_err(|error| RuntimeError::ThreadStart(format!("native-call watchdog: {error}")))?;
        Ok(Self {
            owner: Arc::new(WatchdogOwner { shared }),
        })
    }

    pub fn config(&self) -> WatchdogConfig {
        self.owner.shared.config
    }

    /// A call starts on `kind`'s worker of `owner`; it ends when the guard is finished
    /// or dropped. `path` is that device's speech-path state.
    pub fn begin(
        &self,
        owner: &WorkerOwner,
        kind: SpeechWorkerKind,
        path: &Arc<watch::Sender<SpeechPathState>>,
    ) -> CallGuard {
        let shared = &self.owner.shared;
        let started = Instant::now();
        let mut state = shared.lock();
        state.next = state.next.wrapping_add(1);
        let id = state.next;
        state.calls.insert(
            id,
            Call {
                owner: owner.clone(),
                kind,
                started,
                path: Arc::clone(path),
                overdue: false,
                fatal: false,
            },
        );
        drop(state);
        shared.wake.notify_all();
        CallGuard {
            shared: Arc::clone(shared),
            id,
            started,
            finished: false,
        }
    }
}

/// One call in progress. Finishing or dropping it ends the call.
pub struct CallGuard {
    shared: Arc<WatchShared>,
    id: u64,
    started: Instant,
    finished: bool,
}

impl CallGuard {
    /// Ends the call; `true` when it ran past the call deadline, so its result is late
    /// and must be rejected.
    pub fn finish(mut self) -> bool {
        self.end();
        self.started.elapsed() > self.shared.config.call_deadline
    }

    fn end(&mut self) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        let mut state = self.shared.lock();
        if let Some(call) = state.calls.remove(&self.id)
            && call.overdue
        {
            // The worker is back: the speech path works again unless another worker of
            // the device is still stuck.
            call.path.send_modify(|path| {
                path.overdue.remove(&call.kind);
            });
        }
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        self.end();
    }
}

fn watch_calls(shared: &WatchShared) {
    let config = shared.config;
    let mut state = shared.lock();
    loop {
        if state.stopped {
            return;
        }
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let mut fatal = Vec::new();
        for call in state.calls.values_mut() {
            if call.fatal {
                continue;
            }
            let recovery = call.started + config.recovery_deadline;
            if now >= recovery {
                call.fatal = true;
                fatal.push(format!(
                    "device {} (runtime {}): the {} speech worker has been inside one model \
                     call for more than {} s; a native call cannot be stopped, so the controller \
                     must restart",
                    call.owner.device,
                    call.owner.runtime.0,
                    call.kind.label(),
                    config.recovery_deadline.as_secs_f64()
                ));
                continue;
            }
            let due = if call.overdue {
                recovery
            } else {
                let ordinary = call.started + config.call_deadline;
                if now >= ordinary {
                    call.overdue = true;
                    tracing::warn!(
                        device = %call.owner.device,
                        runtime = call.owner.runtime.0,
                        worker = call.kind.name(),
                        "a speech worker call passed its deadline; the device refuses wakes until it returns"
                    );
                    let kind = call.kind;
                    call.path.send_modify(|path| {
                        path.overdue.insert(kind);
                    });
                    recovery
                } else {
                    ordinary
                }
            };
            next = Some(next.map_or(due, |current| current.min(due)));
        }
        if !fatal.is_empty() {
            drop(state);
            for reason in &fatal {
                shared.fatal.fire(reason);
            }
            state = shared.lock();
            continue;
        }
        const POISONED: &str = "the native-call watchdog's lock was poisoned by a panic";
        state = match next {
            Some(due) => match shared
                .wake
                .wait_timeout(state, due.saturating_duration_since(now))
            {
                Ok((guard, _)) => guard,
                Err(poisoned) => {
                    shared.fatal.fire(POISONED);
                    poisoned.into_inner().0
                }
            },
            None => match shared.wake.wait(state) {
                Ok(guard) => guard,
                Err(poisoned) => {
                    shared.fatal.fire(POISONED);
                    poisoned.into_inner()
                }
            },
        };
    }
}

// ── The bundle the runtimes are given ───────────────────────────────────────────────

/// What a controller is told about failure handling.
#[derive(Clone, Debug, Default)]
pub struct SupervisionOptions {
    /// What an unrecoverable failure does; production ends the process.
    pub fatal: FatalHook,
    pub watchdog: WatchdogConfig,
    pub restart: RestartPolicy,
}

/// Failure handling as the registry and the device runtimes use it. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Supervision {
    pub tasks: TaskSupervisor,
    pub watchdog: Watchdog,
    pub restart: RestartPolicy,
}

impl Supervision {
    /// Starts the watchdog thread.
    pub fn start(options: SupervisionOptions) -> Result<Self, RuntimeError> {
        let restart = options.restart.validate()?;
        let watchdog = Watchdog::start(options.watchdog, options.fatal.clone())?;
        Ok(Self {
            tasks: TaskSupervisor::new(options.fatal),
            watchdog,
            restart,
        })
    }

    pub fn fatal(&self) -> &FatalHook {
        self.tasks.fatal()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc as std_mpsc;

    use super::*;

    fn recorder() -> (FatalHook, std_mpsc::Receiver<String>) {
        let (sender, reasons) = std_mpsc::channel();
        let sender = Mutex::new(sender);
        let hook = FatalHook::new(move |reason| {
            let _ = sender.lock().unwrap().send(reason.to_owned());
        });
        (hook, reasons)
    }

    #[test]
    fn backoff_doubles_to_its_ceiling_and_starts_over_after_a_stable_run() {
        let mut backoff = RestartBackoff::new(RestartPolicy::default());
        let quick = Duration::from_secs(2);
        let delays: Vec<u64> = (0..8).map(|_| backoff.failed(quick).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30, 30]);
        // A run of a stable minute: the next failure waits the first delay again.
        assert_eq!(
            backoff.failed(Duration::from_secs(60)),
            Duration::from_secs(1)
        );
        assert_eq!(backoff.failed(quick), Duration::from_secs(2));
        backoff.reset();
        assert_eq!(backoff.failures(), 0);
        assert_eq!(backoff.failed(quick), Duration::from_secs(1));
    }

    #[test]
    fn backoff_never_overflows() {
        let mut backoff = RestartBackoff::new(RestartPolicy::default());
        for _ in 0..200 {
            assert!(backoff.failed(Duration::ZERO) <= Duration::from_secs(30));
        }
    }

    #[test]
    fn invalid_policies_and_deadlines_are_refused() {
        let zero = RestartPolicy {
            initial: Duration::ZERO,
            ..RestartPolicy::default()
        };
        assert_eq!(zero.validate(), Err(RuntimeError::InvalidLimits));
        let inverted = WatchdogConfig {
            call_deadline: Duration::from_secs(30),
            recovery_deadline: Duration::from_secs(10),
        };
        assert_eq!(inverted.validate(), Err(RuntimeError::InvalidLimits));
    }

    #[test]
    fn the_error_window_counts_until_a_stable_run_and_keeps_the_last_failure() {
        let mut window = ErrorWindow::default();
        window.record("first");
        window.record("second");
        assert_eq!(window.failures, 2);
        window.close();
        assert_eq!(window.failures, 0);
        assert_eq!(
            window.last.map(|last| last.reason).as_deref(),
            Some("second")
        );
    }

    fn owner() -> WorkerOwner {
        WorkerOwner {
            device: DeviceId("kitchen".to_owned()),
            runtime: RuntimeId(7),
        }
    }

    #[test]
    fn an_overdue_call_marks_the_speech_path_until_it_returns() {
        let (fatal, reasons) = recorder();
        let watchdog = Watchdog::start(
            WatchdogConfig {
                call_deadline: Duration::from_millis(50),
                recovery_deadline: Duration::from_secs(60),
            },
            fatal,
        )
        .unwrap();
        let path = Arc::new(watch::Sender::new(SpeechPathState::default()));
        let mut seen = path.subscribe();
        let guard = watchdog.begin(&owner(), SpeechWorkerKind::WakeVerification, &path);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                seen.wait_for(|state| !state.overdue.is_empty()),
            )
            .await
            .unwrap()
            .unwrap();
        });
        assert_eq!(
            path.borrow().unavailable().as_deref(),
            Some("the wake verification speech worker is stuck in a model call past its deadline")
        );
        // The late result is rejected, and the path works again.
        assert!(guard.finish());
        assert_eq!(*path.borrow(), SpeechPathState::default());
        assert!(reasons.try_recv().is_err());
        // A quick call is not late.
        let quick = watchdog.begin(&owner(), SpeechWorkerKind::MicrophoneAnalysis, &path);
        assert!(!quick.finish());
    }

    #[test]
    fn a_call_past_the_recovery_deadline_fires_the_fatal_hook_once() {
        let (fatal, reasons) = recorder();
        let watchdog = Watchdog::start(
            WatchdogConfig {
                call_deadline: Duration::from_millis(20),
                recovery_deadline: Duration::from_millis(80),
            },
            fatal,
        )
        .unwrap();
        let path = Arc::new(watch::Sender::new(SpeechPathState::default()));
        let guard = watchdog.begin(&owner(), SpeechWorkerKind::SpanDecoding, &path);
        let reason = reasons.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(reason.contains("device kitchen"), "{reason}");
        assert!(reason.contains("span decoding"), "{reason}");
        assert!(
            reasons.recv_timeout(Duration::from_millis(200)).is_err(),
            "fired once"
        );
        drop(guard);
    }

    #[tokio::test]
    async fn a_panicking_shared_task_fires_the_fatal_hook() {
        let (fatal, reasons) = recorder();
        let tasks = TaskSupervisor::new(fatal);
        let handle = tasks.spawn("test service", async {
            panic!("injected shared-service panic");
        });
        let joined = handle.await;
        assert!(joined.is_err_and(|error| error.is_panic()));
        let reason = reasons.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(reason.contains("test service"), "{reason}");
        assert!(reason.contains("injected shared-service panic"), "{reason}");
        // A task that finishes reports nothing.
        assert_eq!(tasks.spawn("quiet", async { 3 }).await.unwrap(), 3);
        assert!(reasons.try_recv().is_err());
    }

    /// The next reason the hook records, waited for off the runtime's thread, so the
    /// test sees the hook fire while nobody awaits the task's handle.
    async fn next_reason(
        reasons: std_mpsc::Receiver<String>,
    ) -> (String, std_mpsc::Receiver<String>) {
        tokio::task::spawn_blocking(move || {
            let reason = reasons
                .recv_timeout(Duration::from_secs(5))
                .expect("the fatal hook fired");
            (reason, reasons)
        })
        .await
        .unwrap()
    }

    async fn panics() -> Result<(), String> {
        panic!("injected observed panic")
    }

    #[tokio::test]
    async fn a_shared_service_that_returns_an_error_fires_the_fatal_hook_at_once() {
        let (fatal, reasons) = recorder();
        let tasks = TaskSupervisor::new(fatal);
        let handle = tasks.spawn_fallible("test service", async {
            Err::<(), _>("injected service failure".to_owned())
        });
        // The hook fires before anyone waits on the handle.
        let (reason, reasons) = next_reason(reasons).await;
        assert!(reason.contains("\"test service\" failed"), "{reason}");
        assert!(reason.contains("injected service failure"), "{reason}");
        // Whoever waits on the handle still gets the error itself.
        assert_eq!(
            handle.await.unwrap(),
            Err("injected service failure".to_owned())
        );
        // A service that ends with Ok reports nothing.
        assert_eq!(
            tasks
                .spawn_fallible("quiet", async { Ok::<_, String>(3) })
                .await
                .unwrap(),
            Ok(3)
        );
        assert!(reasons.try_recv().is_err(), "fired once");
    }

    #[tokio::test]
    async fn an_observed_service_fails_on_an_error_or_a_panic_but_not_on_a_clean_stop() {
        let (fatal, reasons) = recorder();
        let tasks = TaskSupervisor::new(fatal);

        // Stopped on purpose: its Ok reaches the caller and nothing fires.
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let quiet = tasks.observe_fallible(
            "quiet service",
            tokio::spawn(async move {
                let _ = stopped.await;
                Ok::<_, String>(7)
            }),
        );
        stop.send(()).unwrap();
        assert_eq!(quiet.await.unwrap(), Ok(7));
        assert!(reasons.try_recv().is_err());

        // A returned error fires while nobody waits on the handle, and still reaches
        // whoever waits on it afterwards.
        let failed = tasks.observe_fallible(
            "failing service",
            tokio::spawn(async { Err::<(), _>("injected observed failure".to_owned()) }),
        );
        let (reason, reasons) = next_reason(reasons).await;
        assert!(reason.contains("\"failing service\" failed"), "{reason}");
        assert!(reason.contains("injected observed failure"), "{reason}");
        assert_eq!(
            failed.await.unwrap(),
            Err("injected observed failure".to_owned())
        );

        // A panic fires as a panic, and the handle still reports it.
        let panicked = tasks.observe_fallible("panicking service", tokio::spawn(panics()));
        let (reason, reasons) = next_reason(reasons).await;
        assert!(
            reason.contains("\"panicking service\" panicked"),
            "{reason}"
        );
        assert!(reason.contains("injected observed panic"), "{reason}");
        assert!(panicked.await.is_err_and(|error| error.is_panic()));
        assert!(reasons.try_recv().is_err(), "each failure fired once");
    }

    #[tokio::test]
    async fn a_panicking_runtime_task_is_reported_to_its_runtime() {
        let (tasks, mut failures) = RuntimeTasks::channel();
        let handle = tasks.spawn("satellite", async {
            panic!("injected node panic");
        });
        assert!(handle.await.is_err_and(|error| error.is_panic()));
        let reason = failures.recv().await.unwrap();
        assert!(reason.contains("satellite"), "{reason}");
    }
}
