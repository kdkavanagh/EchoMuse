//! The alert service (§5.4): one task that owns every speaker's alert state, with a
//! calendar feed and a journal applier per speaker and one task per Home Assistant
//! connection. Device runtimes, the dashboard API and the integrator reach it through
//! [`AlertHandle`]; nothing else touches its state.
//!
//! # Wiring
//!
//! 1. Open the database ([`echomuse_db`]), start the HA client ([`echomuse_ha::HaClient`],
//!    which implements [`AlertHa`]), and implement [`AlertLink`] over the device sessions.
//! 2. [`AlertService::start`] with those and a clock, on the shared Tokio runtime; supervise
//!    the returned task: it ends with an error if any of the service's own tasks panicked.
//! 3. [`AlertHandle::register`] every approved device, and [`AlertHandle::update`] when
//!    its label or sounds change. Pending journaled operations of that speaker resume.
//! 4. Per session: [`AlertHandle::attach`] after `session.ready`, pass every
//!    `alert.ack`/`alert.local_operation`/`alert.ring_ended`/`alert.state` body to
//!    [`AlertHandle::device_message`], and [`AlertHandle::detach`] when it ends.
//! 5. Forward HA's timer events for the speaker with [`AlertHandle::timer_event`].
//!
//! A device runtime then calls [`AlertHandle::act`], [`AlertHandle::set_alarm`],
//! [`AlertHandle::cancel_alarms`], [`AlertHandle::list_alarms`] and reads
//! [`AlertHandle::rings`], which never blocks.

mod applier;
mod feed;
mod ops;
mod session;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    sync::Arc,
    time::Duration,
};

use echomuse_core::{
    services::{HaConnectionGeneration, TimerEvent, TimerEventKind, UnknownCause},
    types::{DeviceId, SessionId},
};
use echomuse_db::DbHandle;
use echomuse_ha::{CalendarItem, CalendarPush};
use echomuse_protocol::{
    Capability,
    alerts::{AlertSound, OccurrenceKind, TombstoneReason},
    vocab::AckStatus,
};
use serde_json::{Value, json};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task::{AbortHandle, JoinSet},
    time::Instant,
};
use uuid::Uuid;

use self::{
    applier::{Applier, ApplierJob},
    feed::{Feed, FeedControl, FeedEvent, FeedTiming},
    session::HaSession,
};
use crate::{
    api::{
        Act, ActOutcome, AlarmEntry, AlarmSet, AlertNotice, CancelAlarms, CancelOutcome,
        CancelTarget, DeviceAttach, DeviceRings, EndpointConfig, Outcome, Refusal, RingsSnapshot,
        ScriptState, SetAlarm, TimerViews,
    },
    calendar::{CalendarView, EventOrigin, Occurrence, occurrences},
    delivery::{Delivery, Desired, Outbox},
    description::{EchomuseLine, ParentRef, RingSettings, defaults},
    ids::{self, OpId},
    journal::{
        self, BindCalendar, Journaled, LoadDelivery, NewOp, OpRow, OpsById, PendingOps,
        RestoreGuard, SetDeliveryPosition, State, request_digest,
    },
    ports::{AlertClock, AlertHa, AlertLink, CommandOutcome, SessionRef},
    relay::{self, CancelSelector, RelayCall, RelayRequest, RelayShape},
    schedule::{HaTimeZone, TimeOfDay, local_text},
    timers::Timers,
    wire::{
        ActAction, ActSource, AlertAct, AlertInbound, AlertLocalOperation, AlertOpResult,
        AlertPrefetch, AlertRing, AlertState, CommandBody, LocalAction, LocalSource, OpResultState,
        PostBody, RingKind,
    },
};

/// The service's timing and limits. [`Default`] gives the spec's values.
#[derive(Clone, Copy, Debug)]
pub struct AlertConfig {
    /// How long a caller waits for an operation to be confirmed before it is told the
    /// operation is pending (the relay's 5 s).
    pub settle_wait: Duration,
    /// How long the applier waits for the calendar to show a write before forcing a fresh
    /// read, and again after it (10 s).
    pub confirm_wait: Duration,
    /// When a relay request is answered at the latest (5 s; the scripts wait 10 s).
    pub relay_answer: Duration,
    /// How long after its due time an occurrence may still ring (30 min).
    pub catch_up: Duration,
    /// How far ahead the calendar is followed (7 days).
    pub window_ahead: Duration,
    /// How far back the backlog is read (90 days).
    pub backlog: Duration,
    /// How long applied terminal operations guard against restores (90 days).
    pub restore_guard: Duration,
    /// How often the subscription is renewed to roll its window (1 h).
    pub resubscribe_every: Duration,
    /// Schedules a calendar may hold (256).
    pub max_schedules: usize,
    /// Occurrences delivered at most (4,096).
    pub max_occurrences: usize,
    /// How long after a timer copy reaches zero its `finished` may still come (5 s).
    pub timer_finish_grace: Duration,
    /// First wait before retrying Home Assistant (1 s, doubling).
    pub retry_initial: Duration,
    /// Longest wait between retries (30 s).
    pub retry_max: Duration,
    /// How many act outcomes and relay results are remembered for repeats.
    pub remembered: usize,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            settle_wait: Duration::from_secs(5),
            confirm_wait: Duration::from_secs(10),
            relay_answer: Duration::from_secs(5),
            catch_up: Duration::from_secs(30 * 60),
            window_ahead: Duration::from_secs(7 * 24 * 3600),
            backlog: Duration::from_secs(90 * 24 * 3600),
            restore_guard: Duration::from_secs(90 * 24 * 3600),
            resubscribe_every: Duration::from_secs(3600),
            max_schedules: 256,
            max_occurrences: 4096,
            timer_finish_grace: Duration::from_secs(5),
            retry_initial: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
            remembered: 256,
        }
    }
}

fn ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Why the service stopped.
#[derive(Debug)]
pub enum ServiceFailure {
    /// One of its tasks panicked; §5.5 stops the process for a shared service.
    TaskPanicked(String),
}

impl fmt::Display for ServiceFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceFailure::TaskPanicked(what) => write!(f, "alert service task panicked: {what}"),
        }
    }
}

impl std::error::Error for ServiceFailure {}

/// Where a finished operation's outcome goes.
pub(crate) enum Deliver {
    Set(oneshot::Sender<Outcome<AlarmSet>>),
    Cancel(oneshot::Sender<CancelOutcome>),
    Act(oneshot::Sender<ActOutcome>),
    Relay {
        request_id: String,
        shape: RelayShape,
    },
}

pub(crate) enum Msg {
    Register(EndpointConfig),
    Update(EndpointConfig),
    Remove(DeviceId),
    Attach(DeviceId, Box<DeviceAttach>),
    Detach(DeviceId, SessionId),
    Inbound(DeviceId, SessionId, AlertInbound),
    Timer(DeviceId, TimerEvent),
    IntentWait(DeviceId, bool),
    SetAlarm(SetAlarm, Deliver),
    Cancel(CancelAlarms, Deliver),
    Act(Act, Deliver),
    List(
        DeviceId,
        usize,
        oneshot::Sender<Result<Vec<AlarmEntry>, Refusal>>,
    ),
    Status(
        DeviceId,
        oneshot::Sender<Option<crate::status::EndpointStatus>>,
    ),
    HaUp(HaConnectionGeneration, Result<HaTimeZone, String>),
    HaDown,
    Feed(DeviceId, FeedEvent),
    Settled(DeviceId, OpRow),
    GuardDone(DeviceId, Uuid),
    ActAcked(OpId, CommandOutcome),
    WaitTimeout(u64),
    Relay(Value),
    RelayDeadline(String),
    Notice(AlertNotice),
    Shutdown,
}

/// How a device runtime, the API and the integrator reach the alert service. Cheap to
/// clone.
#[derive(Clone)]
pub struct AlertHandle {
    inbox: mpsc::UnboundedSender<Msg>,
    rings: watch::Receiver<Arc<RingsSnapshot>>,
    timers: watch::Receiver<Arc<TimerViews>>,
    scripts: watch::Receiver<Vec<(String, ScriptState)>>,
    notices: broadcast::Sender<AlertNotice>,
}

impl fmt::Debug for AlertHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlertHandle").finish_non_exhaustive()
    }
}

/// The service has stopped; nothing was done.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceStopped;

impl fmt::Display for ServiceStopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the alert service has stopped")
    }
}

impl std::error::Error for ServiceStopped {}

/// Holds a speaker's place among the turns waiting on an HA intent run, which the relay
/// uses to pick the speaker of a tool call that names none. Dropping it ends the wait.
#[must_use]
pub struct IntentWaitGuard {
    inbox: mpsc::UnboundedSender<Msg>,
    device: DeviceId,
}

impl Drop for IntentWaitGuard {
    fn drop(&mut self) {
        let _ = self.inbox.send(Msg::IntentWait(self.device.clone(), false));
    }
}

impl AlertHandle {
    fn send(&self, msg: Msg) -> Result<(), ServiceStopped> {
        self.inbox.send(msg).map_err(|_| ServiceStopped)
    }

    /// Serves a speaker (an approved device). Its pending journaled operations resume.
    pub fn register(&self, endpoint: EndpointConfig) -> Result<(), ServiceStopped> {
        self.send(Msg::Register(endpoint))
    }

    /// Changes a speaker's label, area or sounds. Its calendar stays the one it has.
    pub fn update(&self, endpoint: EndpointConfig) -> Result<(), ServiceStopped> {
        self.send(Msg::Update(endpoint))
    }

    /// Stops serving a speaker. Its journal rows and calendar stay.
    pub fn remove(&self, device: DeviceId) -> Result<(), ServiceStopped> {
        self.send(Msg::Remove(device))
    }

    /// A session of `device` received `session.ready`.
    pub fn attach(&self, device: DeviceId, attach: DeviceAttach) -> Result<(), ServiceStopped> {
        self.send(Msg::Attach(device, Box::new(attach)))
    }

    /// Session `session` of `device` ended.
    pub fn detach(&self, device: DeviceId, session: SessionId) -> Result<(), ServiceStopped> {
        self.send(Msg::Detach(device, session))
    }

    /// A device alert message of session `session`, in arrival order.
    pub fn device_message(
        &self,
        device: DeviceId,
        session: SessionId,
        message: AlertInbound,
    ) -> Result<(), ServiceStopped> {
        self.send(Msg::Inbound(device, session, message))
    }

    /// An HA timer event for `device`'s satellite.
    pub fn timer_event(&self, device: DeviceId, event: TimerEvent) -> Result<(), ServiceStopped> {
        self.send(Msg::Timer(device, event))
    }

    /// Marks a turn of `device` as waiting on an HA intent run until the guard drops.
    pub fn intent_wait(&self, device: DeviceId) -> Result<IntentWaitGuard, ServiceStopped> {
        self.send(Msg::IntentWait(device.clone(), true))?;
        Ok(IntentWaitGuard {
            inbox: self.inbox.clone(),
            device,
        })
    }

    /// Sets an alarm. Resolves once it is applied, rejected or refused, or after the
    /// settle wait as pending.
    pub async fn set_alarm(&self, request: SetAlarm) -> Result<Outcome<AlarmSet>, ServiceStopped> {
        let (reply, answer) = oneshot::channel();
        self.send(Msg::SetAlarm(request, Deliver::Set(reply)))?;
        answer.await.map_err(|_| ServiceStopped)
    }

    /// Cancels alarms, one journaled operation per schedule.
    pub async fn cancel_alarms(
        &self,
        request: CancelAlarms,
    ) -> Result<CancelOutcome, ServiceStopped> {
        let (reply, answer) = oneshot::channel();
        self.send(Msg::Cancel(request, Deliver::Cancel(reply)))?;
        answer.await.map_err(|_| ServiceStopped)
    }

    /// Stops or snoozes a ring, here or on another speaker. The service checks again,
    /// immediately before sending, that it is still what rings there.
    pub async fn act(&self, request: Act) -> Result<ActOutcome, ServiceStopped> {
        let (reply, answer) = oneshot::channel();
        self.send(Msg::Act(request, Deliver::Act(reply)))?;
        answer.await.map_err(|_| ServiceStopped)
    }

    /// The next `limit` alarms of a speaker, from its calendar as read on the current
    /// Home Assistant connection.
    pub async fn list_alarms(
        &self,
        device: DeviceId,
        limit: usize,
    ) -> Result<Result<Vec<AlarmEntry>, Refusal>, ServiceStopped> {
        let (reply, answer) = oneshot::channel();
        self.send(Msg::List(device, limit, reply))?;
        answer.await.map_err(|_| ServiceStopped)
    }

    /// The speaker's alert status for the dashboard; `None` when the service does not
    /// serve the device.
    pub async fn status(
        &self,
        device: DeviceId,
    ) -> Result<Option<crate::status::EndpointStatus>, ServiceStopped> {
        let (reply, answer) = oneshot::channel();
        self.send(Msg::Status(device, reply))?;
        answer.await.map_err(|_| ServiceStopped)
    }

    /// What rings where, updated on every `alert.state`. Never blocks.
    pub fn rings(&self) -> watch::Receiver<Arc<RingsSnapshot>> {
        self.rings.clone()
    }

    /// Every served speaker's running timers as its display copy shows them, by device
    /// id: republished whenever a timer starts, changes or ends, and every second while
    /// any runs. A speaker without timers has an empty view. Never blocks.
    pub fn timer_views(&self) -> watch::Receiver<Arc<TimerViews>> {
        self.timers.clone()
    }

    /// The LLM tool scripts' states after the latest installation pass.
    pub fn scripts(&self) -> watch::Receiver<Vec<(String, ScriptState)>> {
        self.scripts.clone()
    }

    /// Notices for the dashboard and logs.
    pub fn notices(&self) -> broadcast::Receiver<AlertNotice> {
        self.notices.subscribe()
    }

    /// Stops the service and its tasks.
    pub fn shutdown(&self) {
        let _ = self.inbox.send(Msg::Shutdown);
    }
}

/// The alert service.
pub struct AlertService;

impl AlertService {
    /// Starts the service on the current Tokio runtime. The task ends after
    /// [`AlertHandle::shutdown`], or with an error when one of its tasks panicked.
    pub fn start<H: AlertHa, L: AlertLink, C: AlertClock>(
        config: AlertConfig,
        db: DbHandle,
        ha: Arc<H>,
        link: Arc<L>,
        clock: Arc<C>,
    ) -> (
        AlertHandle,
        tokio::task::JoinHandle<Result<(), ServiceFailure>>,
    ) {
        let (inbox, messages) = mpsc::unbounded_channel();
        let (rings_tx, rings) = watch::channel(Arc::new(RingsSnapshot::default()));
        let (timers_tx, timers) = watch::channel(Arc::new(TimerViews::new()));
        let (scripts_tx, scripts) = watch::channel(Vec::new());
        let (notices, _) = broadcast::channel(256);
        let handle = AlertHandle {
            inbox: inbox.clone(),
            rings,
            timers,
            scripts,
            notices: notices.clone(),
        };
        let mut tasks = JoinSet::new();
        tasks.spawn(
            HaSession {
                ha: ha.clone(),
                db: db.clone(),
                status: ha.status(),
                actor: inbox.clone(),
                scripts: scripts_tx,
                retry_initial: config.retry_initial,
                retry_max: config.retry_max,
            }
            .run(),
        );
        let core = Core {
            config,
            db,
            ha,
            link,
            clock,
            inbox,
            tasks,
            endpoints: HashMap::new(),
            stashed: HashMap::new(),
            generation: None,
            zone: None,
            waits: HashMap::new(),
            wait_index: HashMap::new(),
            next_wait: 0,
            acts: HashMap::new(),
            remembered_acts: Remembered::new(config.remembered),
            relay_results: Remembered::new(config.remembered),
            relay_open: HashSet::new(),
            rings: rings_tx,
            timers: timers_tx,
            notices,
            next_tick: None,
        };
        (handle, tokio::spawn(core.run(messages)))
    }
}

/// A bounded memory of answers, oldest forgotten first.
struct Remembered<K, V> {
    capacity: usize,
    order: VecDeque<K>,
    values: HashMap<K, V>,
}

impl<K: Clone + Eq + std::hash::Hash, V> Remembered<K, V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            order: VecDeque::new(),
            values: HashMap::new(),
        }
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.values.get(key)
    }

    fn put(&mut self, key: K, value: V) {
        if self.values.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
            while self.order.len() > self.capacity {
                if let Some(old) = self.order.pop_front() {
                    self.values.remove(&old);
                }
            }
        }
    }
}

struct Attached {
    session: SessionId,
    capabilities: echomuse_protocol::Capabilities,
    hello: Option<echomuse_protocol::session::HelloAlerts>,
}

/// What a pending operation does to delivery until it settles.
#[derive(Clone, Debug)]
enum PendingEffect {
    Occurrence {
        schedule: Uuid,
        key: String,
        reason: TombstoneReason,
    },
    Schedule(Uuid),
    Nothing,
}

struct RawPush {
    generation: HaConnectionGeneration,
    window_start_ms: i64,
    window_end_ms: i64,
    items: Vec<CalendarItem>,
}

struct Endpoint {
    config: EndpointConfig,
    binding: Option<(String, String)>,
    delivery: Option<Delivery>,
    raw: Option<RawPush>,
    backlog: Option<(HaConnectionGeneration, Vec<CalendarItem>)>,
    view: Option<Arc<CalendarView>>,
    views: watch::Sender<Option<Arc<CalendarView>>>,
    pushes: u64,
    known: HashMap<Uuid, Occurrence>,
    guard: HashMap<(Uuid, String), journal::Action>,
    cancelled: HashSet<Uuid>,
    pending: HashMap<OpId, PendingEffect>,
    held: Vec<AlertLocalOperation>,
    guarding: HashSet<Uuid>,
    malformed: HashSet<String>,
    attached: Option<Attached>,
    state: Option<AlertState>,
    timers: Timers,
    intent_waits: u32,
    applier: mpsc::UnboundedSender<ApplierJob>,
    feed: mpsc::UnboundedSender<FeedControl>,
    /// Acts written to this speaker whose outcome no journal row records yet: the device
    /// acknowledged one and has its local operation still to upload, or never answered.
    /// A repeat under the same id waits instead of sending again; the entry lasts until
    /// that operation is journaled, for as long as the speaker is registered.
    unconfirmed_acts: HashMap<OpId, RingKind>,
    tasks: Vec<AbortHandle>,
}

impl Endpoint {
    fn session_ref(&self) -> Option<SessionRef> {
        self.attached.as_ref().map(|a| SessionRef {
            device: self.config.device.clone(),
            session: a.session,
        })
    }

    fn capable(&self, capability: Capability) -> bool {
        self.attached
            .as_ref()
            .is_some_and(|a| a.capabilities.contains(capability))
    }

    /// The reason the restore guard tombstones an occurrence: an applied operation
    /// already handled it.
    fn guarded(&self, occurrence: &Occurrence) -> Option<TombstoneReason> {
        if let Some(action) = self
            .guard
            .get(&(occurrence.schedule_id, occurrence.key.clone()))
        {
            return Some(tombstone_for(*action));
        }
        self.cancelled
            .contains(&occurrence.schedule_id)
            .then_some(TombstoneReason::Deleted)
    }

    /// The reason a pending operation that handles `occurrence` tombstones it.
    fn pending_reason(&self, occurrence: &Occurrence) -> Option<TombstoneReason> {
        self.pending.values().find_map(|effect| match effect {
            PendingEffect::Occurrence {
                schedule,
                key,
                reason,
            } if *schedule == occurrence.schedule_id && *key == occurrence.key => Some(*reason),
            PendingEffect::Schedule(schedule) if *schedule == occurrence.schedule_id => {
                Some(TombstoneReason::Deleted)
            }
            _ => None,
        })
    }

    /// Why an occurrence is not live, or `None` when it is.
    fn exclusion(
        &self,
        occurrence: &Occurrence,
        now: i64,
        catch_up: i64,
    ) -> Option<TombstoneReason> {
        self.guarded(occurrence)
            .or_else(|| self.pending_reason(occurrence))
            .or_else(|| {
                (occurrence.due_utc_ms < now - catch_up).then_some(TombstoneReason::Expired)
            })
    }

    fn live<'a>(&'a self, now: i64, catch_up: i64) -> impl Iterator<Item = &'a Occurrence> + 'a {
        self.view
            .iter()
            .flat_map(|v| v.occurrences.items.iter())
            .filter(move |o| self.exclusion(o, now, catch_up).is_none())
    }
}

fn tombstone_for(action: journal::Action) -> TombstoneReason {
    match action {
        journal::Action::Dismiss => TombstoneReason::Dismissed,
        journal::Action::Snooze => TombstoneReason::Snoozed,
        journal::Action::Expire => TombstoneReason::Expired,
        _ => TombstoneReason::Deleted,
    }
}

enum WaitKind {
    Set {
        endpoint: DeviceId,
    },
    Cancel {
        root: OpId,
        labels: Vec<(Uuid, String)>,
    },
    Act {
        kind: RingKind,
    },
}

struct Wait {
    ops: Vec<OpId>,
    rows: Vec<Option<OpRow>>,
    kind: WaitKind,
    delivers: Vec<Deliver>,
}

impl Wait {
    /// Whether every operation has an outcome: settled, or (in a cancellation) never
    /// journaled because its id conflicted. An act's row is filled in once the device's
    /// operation is journaled.
    fn settled(&self) -> bool {
        self.rows.iter().all(|row| match row {
            Some(row) => row.state != Some(State::Pending),
            None => matches!(self.kind, WaitKind::Cancel { .. }),
        })
    }
}

struct ActFlight {
    device: DeviceId,
    kind: RingKind,
    delivers: Vec<Deliver>,
}

struct Core<H, L, C> {
    config: AlertConfig,
    db: DbHandle,
    ha: Arc<H>,
    link: Arc<L>,
    clock: Arc<C>,
    inbox: mpsc::UnboundedSender<Msg>,
    tasks: JoinSet<()>,
    endpoints: HashMap<DeviceId, Endpoint>,
    /// Pending rows of speakers not registered yet, in `seq` order.
    stashed: HashMap<String, Vec<OpRow>>,
    generation: Option<HaConnectionGeneration>,
    zone: Option<HaTimeZone>,
    waits: HashMap<u64, Wait>,
    wait_index: HashMap<OpId, Vec<u64>>,
    next_wait: u64,
    acts: HashMap<OpId, ActFlight>,
    remembered_acts: Remembered<OpId, ActOutcome>,
    relay_results: Remembered<String, Value>,
    relay_open: HashSet<String>,
    rings: watch::Sender<Arc<RingsSnapshot>>,
    timers: watch::Sender<Arc<TimerViews>>,
    notices: broadcast::Sender<AlertNotice>,
    next_tick: Option<Instant>,
}

impl<H: AlertHa, L: AlertLink, C: AlertClock> Core<H, L, C> {
    async fn run(
        mut self,
        mut messages: mpsc::UnboundedReceiver<Msg>,
    ) -> Result<(), ServiceFailure> {
        self.load_pending().await;
        loop {
            let tick = self.next_tick;
            tokio::select! {
                biased;
                Some(joined) = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    if let Err(error) = joined
                        && error.is_panic() {
                            self.tasks.abort_all();
                            return Err(ServiceFailure::TaskPanicked(error.to_string()));
                        }
                }
                () = async { tokio::time::sleep_until(tick.expect("guarded")).await }, if tick.is_some() => {
                    self.tick();
                }
                message = messages.recv() => match message {
                    None | Some(Msg::Shutdown) => {
                        self.tasks.abort_all();
                        return Ok(());
                    }
                    Some(message) => self.handle(message).await,
                },
            }
        }
    }

    fn notice(&self, notice: AlertNotice) {
        let _ = self.notices.send(notice);
    }

    fn now(&self) -> i64 {
        self.clock.now_utc_ms()
    }

    async fn load_pending(&mut self) {
        match self.db.read(PendingOps).await {
            Ok(rows) => {
                for row in rows {
                    self.stashed
                        .entry(row.endpoint_id.clone())
                        .or_default()
                        .push(row);
                }
            }
            Err(error) => self.notice(AlertNotice::DatabaseProblem(format!(
                "reading pending alert operations: {error}"
            ))),
        }
    }

    async fn handle(&mut self, message: Msg) {
        match message {
            Msg::Register(config) => self.register(config).await,
            Msg::Update(config) => self.update(config).await,
            Msg::Remove(device) => {
                if let Some(endpoint) = self.endpoints.remove(&device) {
                    for task in endpoint.tasks {
                        task.abort();
                    }
                    self.publish_rings();
                    self.publish_timers();
                }
            }
            Msg::Attach(device, attach) => self.attach(device, *attach).await,
            Msg::Detach(device, session) => self.detach(&device, session),
            Msg::Inbound(device, session, message) => self.inbound(device, session, message).await,
            Msg::Timer(device, event) => self.timer_event(&device, event),
            Msg::IntentWait(device, begin) => {
                if let Some(endpoint) = self.endpoints.get_mut(&device) {
                    endpoint.intent_waits = if begin {
                        endpoint.intent_waits + 1
                    } else {
                        endpoint.intent_waits.saturating_sub(1)
                    };
                }
            }
            Msg::SetAlarm(request, deliver) => self.set_alarm(request, deliver).await,
            Msg::Cancel(request, deliver) => self.cancel_alarms(request, deliver).await,
            Msg::Act(request, deliver) => self.act(request, deliver).await,
            Msg::List(device, limit, reply) => {
                let _ = reply.send(self.list(&device, limit));
            }
            Msg::Status(device, reply) => {
                let _ = reply.send(self.status(&device));
            }
            Msg::HaUp(generation, zone) => {
                self.generation = Some(generation);
                match zone {
                    Ok(zone) => {
                        if self.zone.as_ref() != Some(&zone) {
                            self.zone = Some(zone);
                            let devices: Vec<DeviceId> = self.endpoints.keys().cloned().collect();
                            for device in devices {
                                self.rebuild_view(&device).await;
                            }
                        }
                    }
                    Err(why) => self.notice(AlertNotice::TimeZoneUnknown(why)),
                }
            }
            Msg::HaDown => self.generation = None,
            Msg::Feed(device, event) => self.feed_event(device, event).await,
            Msg::Settled(device, row) => self.settled(&device, row).await,
            Msg::GuardDone(device, occurrence) => {
                if let Some(endpoint) = self.endpoints.get_mut(&device) {
                    endpoint.guarding.remove(&occurrence);
                }
            }
            Msg::ActAcked(op, outcome) => self.act_acked(op, outcome).await,
            Msg::WaitTimeout(id) => {
                if let Some(wait) = self.take_wait(id) {
                    self.complete(wait);
                }
            }
            Msg::Relay(data) => self.relay(data).await,
            Msg::RelayDeadline(request_id) => self.relay_deadline(request_id),
            Msg::Notice(notice) => self.notice(notice),
            Msg::Shutdown => {}
        }
    }

    // ── Speakers ─────────────────────────────────────────────────────────────

    async fn register(&mut self, config: EndpointConfig) {
        if self.endpoints.contains_key(&config.device) {
            return self.update(config).await;
        }
        let device = config.device.clone();
        let row = match self.db.read(LoadDelivery(device.0.clone())).await {
            Ok(row) => row,
            Err(error) => {
                self.notice(AlertNotice::DatabaseProblem(format!(
                    "reading delivery of {device}: {error}"
                )));
                None
            }
        };
        let since = self.now() - ms(self.config.restore_guard);
        let guard_rows = self
            .db
            .read(RestoreGuard {
                endpoint_id: device.0.clone(),
                since_ms: since,
            })
            .await
            .unwrap_or_else(|error| {
                self.notice(AlertNotice::DatabaseProblem(format!(
                    "reading the restore guard of {device}: {error}"
                )));
                Vec::new()
            });
        let (views, view_rx) = watch::channel(None);
        let (applier_tx, jobs) = mpsc::unbounded_channel();
        let (feed_tx, control) = mpsc::unbounded_channel();
        let binding = row
            .as_ref()
            .map(|r| (r.calendar_entry_id.clone(), r.calendar_entity.clone()));
        let applier = self.tasks.spawn(
            Applier {
                device: device.clone(),
                ha: self.ha.clone(),
                clock: self.clock.clone(),
                db: self.db.clone(),
                jobs,
                views: view_rx,
                status: self.ha.status(),
                feed: feed_tx.clone(),
                actor: self.inbox.clone(),
                confirm_wait: self.config.confirm_wait,
                retry: self.config.retry_initial,
            }
            .run(),
        );
        let feed = self.tasks.spawn(
            Feed {
                device: device.clone(),
                ha: self.ha.clone(),
                clock: self.clock.clone(),
                title: config.calendar_title(),
                binding: binding.clone(),
                control,
                status: self.ha.status(),
                actor: self.inbox.clone(),
                timing: FeedTiming {
                    window_back_ms: ms(self.config.catch_up),
                    window_ahead_ms: ms(self.config.window_ahead),
                    backlog_ms: ms(self.config.backlog),
                    resubscribe_every: self.config.resubscribe_every,
                    retry_initial: self.config.retry_initial,
                    retry_max: self.config.retry_max,
                },
            }
            .run(),
        );
        let mut endpoint = Endpoint {
            config,
            binding,
            delivery: row.map(|r| Delivery::new(r.delivery_epoch, r.sequence, r.acked_sequence)),
            raw: None,
            backlog: None,
            view: None,
            views,
            pushes: 0,
            known: HashMap::new(),
            guard: HashMap::new(),
            cancelled: HashSet::new(),
            pending: HashMap::new(),
            held: Vec::new(),
            guarding: HashSet::new(),
            malformed: HashSet::new(),
            attached: None,
            state: None,
            timers: Timers::default(),
            intent_waits: 0,
            applier: applier_tx,
            feed: feed_tx,
            unconfirmed_acts: HashMap::new(),
            tasks: vec![applier, feed],
        };
        for entry in guard_rows {
            match entry.occurrence_key {
                Some(key) => {
                    endpoint
                        .guard
                        .insert((entry.schedule_id, key), entry.action);
                }
                None => {
                    endpoint.cancelled.insert(entry.schedule_id);
                }
            }
        }
        for row in self.stashed.remove(&device.0).unwrap_or_default() {
            endpoint.pending.insert(row.op_id, pending_effect(&row));
            let _ = endpoint.applier.send(ApplierJob::Op(row));
        }
        self.endpoints.insert(device, endpoint);
    }

    async fn update(&mut self, config: EndpointConfig) {
        let device = config.device.clone();
        let Some(endpoint) = self.endpoints.get_mut(&device) else {
            return;
        };
        if endpoint.binding.is_none() && endpoint.config.label != config.label {
            let _ = endpoint
                .feed
                .send(FeedControl::Retitle(config.calendar_title()));
        }
        let sound_changed = endpoint.config.alarm_sound != config.alarm_sound;
        let timer_sound_changed = endpoint.config.timer_sound != config.timer_sound;
        endpoint.config = config;
        // A connected speaker installs a newly chosen timer sound now, not as the timer
        // finishes (alarm sounds the device fetches itself for the alarms it holds).
        if timer_sound_changed {
            self.prefetch_timer_sound(&device);
        }
        if sound_changed {
            self.rebuild_view(&device).await;
        }
        self.publish_rings();
    }

    async fn attach(&mut self, device: DeviceId, attach: DeviceAttach) {
        let Some(endpoint) = self.endpoints.get_mut(&device) else {
            return;
        };
        endpoint.attached = Some(Attached {
            session: attach.session,
            capabilities: attach.capabilities,
            hello: attach.alerts.clone(),
        });
        endpoint.state = None;
        let outbox = endpoint
            .delivery
            .as_mut()
            .map(|d| d.attach(attach.session, attach.alerts.as_ref()));
        if let Some(outbox) = outbox {
            self.send_outbox(&device, outbox).await;
        }
        self.refresh(&device).await;
        let Some(endpoint) = self.endpoints.get(&device) else {
            return;
        };
        if endpoint.capable(Capability::TimerView) {
            let view = endpoint.timers.view(Instant::now());
            self.post(&device, PostBody::TimerView(view));
        }
        self.prefetch_timer_sound(&device);
        self.publish_rings();
    }

    fn detach(&mut self, device: &DeviceId, session: SessionId) {
        let Some(endpoint) = self.endpoints.get_mut(device) else {
            return;
        };
        if endpoint.attached.as_ref().map(|a| a.session) != Some(session) {
            return;
        }
        endpoint.attached = None;
        endpoint.state = None;
        if let Some(delivery) = &mut endpoint.delivery {
            delivery.detach(session);
        }
        self.publish_rings();
    }

    /// Has a connected speaker that installs sounds ahead fetch its timer sound.
    fn prefetch_timer_sound(&mut self, device: &DeviceId) {
        let Some(sha) = self
            .endpoints
            .get(device)
            .filter(|e| e.capable(Capability::AlertPrefetch))
            .and_then(|e| match &e.config.timer_sound {
                AlertSound::Asset(sha) => Some(sha.clone()),
                _ => None,
            })
        else {
            return;
        };
        let body = CommandBody::Prefetch(AlertPrefetch { sounds: vec![sha] });
        self.command(device, body, |_| None);
    }

    fn publish_rings(&self) {
        let devices = self
            .endpoints
            .values()
            .filter_map(|e| {
                let attached = e.attached.as_ref()?;
                let state = e.state.as_ref()?;
                Some((
                    e.config.device.0.clone(),
                    DeviceRings {
                        label: e.config.label.clone(),
                        session: attached.session,
                        active: state.active.clone(),
                        queue: state.queue.clone(),
                    },
                ))
            })
            .collect();
        self.rings.send_replace(Arc::new(RingsSnapshot { devices }));
    }

    /// Publishes every speaker's timer view, when any changed.
    fn publish_timers(&self) {
        let now = Instant::now();
        let views: TimerViews = self
            .endpoints
            .values()
            .map(|e| (e.config.device.0.clone(), e.timers.view(now)))
            .collect();
        self.timers.send_if_modified(|current| {
            if **current == views {
                false
            } else {
                *current = Arc::new(views);
                true
            }
        });
    }

    // ── Device link ──────────────────────────────────────────────────────────

    fn post(&self, device: &DeviceId, body: PostBody) {
        let Some(to) = self.endpoints.get(device).and_then(Endpoint::session_ref) else {
            return;
        };
        if let Err(refusal) = self.link.post(&to, body) {
            self.notice(AlertNotice::MalformedMessage {
                device: device.clone(),
                detail: format!("could not send to the speaker: {refusal}"),
            });
        }
    }

    /// Sends a command in its own task; `then` turns the outcome into a message.
    fn command(
        &mut self,
        device: &DeviceId,
        body: CommandBody,
        then: impl FnOnce(CommandOutcome) -> Option<Msg> + Send + 'static,
    ) -> bool {
        let Some(to) = self.endpoints.get(device).and_then(Endpoint::session_ref) else {
            return false;
        };
        let link = self.link.clone();
        let inbox = self.inbox.clone();
        self.tasks.spawn(async move {
            let outcome = link.command(&to, body).await;
            if let Some(message) = then(outcome) {
                let _ = inbox.send(message);
            }
        });
        true
    }

    async fn send_outbox(&mut self, device: &DeviceId, outbox: Outbox) {
        if let Some((sequence, acked)) = outbox.position {
            let write = SetDeliveryPosition {
                endpoint_id: device.0.clone(),
                sequence,
                acked_sequence: acked,
            };
            if let Err(error) = self.db.write(write).await {
                // Unrecorded sequences must not reach the device: a restart could reuse
                // them for other content.
                self.notice(AlertNotice::DatabaseProblem(format!(
                    "recording the delivery position of {device}: {error}"
                )));
                return;
            }
        }
        for body in outbox.bodies {
            self.post(device, body);
        }
    }

    /// Brings the speaker's device to its live set, when both are known.
    async fn refresh(&mut self, device: &DeviceId) {
        let now = self.now();
        let catch_up = ms(self.config.catch_up);
        let max = self.config.max_occurrences;
        let Some(endpoint) = self.endpoints.get_mut(device) else {
            return;
        };
        if endpoint.view.is_none() || endpoint.delivery.is_none() || endpoint.attached.is_none() {
            return;
        }
        let mut desired = Desired::default();
        let mut live = 0usize;
        let mut total = 0usize;
        for occurrence in endpoint
            .view
            .iter()
            .flat_map(|v| v.occurrences.items.iter())
        {
            match endpoint.exclusion(occurrence, now, catch_up) {
                Some(reason) => {
                    desired.reasons.insert(occurrence.occurrence_id, reason);
                }
                None => {
                    total += 1;
                    if live < max {
                        live += 1;
                        desired.live.push(occurrence.object(0));
                    }
                }
            }
        }
        for effect in endpoint.pending.values() {
            if let PendingEffect::Occurrence {
                schedule,
                key,
                reason,
            } = effect
            {
                desired
                    .reasons
                    .insert(ids::occurrence_id(*schedule, key), *reason);
            }
        }
        let delivery = endpoint.delivery.as_mut().expect("checked above");
        let outbox = delivery.refresh(&desired);
        if total > max {
            self.notice(AlertNotice::TooManyOccurrences {
                device: device.clone(),
                count: total,
            });
        }
        match outbox {
            Ok(outbox) => self.send_outbox(device, outbox).await,
            Err(error) => self.notice(AlertNotice::MalformedMessage {
                device: device.clone(),
                detail: format!("the alert snapshot cannot be built: {error}"),
            }),
        }
    }

    async fn inbound(&mut self, device: DeviceId, session: SessionId, message: AlertInbound) {
        let Some(endpoint) = self.endpoints.get_mut(&device) else {
            return;
        };
        if endpoint.attached.as_ref().map(|a| a.session) != Some(session) {
            return;
        }
        match message {
            AlertInbound::Ack(ack) => {
                let Some(delivery) = &mut endpoint.delivery else {
                    return;
                };
                let outbox = delivery.ack(&ack);
                let owed = delivery.snapshot_owed();
                self.send_outbox(&device, outbox).await;
                if owed {
                    self.refresh(&device).await;
                }
            }
            AlertInbound::State(state) => {
                endpoint.state = Some(state);
                self.publish_rings();
            }
            AlertInbound::RingEnded(ended) => self.notice(AlertNotice::RingEnded { device, ended }),
            AlertInbound::LocalOperation(op) => self.local_operation(&device, *op).await,
        }
    }

    // ── Calendar ─────────────────────────────────────────────────────────────

    async fn feed_event(&mut self, device: DeviceId, event: FeedEvent) {
        let Some(endpoint) = self.endpoints.get_mut(&device) else {
            return;
        };
        match event {
            FeedEvent::Bound { entry_id, entity } => {
                let bind = BindCalendar {
                    endpoint_id: device.0.clone(),
                    calendar_entry_id: entry_id.clone(),
                    calendar_entity: entity.clone(),
                    new_epoch: Uuid::new_v4().to_string(),
                };
                match self.db.write(bind).await {
                    Ok(row) => {
                        let Some(endpoint) = self.endpoints.get_mut(&device) else {
                            return;
                        };
                        endpoint.binding = Some((entry_id, entity));
                        if endpoint.delivery.is_none() {
                            let mut delivery =
                                Delivery::new(row.delivery_epoch, row.sequence, row.acked_sequence);
                            let mut outbox = None;
                            if let Some(attached) = &endpoint.attached {
                                outbox = Some(
                                    delivery.attach(attached.session, attached.hello.as_ref()),
                                );
                            }
                            endpoint.delivery = Some(delivery);
                            if let Some(outbox) = outbox {
                                self.send_outbox(&device, outbox).await;
                            }
                            self.refresh(&device).await;
                        }
                    }
                    Err(error) => self.notice(AlertNotice::DatabaseProblem(format!(
                        "recording the calendar of {device}: {error}"
                    ))),
                }
            }
            FeedEvent::Backlog { generation, items } => {
                endpoint.backlog = Some((generation, items));
                self.process_backlog(&device).await;
            }
            FeedEvent::Push {
                generation,
                window_start_ms,
                window_end_ms,
                push,
            } => match push {
                CalendarPush::Events(items) => {
                    endpoint.raw = Some(RawPush {
                        generation,
                        window_start_ms,
                        window_end_ms,
                        items,
                    });
                    self.rebuild_view(&device).await;
                }
                CalendarPush::Unavailable => self.notice(AlertNotice::CalendarProblem {
                    device,
                    detail: "Home Assistant could not read the calendar".to_owned(),
                }),
                CalendarPush::Unreadable(detail) => {
                    self.notice(AlertNotice::CalendarProblem { device, detail })
                }
            },
            FeedEvent::Problem(detail) => {
                self.notice(AlertNotice::CalendarProblem { device, detail })
            }
        }
    }

    fn entity(&self, device: &DeviceId) -> Option<String> {
        self.endpoints
            .get(device)?
            .binding
            .as_ref()
            .map(|(_, entity)| entity.clone())
    }

    async fn process_backlog(&mut self, device: &DeviceId) {
        let (Some(zone), Some(entity)) = (self.zone.clone(), self.entity(device)) else {
            return;
        };
        let Some(endpoint) = self.endpoints.get_mut(device) else {
            return;
        };
        let Some((_, items)) = endpoint.backlog.take() else {
            return;
        };
        let read = occurrences(&items, &entity, &zone, &endpoint.config.alarm_sound);
        let mut missed = Vec::new();
        for occurrence in read.items {
            endpoint
                .known
                .insert(occurrence.occurrence_id, occurrence.clone());
            if endpoint.guarded(&occurrence).is_some() {
                guard_delete(endpoint, &entity, &occurrence);
            } else if endpoint.pending_reason(&occurrence).is_none() {
                missed.push(occurrence);
            }
        }
        self.journal_missed(device, &entity, missed, true).await;
    }

    async fn rebuild_view(&mut self, device: &DeviceId) {
        let (Some(zone), Some(entity)) = (self.zone.clone(), self.entity(device)) else {
            return;
        };
        let now = self.now();
        let catch_up = ms(self.config.catch_up);
        let Some(endpoint) = self.endpoints.get_mut(device) else {
            return;
        };
        let Some(raw) = &endpoint.raw else {
            return;
        };
        let read = occurrences(&raw.items, &entity, &zone, &endpoint.config.alarm_sound);
        endpoint.pushes += 1;
        let view = Arc::new(CalendarView {
            generation: raw.generation,
            push: endpoint.pushes,
            window_start_ms: raw.window_start_ms,
            window_end_ms: raw.window_end_ms,
            occurrences: read,
        });
        let mut notices = Vec::new();
        let mut missed = Vec::new();
        for occurrence in &view.occurrences.items {
            endpoint
                .known
                .insert(occurrence.occurrence_id, occurrence.clone());
            if let EventOrigin::MalformedLine(detail) = &occurrence.origin
                && endpoint.malformed.insert(occurrence.uid.clone())
            {
                notices.push(AlertNotice::MalformedEvent {
                    device: device.clone(),
                    uid: occurrence.uid.clone(),
                    detail: detail.clone(),
                });
            }
            if endpoint.guarded(occurrence).is_some() {
                guard_delete(endpoint, &entity, occurrence);
            } else if occurrence.due_utc_ms < now - catch_up
                && endpoint.pending_reason(occurrence).is_none()
            {
                missed.push(occurrence.clone());
            }
        }
        endpoint.view = Some(view.clone());
        endpoint.views.send_replace(Some(view));
        let held = std::mem::take(&mut endpoint.held);
        for notice in notices {
            self.notice(notice);
        }
        self.journal_missed(device, &entity, missed, false).await;
        for op in held {
            self.local_operation(device, op).await;
        }
        if self
            .endpoints
            .get(device)
            .is_some_and(|e| e.backlog.is_some())
        {
            self.process_backlog(device).await;
        }
        self.refresh(device).await;
    }

    /// Journals the expiry of occurrences past their catch-up window as `missed`.
    async fn journal_missed(
        &mut self,
        device: &DeviceId,
        entity: &str,
        missed: Vec<Occurrence>,
        before_window: bool,
    ) {
        if missed.is_empty() {
            return;
        }
        let now = self.now();
        let ops: Vec<NewOp> = missed
            .iter()
            .map(|occurrence| NewOp {
                op_id: OpId::missed(occurrence.occurrence_id),
                endpoint_id: device.0.clone(),
                calendar_entity: entity.to_owned(),
                action: journal::Action::Expire,
                schedule_id: occurrence.schedule_id,
                occurrence_key: Some(occurrence.key.clone()),
                source: journal::Source::Engine,
                request: ops::missed_request(&device.0, occurrence),
                plan: json!({
                    "target": ops::target_json(occurrence),
                    "due_utc_ms": occurrence.due_utc_ms,
                    "reason": "missed",
                    "before_window": before_window,
                }),
                created_ms: now,
            })
            .collect();
        self.journal(device, ops).await;
    }

    /// Journals operations and hands new ones to the speaker's applier.
    async fn journal(&mut self, device: &DeviceId, ops: Vec<NewOp>) -> Option<Vec<Journaled>> {
        match self.db.write(journal::JournalOps(ops)).await {
            Ok(outcomes) => {
                if let Some(endpoint) = self.endpoints.get_mut(device) {
                    for outcome in &outcomes {
                        if let Journaled::Inserted(row) = outcome {
                            endpoint.pending.insert(row.op_id, pending_effect(row));
                            let _ = endpoint.applier.send(ApplierJob::Op(row.clone()));
                        }
                    }
                }
                Some(outcomes)
            }
            Err(error) => {
                self.notice(AlertNotice::DatabaseProblem(format!(
                    "journaling an alert operation: {error}"
                )));
                None
            }
        }
    }

    async fn settled(&mut self, device: &DeviceId, row: OpRow) {
        if let Some(endpoint) = self.endpoints.get_mut(device) {
            endpoint.pending.remove(&row.op_id);
            if row.state == Some(State::Applied)
                && let (Some(action), Some(schedule)) = (row.action, row.schedule_id)
            {
                match (action, &row.occurrence_key) {
                    (journal::Action::Cancel, _) => {
                        endpoint.cancelled.insert(schedule);
                    }
                    (action, Some(key)) if action.is_terminal() => {
                        endpoint.guard.insert((schedule, key.clone()), action);
                    }
                    _ => {}
                }
            }
        }
        if ops::request_kind(&row) == Some(ops::kind::LOCAL_OPERATION) {
            self.post_op_result(device, &row);
            if row.state == Some(State::Rejected) {
                self.retract_child(device, &row).await;
            }
        }
        if row.state == Some(State::Rejected) {
            self.notice(AlertNotice::OperationRejected {
                device: device.clone(),
                op_id: row.op_id,
                reason: ops::rejection_of(&row),
            });
        }
        if let Some(ids) = self.wait_index.remove(&row.op_id) {
            for id in ids {
                let complete = match self.waits.get_mut(&id) {
                    Some(wait) => {
                        if let Some(position) = wait.ops.iter().position(|op| *op == row.op_id) {
                            wait.rows[position] = Some(row.clone());
                        }
                        wait.settled()
                    }
                    None => false,
                };
                if complete && let Some(wait) = self.take_wait(id) {
                    self.complete(wait);
                }
            }
        }
        self.refresh(device).await;
    }

    fn post_op_result(&self, device: &DeviceId, row: &OpRow) {
        let state = match row.state {
            Some(State::Applied) => OpResultState::Applied,
            Some(State::Rejected) => OpResultState::Rejected,
            _ => return,
        };
        let error = (state == OpResultState::Rejected).then(|| ops::rejection_of(row).code);
        self.post(
            device,
            PostBody::OpResult(AlertOpResult {
                op_id: row.op_id.uuid(),
                state,
                error,
            }),
        );
    }

    /// A rejected snooze's child: tombstoned on the device.
    async fn retract_child(&mut self, device: &DeviceId, row: &OpRow) {
        let child = row
            .plan()
            .and_then(|p| p.get("child_occurrence_id"))
            .and_then(Value::as_str)
            .and_then(|t| Uuid::parse_str(t).ok());
        let Some(child) = child else {
            return;
        };
        let Some(endpoint) = self.endpoints.get_mut(device) else {
            return;
        };
        if let Some(delivery) = &mut endpoint.delivery {
            let outbox = delivery.retract(&[(child, TombstoneReason::Deleted)]);
            self.send_outbox(device, outbox).await;
        }
    }

    // ── Device operations ────────────────────────────────────────────────────

    async fn local_operation(&mut self, device: &DeviceId, op: AlertLocalOperation) {
        let op_id = OpId::from_device(op.op_id);
        match self.db.read(OpsById(vec![op_id])).await {
            Ok(rows) => {
                if let Some(row) = rows.into_iter().next() {
                    // Already journaled: answer once it has settled.
                    if let Some(endpoint) = self.endpoints.get_mut(device) {
                        endpoint.unconfirmed_acts.remove(&op_id);
                    }
                    return self.post_op_result(device, &row);
                }
            }
            Err(error) => {
                return self.notice(AlertNotice::DatabaseProblem(format!(
                    "looking up a local operation: {error}"
                )));
            }
        }
        let Some(entity) = self.entity(device) else {
            return self.hold(device, op);
        };
        let has_view = self.endpoints.get(device).is_some_and(|e| e.view.is_some());
        if !has_view || self.zone.is_none() {
            return self.hold(device, op);
        }
        let Some(endpoint) = self.endpoints.get_mut(device) else {
            return;
        };
        let Some(target) = endpoint.known.get(&op.occurrence_id).cloned() else {
            // Not on the calendar as far as it has been read: a dismissal or expiry has
            // nothing to write; a snooze has no parent to adopt.
            let (state, error) = match op.action {
                LocalAction::Snooze => {
                    (OpResultState::Rejected, Some(ops::code::UNKNOWN_OCCURRENCE))
                }
                _ => (OpResultState::Applied, None),
            };
            if let (LocalAction::Snooze, Some(child)) = (op.action, &op.child)
                && let Some(delivery) = &mut endpoint.delivery
            {
                let outbox = delivery.retract(&[(child.occurrence_id, TombstoneReason::Deleted)]);
                self.send_outbox(device, outbox).await;
            }
            return self.post(
                device,
                PostBody::OpResult(AlertOpResult {
                    op_id: op.op_id,
                    state,
                    error: error.map(str::to_owned),
                }),
            );
        };
        let (action, mut plan) = match op.action {
            LocalAction::Dismiss => (journal::Action::Dismiss, json!({})),
            LocalAction::Expire => (journal::Action::Expire, json!({})),
            LocalAction::Snooze => {
                let child = op.child.clone().expect("decoded snooze has a child");
                if child.schedule_id != ids::snooze_child_schedule_id(op.occurrence_id)
                    || child.occurrence_id
                        != ids::occurrence_id(child.schedule_id, &child.due_local)
                {
                    return self.post(
                        device,
                        PostBody::OpResult(AlertOpResult {
                            op_id: op.op_id,
                            state: OpResultState::Rejected,
                            error: Some(ops::code::INVALID_CHILD.to_owned()),
                        }),
                    );
                }
                let event = snooze_child_event(&target, op.op_id, &child);
                (
                    journal::Action::Snooze,
                    json!({
                        "child": ops::event_json(&event),
                        "child_occurrence_id": child.occurrence_id.to_string(),
                    }),
                )
            }
        };
        plan["target"] = ops::target_json(&target);
        plan["due_utc_ms"] = json!(target.due_utc_ms);
        plan["before_window"] = json!(false);
        let source = match op.source {
            LocalSource::Voice => journal::Source::Voice,
            LocalSource::Llm => journal::Source::Llm,
            LocalSource::Dashboard => journal::Source::Dashboard,
            LocalSource::Button | LocalSource::Entity | LocalSource::Executor => {
                journal::Source::Device
            }
        };
        let new = NewOp {
            op_id,
            endpoint_id: device.0.clone(),
            calendar_entity: entity,
            action,
            schedule_id: target.schedule_id,
            occurrence_key: Some(target.key.clone()),
            source,
            request: ops::local_request(&device.0, &op),
            plan,
            created_ms: self.now(),
        };
        if let Some(outcomes) = self.journal(device, vec![new]).await
            && let Some(Journaled::Inserted(row) | Journaled::Existing(row)) = outcomes.first()
        {
            // An act waiting on this operation now has a row to follow, which also answers
            // any later repeat of it.
            if let Some(endpoint) = self.endpoints.get_mut(device) {
                endpoint.unconfirmed_acts.remove(&op_id);
            }
            self.attach_row_to_waits(row.clone());
        }
        self.refresh(device).await;
    }

    fn hold(&mut self, device: &DeviceId, op: AlertLocalOperation) {
        if let Some(endpoint) = self.endpoints.get_mut(device)
            && !endpoint.held.iter().any(|held| held.op_id == op.op_id)
        {
            endpoint.held.push(op);
        }
    }

    // ── Requests ─────────────────────────────────────────────────────────────

    /// Whether new calendar operations can be journaled for `device`: HA connected, its
    /// time zone known, and the calendar read on this connection.
    fn ready(
        &self,
        device: &DeviceId,
    ) -> Result<(&Endpoint, &HaTimeZone, Arc<CalendarView>), Refusal> {
        let endpoint = self.endpoints.get(device).ok_or(Refusal::UnknownEndpoint)?;
        let generation = self.generation.ok_or(Refusal::HaUnavailable)?;
        let zone = self.zone.as_ref().ok_or(Refusal::HaUnavailable)?;
        endpoint.binding.as_ref().ok_or(Refusal::HaUnavailable)?;
        let view = endpoint
            .view
            .clone()
            .filter(|v| v.generation == generation)
            .ok_or(Refusal::HaUnavailable)?;
        Ok((endpoint, zone, view))
    }

    async fn existing(&self, op: OpId) -> Result<Option<OpRow>, ()> {
        match self.db.read(OpsById(vec![op])).await {
            Ok(rows) => Ok(rows.into_iter().next()),
            Err(error) => {
                self.notice(AlertNotice::DatabaseProblem(format!(
                    "looking up operation {op}: {error}"
                )));
                Err(())
            }
        }
    }

    /// The row a repeated request was journaled under, if any; a different request
    /// under the same id is `op_id_conflict`.
    async fn repeated(&self, op: OpId, request: &Value) -> Result<Option<OpRow>, Refusal> {
        let row = self
            .existing(op)
            .await
            .map_err(|()| Refusal::InvalidRequest("journal unavailable".to_owned()))?;
        match row {
            Some(row)
                if request_digest(request).ok().as_deref() != Some(row.payload_sha256.as_str()) =>
            {
                Err(Refusal::OpIdConflict)
            }
            row => Ok(row),
        }
    }

    async fn set_alarm(&mut self, request: SetAlarm, deliver: Deliver) {
        let op = request.op_id;
        let body = ops::set_request(&request);
        match self.repeated(op, &body).await {
            Err(refusal) => return self.refuse(deliver, refusal),
            Ok(Some(row)) => {
                return self.wait_rows(
                    vec![op],
                    vec![Some(row)],
                    WaitKind::Set {
                        endpoint: request.endpoint,
                    },
                    vec![deliver],
                );
            }
            Ok(None) => {}
        }
        let label = request
            .label
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty());
        if let Some(label) = label
            && label.chars().count() > echomuse_protocol::alerts::MAX_LABEL_CHARS
        {
            return self.refuse(
                deliver,
                Refusal::InvalidRequest("the label is too long".to_owned()),
            );
        }
        let now = self.now();
        let plan = match self.ready(&request.endpoint) {
            Err(refusal) => Err(refusal),
            Ok((endpoint, zone, view)) => {
                let schedules: HashSet<Uuid> = view
                    .occurrences
                    .items
                    .iter()
                    .filter(|o| matches!(o.origin, EventOrigin::EchoMuse { .. }))
                    .map(|o| o.schedule_id)
                    .collect();
                if schedules.len() >= self.config.max_schedules {
                    Err(Refusal::CapacityExceeded)
                } else {
                    let instant = jiff::Timestamp::from_millisecond(now).unwrap_or_default();
                    match request.when.first_due(instant, zone) {
                        Err(crate::schedule::ScheduleError::InThePast) => Err(Refusal::InThePast),
                        Err(error) => Err(Refusal::InvalidRequest(error.to_string())),
                        Ok(due) => {
                            let schedule_id = ids::new_schedule_id(op);
                            let line = EchomuseLine {
                                id: schedule_id,
                                kind: OccurrenceKind::Alarm,
                                op: op.uuid(),
                                parent: None,
                                ring: RingSettings::defaults(endpoint.config.alarm_sound.clone()),
                            };
                            let end = due.timestamp() + jiff::SignedDuration::from_secs(60);
                            let event = echomuse_ha::EventWrite {
                                summary: label.unwrap_or(defaults::LABEL).to_owned(),
                                dtstart: local_text(&due),
                                dtend: zone.local_text(end),
                                description: Some(line.description(None)),
                                rrule: request.when.repeat.rrule(),
                            };
                            let entity = endpoint.binding.clone().expect("ready").1;
                            Ok(NewOp {
                                op_id: op,
                                endpoint_id: request.endpoint.0.clone(),
                                calendar_entity: entity,
                                action: journal::Action::Create,
                                schedule_id,
                                occurrence_key: None,
                                source: request.source.journal(),
                                request: body,
                                plan: json!({
                                    "event": ops::event_json(&event),
                                    "first_due_utc_ms": due.timestamp().as_millisecond(),
                                }),
                                created_ms: now,
                            })
                        }
                    }
                }
            }
        };
        let new = match plan {
            Ok(new) => new,
            Err(refusal) => return self.refuse(deliver, refusal),
        };
        let kind = WaitKind::Set {
            endpoint: request.endpoint.clone(),
        };
        match self.journal(&request.endpoint, vec![new]).await {
            None => self.refuse(
                deliver,
                Refusal::InvalidRequest("journal unavailable".to_owned()),
            ),
            Some(outcomes) => match outcomes.into_iter().next() {
                Some(Journaled::Inserted(row) | Journaled::Existing(row)) => {
                    self.wait_rows(vec![op], vec![Some(row)], kind, vec![deliver])
                }
                _ => self.refuse(deliver, Refusal::OpIdConflict),
            },
        }
    }

    async fn cancel_alarms(&mut self, request: CancelAlarms, deliver: Deliver) {
        let root = request.op_id;
        let body = ops::cancel_request(&request.endpoint.0, &request.selector);
        match self.repeated(root, &body).await {
            Err(refusal) => return self.refuse(deliver, refusal),
            Ok(Some(row)) => {
                return self.cancel_from_journal(row, deliver).await;
            }
            Ok(None) => {}
        }
        let now = self.now();
        let catch_up = ms(self.config.catch_up);
        let targets: Vec<(Uuid, String)> = match self.ready(&request.endpoint) {
            Err(refusal) => return self.refuse(deliver, refusal),
            Ok((endpoint, _, _)) => {
                let mut seen = HashSet::new();
                endpoint
                    .live(now, catch_up)
                    .filter(|o| match &request.selector {
                        CancelSelector::Schedules(ids) => ids.contains(&o.schedule_id),
                        CancelSelector::Label(label) => {
                            o.summary.trim().eq_ignore_ascii_case(label.trim())
                        }
                        CancelSelector::Time(time) => {
                            TimeOfDay::of_local_text(&o.due_local) == Some(*time)
                        }
                        CancelSelector::All => true,
                    })
                    .filter(|o| seen.insert(o.schedule_id))
                    .map(|o| (o.schedule_id, o.summary.clone()))
                    .collect()
            }
        };
        if targets.is_empty() {
            return self.refuse(deliver, Refusal::NothingToCancel);
        }
        let entity = self.entity(&request.endpoint).expect("ready");
        let new: Vec<NewOp> = targets
            .iter()
            .enumerate()
            .map(|(index, (schedule, _))| {
                let (op_id, request_json, plan) = if index == 0 {
                    (
                        root,
                        body.clone(),
                        json!({"targets": targets.iter().map(|(s, l)| json!({"schedule_id": s.to_string(), "label": l})).collect::<Vec<_>>()}),
                    )
                } else {
                    (
                        OpId::group_member(root, *schedule),
                        ops::cancel_member_request(root, *schedule),
                        Value::Null,
                    )
                };
                NewOp {
                    op_id,
                    endpoint_id: request.endpoint.0.clone(),
                    calendar_entity: entity.clone(),
                    action: journal::Action::Cancel,
                    schedule_id: *schedule,
                    occurrence_key: None,
                    source: request.source.journal(),
                    request: request_json,
                    plan,
                    created_ms: now,
                }
            })
            .collect();
        let op_ids: Vec<OpId> = new.iter().map(|op| op.op_id).collect();
        match self.journal(&request.endpoint, new).await {
            None => self.refuse(
                deliver,
                Refusal::InvalidRequest("journal unavailable".to_owned()),
            ),
            Some(outcomes) => {
                let rows = outcomes
                    .into_iter()
                    .map(|outcome| match outcome {
                        Journaled::Inserted(row) | Journaled::Existing(row) => Some(row),
                        Journaled::Conflict(_) => None,
                    })
                    .collect();
                self.wait_rows(
                    op_ids,
                    rows,
                    WaitKind::Cancel {
                        root,
                        labels: targets,
                    },
                    vec![deliver],
                );
                self.refresh(&request.endpoint).await;
            }
        }
    }

    /// A repeated cancellation: the group recorded in the root's plan.
    async fn cancel_from_journal(&mut self, root: OpRow, deliver: Deliver) {
        let targets: Vec<(Uuid, String)> = root
            .plan()
            .and_then(|p| p.get("targets"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|t| {
                        let schedule = Uuid::parse_str(t.get("schedule_id")?.as_str()?).ok()?;
                        Some((schedule, t.get("label")?.as_str()?.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let ops: Vec<OpId> = targets
            .iter()
            .enumerate()
            .map(|(i, (s, _))| {
                if i == 0 {
                    root.op_id
                } else {
                    OpId::group_member(root.op_id, *s)
                }
            })
            .collect();
        let found = self.db.read(OpsById(ops.clone())).await.unwrap_or_default();
        let rows = ops
            .iter()
            .map(|op| found.iter().find(|r| r.op_id == *op).cloned())
            .collect();
        self.wait_rows(
            ops,
            rows,
            WaitKind::Cancel {
                root: root.op_id,
                labels: targets,
            },
            vec![deliver],
        );
    }

    fn list(&self, device: &DeviceId, limit: usize) -> Result<Vec<AlarmEntry>, Refusal> {
        let (endpoint, _, _) = self.ready(device)?;
        let now = self.now();
        Ok(endpoint
            .live(now, ms(self.config.catch_up))
            .take(limit)
            .map(|o| AlarmEntry {
                schedule_id: o.schedule_id,
                occurrence_id: o.occurrence_id,
                kind: o.kind,
                label: o.summary.clone(),
                due_local: o.due_local.clone(),
                due_utc_ms: o.due_utc_ms,
                rrule: o.rrule.clone(),
                armed_on_endpoint: endpoint
                    .delivery
                    .as_ref()
                    .is_some_and(|d| d.armed(o.occurrence_id)),
            })
            .collect())
    }

    fn status(&self, device: &DeviceId) -> Option<crate::status::EndpointStatus> {
        use crate::status::{DeliveryPosition, EndpointStatus, OccurrenceStatus, StatusFlag};
        let endpoint = self.endpoints.get(device)?;
        let calendar_live = self.ready(device).is_ok();
        let mut flags = Vec::new();
        if self.generation.is_none() {
            flags.push(StatusFlag::HaUnavailable);
        } else if self.zone.is_none() {
            flags.push(StatusFlag::TimeZoneUnknown);
        }
        if endpoint.binding.is_none() {
            flags.push(StatusFlag::CalendarNotBound);
        }
        if endpoint
            .delivery
            .as_ref()
            .is_some_and(Delivery::snapshot_owed)
        {
            flags.push(StatusFlag::SnapshotOwed);
        }
        if !endpoint.malformed.is_empty() {
            flags.push(StatusFlag::MalformedEvents);
        }
        let occurrences = if calendar_live {
            endpoint
                .live(self.now(), ms(self.config.catch_up))
                .map(|o| OccurrenceStatus {
                    schedule_id: o.schedule_id,
                    occurrence_id: o.occurrence_id,
                    kind: o.kind,
                    label: o.summary.clone(),
                    due_local: o.due_local.clone(),
                    armed_on_endpoint: endpoint
                        .delivery
                        .as_ref()
                        .is_some_and(|d| d.armed(o.occurrence_id)),
                })
                .collect()
        } else {
            Vec::new()
        };
        Some(EndpointStatus {
            calendar_live,
            online: endpoint.attached.is_some(),
            delivery: endpoint.delivery.as_ref().map(|d| {
                let (sequence, acked) = d.position();
                DeliveryPosition { sequence, acked }
            }),
            flags,
            occurrences,
            alert_state: endpoint.state.clone(),
        })
    }

    async fn act(&mut self, request: Act, deliver: Deliver) {
        let op = request.op_id;
        if let Some(outcome) = self.remembered_acts.get(&op).cloned() {
            return self.deliver_act(deliver, outcome, None);
        }
        if let Some(flight) = self.acts.get_mut(&op) {
            return flight.delivers.push(deliver);
        }
        if let Some(id) = self
            .wait_index
            .get(&op)
            .and_then(|ids| ids.first())
            .copied()
            && let Some(wait) = self.waits.get_mut(&id)
        {
            return wait.delivers.push(deliver);
        }
        match self.existing(op).await {
            Err(()) => {
                return self.refuse(
                    deliver,
                    Refusal::InvalidRequest("journal unavailable".to_owned()),
                );
            }
            Ok(Some(row)) => {
                return self.wait_rows(
                    vec![op],
                    vec![Some(row)],
                    WaitKind::Act {
                        kind: RingKind::Alarm,
                    },
                    vec![deliver],
                );
            }
            Ok(None) => {
                if let Some(kind) = self
                    .endpoints
                    .get(&request.target.device)
                    .and_then(|e| e.unconfirmed_acts.get(&op))
                    .copied()
                {
                    return self.wait_rows(
                        vec![op],
                        vec![None],
                        WaitKind::Act { kind },
                        vec![deliver],
                    );
                }
            }
        }
        let Some(endpoint) = self.endpoints.get(&request.target.device) else {
            return self.refuse(deliver, Refusal::UnknownEndpoint);
        };
        if endpoint.attached.is_none() {
            return self.refuse(deliver, Refusal::DeviceNotConnected);
        }
        let Some(active) = endpoint.state.as_ref().and_then(|s| s.active.clone()) else {
            return self.refuse(deliver, Refusal::NothingRinging);
        };
        if active.id != request.target.ring_id {
            return self.refuse(deliver, Refusal::RingChanged { now: active.id });
        }
        if request.action == ActAction::Snooze && active.kind == RingKind::Timer {
            return self.refuse(deliver, Refusal::NotSnoozable);
        }
        let body = CommandBody::Act(AlertAct {
            op_id: op.uuid(),
            target_id: active.id.clone(),
            action: request.action,
            source: request.source,
        });
        self.acts.insert(
            op,
            ActFlight {
                device: request.target.device.clone(),
                kind: active.kind,
                delivers: vec![deliver],
            },
        );
        let sent = self.command(&request.target.device, body, move |outcome| {
            Some(Msg::ActAcked(op, outcome))
        });
        if !sent && let Some(flight) = self.acts.remove(&op) {
            for deliver in flight.delivers {
                self.refuse(deliver, Refusal::DeviceNotConnected);
            }
        }
    }

    async fn act_acked(&mut self, op: OpId, outcome: CommandOutcome) {
        let Some(flight) = self.acts.remove(&op) else {
            return;
        };
        let alarm = flight.kind != RingKind::Timer;
        let finished = match outcome {
            CommandOutcome::NotSent(_) => {
                Some((ActOutcome::Refused(Refusal::DeviceNotConnected), false))
            }
            CommandOutcome::Unknown(cause) => {
                Some((ActOutcome::Unknown { op_id: op, cause }, true))
            }
            CommandOutcome::Acked {
                status: AckStatus::Rejected,
                error,
            } => Some((
                ActOutcome::Refused(Refusal::DeviceRefused(error.unwrap_or_default())),
                true,
            )),
            CommandOutcome::Acked {
                status: AckStatus::Applied,
                error: None,
            } if !alarm => Some((
                ActOutcome::Applied {
                    op_id: op,
                    kind: flight.kind,
                },
                true,
            )),
            CommandOutcome::Acked {
                status: AckStatus::Accepted,
                ..
            } => Some((
                ActOutcome::Unknown {
                    op_id: op,
                    cause: UnknownCause::Inconclusive,
                },
                true,
            )),
            // An alarm operation the device journaled (durable), or stopped without
            // persisting (applied with an error): it uploads the operation next.
            CommandOutcome::Acked { .. } => None,
        };
        let unconfirmed = match &finished {
            Some((ActOutcome::Unknown { .. }, _)) | None => true,
            Some(_) => false,
        };
        if unconfirmed && let Some(endpoint) = self.endpoints.get_mut(&flight.device) {
            endpoint.unconfirmed_acts.insert(op, flight.kind);
        }
        match finished {
            Some((outcome, remember)) => {
                if remember {
                    self.remembered_acts.put(op, outcome.clone());
                }
                for deliver in flight.delivers {
                    self.deliver_act(deliver, outcome.clone(), None);
                }
            }
            None => {
                let row = self.existing(op).await.ok().flatten();
                self.wait_rows(
                    vec![op],
                    vec![row],
                    WaitKind::Act { kind: flight.kind },
                    flight.delivers,
                );
            }
        }
    }

    // ── Waiting for outcomes ─────────────────────────────────────────────────

    fn refuse(&mut self, deliver: Deliver, refusal: Refusal) {
        match deliver {
            Deliver::Set(reply) => {
                let _ = reply.send(Outcome::Refused(refusal));
            }
            Deliver::Cancel(reply) => {
                let _ = reply.send(CancelOutcome::Refused(refusal));
            }
            Deliver::Act(reply) => {
                let _ = reply.send(ActOutcome::Refused(refusal));
            }
            Deliver::Relay { request_id, .. } => {
                self.relay_answer(request_id, relay::error_result(&refusal.to_string()), true);
            }
        }
    }

    /// Waits for `ops` to settle, completing at once when they have or after the settle
    /// wait with whatever is still pending.
    fn wait_rows(
        &mut self,
        ops: Vec<OpId>,
        rows: Vec<Option<OpRow>>,
        kind: WaitKind,
        delivers: Vec<Deliver>,
    ) {
        let wait = Wait {
            ops,
            rows,
            kind,
            delivers,
        };
        if wait.settled() {
            return self.complete(wait);
        }
        let id = self.next_wait;
        self.next_wait += 1;
        for op in &wait.ops {
            self.wait_index.entry(*op).or_default().push(id);
        }
        self.waits.insert(id, wait);
        let inbox = self.inbox.clone();
        let after = self.config.settle_wait;
        self.tasks.spawn(async move {
            tokio::time::sleep(after).await;
            let _ = inbox.send(Msg::WaitTimeout(id));
        });
    }

    /// A row journaled after its wait began (an act's local operation).
    fn attach_row_to_waits(&mut self, row: OpRow) {
        let Some(ids) = self.wait_index.get(&row.op_id) else {
            return;
        };
        for id in ids {
            if let Some(wait) = self.waits.get_mut(id)
                && let Some(position) = wait.ops.iter().position(|op| *op == row.op_id)
                && wait.rows[position].is_none()
            {
                wait.rows[position] = Some(row.clone());
            }
        }
    }

    fn take_wait(&mut self, id: u64) -> Option<Wait> {
        let wait = self.waits.remove(&id)?;
        for op in &wait.ops {
            if let Some(ids) = self.wait_index.get_mut(op) {
                ids.retain(|other| *other != id);
                if ids.is_empty() {
                    self.wait_index.remove(op);
                }
            }
        }
        Some(wait)
    }

    fn armed(&self, device: &DeviceId, occurrence: Uuid) -> bool {
        self.endpoints
            .get(device)
            .and_then(|e| e.delivery.as_ref())
            .is_some_and(|d| d.armed(occurrence))
    }

    fn complete(&mut self, wait: Wait) {
        match wait.kind {
            WaitKind::Set { endpoint } => {
                let row = wait.rows.first().cloned().flatten();
                let op = wait.ops[0];
                let outcome = ops::outcome_of(row.as_ref(), op, |row| {
                    ops::alarm_set_of(row, |occurrence| self.armed(&endpoint, occurrence))
                });
                for deliver in wait.delivers {
                    match deliver {
                        Deliver::Set(reply) => {
                            let _ = reply.send(outcome.clone());
                        }
                        Deliver::Relay { request_id, shape } => {
                            let json = relay::set_result(&outcome, &shape);
                            self.relay_answer(request_id, json, true);
                        }
                        other => self.refuse(
                            other,
                            Refusal::InvalidRequest("mismatched reply".to_owned()),
                        ),
                    }
                }
            }
            WaitKind::Cancel { root, labels } => {
                let targets: Vec<CancelTarget> = labels
                    .iter()
                    .enumerate()
                    .map(|(i, (schedule, label))| CancelTarget {
                        schedule_id: *schedule,
                        label: label.clone(),
                        outcome: match wait.rows.get(i).cloned().flatten() {
                            None => Outcome::Refused(Refusal::OpIdConflict),
                            row => ops::outcome_of(row.as_ref(), wait.ops[i], |_| ()),
                        },
                    })
                    .collect();
                let outcome = CancelOutcome::Journaled { root, targets };
                for deliver in wait.delivers {
                    match deliver {
                        Deliver::Cancel(reply) => {
                            let _ = reply.send(outcome.clone());
                        }
                        Deliver::Relay { request_id, .. } => {
                            let json = relay::cancel_result(&outcome);
                            self.relay_answer(request_id, json, true);
                        }
                        other => self.refuse(
                            other,
                            Refusal::InvalidRequest("mismatched reply".to_owned()),
                        ),
                    }
                }
            }
            WaitKind::Act { kind } => {
                let op = wait.ops[0];
                let row = wait.rows.first().cloned().flatten();
                let outcome = match ops::outcome_of(row.as_ref(), op, |_| ()) {
                    Outcome::Applied { op_id, .. } => ActOutcome::Applied { op_id, kind },
                    Outcome::Rejected { op_id, reason } => ActOutcome::Rejected { op_id, reason },
                    Outcome::Pending { op_id } => ActOutcome::Pending { op_id },
                    Outcome::Refused(refusal) => ActOutcome::Refused(refusal),
                };
                if !matches!(outcome, ActOutcome::Pending { .. }) {
                    self.remembered_acts.put(op, outcome.clone());
                }
                for deliver in wait.delivers {
                    self.deliver_act(deliver, outcome.clone(), row.as_ref());
                }
            }
        }
    }

    fn deliver_act(&mut self, deliver: Deliver, outcome: ActOutcome, row: Option<&OpRow>) {
        match deliver {
            Deliver::Act(reply) => {
                let _ = reply.send(outcome);
            }
            Deliver::Relay { request_id, shape } => {
                let due = row
                    .and_then(|r| r.result.as_ref())
                    .and_then(|r| r.get("due_local"));
                let json = relay::stop_result(&outcome, &shape, due);
                self.relay_answer(request_id, json, true);
            }
            other => self.refuse(
                other,
                Refusal::InvalidRequest("mismatched reply".to_owned()),
            ),
        }
    }

    // ── Relay ────────────────────────────────────────────────────────────────

    async fn relay(&mut self, data: Value) {
        let Some(request) = relay::parse_request(&data) else {
            return;
        };
        if let Some(result) = self.relay_results.get(&request.request_id).cloned() {
            return self.fire(&request.request_id, &result);
        }
        if !self.relay_open.insert(request.request_id.clone()) {
            return;
        }
        let inbox = self.inbox.clone();
        let deadline = self.config.relay_answer;
        let id = request.request_id.clone();
        self.tasks.spawn(async move {
            tokio::time::sleep(deadline).await;
            let _ = inbox.send(Msg::RelayDeadline(id));
        });
        self.relay_call(request).await;
    }

    async fn relay_call(&mut self, request: RelayRequest) {
        let op = request.op_id();
        let request_id = request.request_id.clone();
        let call = match request.call {
            Ok(call) => call,
            Err(why) => return self.relay_answer(request_id, relay::error_result(&why), true),
        };
        let Some(device) = self.speaker(call.speaker()) else {
            return self.relay_answer(request_id, relay::error_result("which speaker?"), true);
        };
        let label = self.endpoints[&device].config.label.clone();
        match call {
            RelayCall::SetAlarm {
                time, days, name, ..
            } => {
                let request = SetAlarm {
                    op_id: op,
                    endpoint: device,
                    when: crate::schedule::AlarmWhen {
                        time,
                        date: None,
                        repeat: crate::schedule::Repeat::on(days),
                    },
                    label: name,
                    source: crate::api::OpSource::Llm,
                };
                let shape = RelayShape::Set { speaker: label };
                self.set_alarm(request, Deliver::Relay { request_id, shape })
                    .await;
            }
            RelayCall::ListAlarms { .. } => {
                let result = match self.list(&device, 10) {
                    Ok(entries) => relay::list_result(&label, &entries),
                    Err(refusal) => relay::error_result(&refusal.to_string()),
                };
                self.relay_answer(request_id, result, true);
            }
            RelayCall::CancelAlarm { selector, .. } => {
                let request = CancelAlarms {
                    op_id: op,
                    endpoint: device,
                    selector,
                    source: crate::api::OpSource::Llm,
                };
                self.cancel_alarms(
                    request,
                    Deliver::Relay {
                        request_id,
                        shape: RelayShape::Cancel,
                    },
                )
                .await;
            }
            RelayCall::DismissAlert { .. } | RelayCall::SnoozeAlarm { .. } => {
                let snooze = matches!(call, RelayCall::SnoozeAlarm { .. });
                let active = self.endpoints[&device]
                    .state
                    .as_ref()
                    .and_then(|s| s.active.clone());
                let Some(active) = active else {
                    return self.relay_answer(
                        request_id,
                        relay::error_result("nothing is ringing"),
                        true,
                    );
                };
                let shape = RelayShape::Stop {
                    kind: active.kind,
                    name: active.name.clone(),
                    snooze,
                };
                let act = Act {
                    op_id: op,
                    target: crate::api::RingRef {
                        device,
                        ring_id: active.id,
                    },
                    action: if snooze {
                        ActAction::Snooze
                    } else {
                        ActAction::Dismiss
                    },
                    source: ActSource::Llm,
                };
                self.act(act, Deliver::Relay { request_id, shape }).await;
            }
        }
    }

    /// The speaker a tool call means: the one it names (by label, then area), else the
    /// single one with a turn waiting on an intent run.
    fn speaker(&self, named: Option<&str>) -> Option<DeviceId> {
        if let Some(named) = named {
            let by = |pick: &dyn Fn(&Endpoint) -> Option<&str>| {
                let found: Vec<&DeviceId> = self
                    .endpoints
                    .values()
                    .filter(|e| {
                        pick(e).is_some_and(|n| n.trim().eq_ignore_ascii_case(named.trim()))
                    })
                    .map(|e| &e.config.device)
                    .collect();
                (found.len() == 1).then(|| found[0].clone())
            };
            return by(&|e| Some(e.config.label.as_str()))
                .or_else(|| by(&|e| e.config.area.as_deref()));
        }
        let waiting: Vec<&DeviceId> = self
            .endpoints
            .values()
            .filter(|e| e.intent_waits > 0)
            .map(|e| &e.config.device)
            .collect();
        (waiting.len() == 1).then(|| waiting[0].clone())
    }

    /// Records a relay result and fires it if the request is still unanswered.
    fn relay_answer(&mut self, request_id: String, result: Value, final_answer: bool) {
        if final_answer {
            // A result remembered earlier for this request is replaced by the final one.
            self.relay_results.put(request_id.clone(), result.clone());
        }
        if self.relay_open.remove(&request_id) {
            self.fire(&request_id, &result);
        }
    }

    fn relay_deadline(&mut self, request_id: String) {
        if !self.relay_open.contains(&request_id) {
            return;
        }
        let op = OpId::relay(&request_id);
        self.relay_answer(request_id, relay::unconfirmed_result(op), false);
    }

    fn fire(&mut self, request_id: &str, result: &Value) {
        let ha = self.ha.clone();
        let data = relay::result_event(request_id, result);
        self.tasks.spawn(async move {
            let _ = ha.fire_result(data).await;
        });
    }

    // ── Timers ───────────────────────────────────────────────────────────────

    fn timer_event(&mut self, device: &DeviceId, event: TimerEvent) {
        let now = Instant::now();
        let Some(endpoint) = self.endpoints.get_mut(device) else {
            return;
        };
        endpoint.timers.apply(&event, now);
        let config = endpoint.config.clone();
        let view = endpoint
            .capable(Capability::TimerView)
            .then(|| endpoint.timers.view(now));
        match event.kind {
            TimerEventKind::Started => self.prefetch_timer_sound(device),
            TimerEventKind::Finished => {
                let body = CommandBody::Ring(AlertRing {
                    ring_id: event.timer_id.clone(),
                    name: event.name.clone().unwrap_or_default(),
                    sound: config.timer_sound.clone(),
                    loop_gap_ms: config.timer_loop_gap_ms,
                    max_ring_ms: config.timer_max_ring_ms,
                });
                let undeliverable = AlertNotice::TimerRingUndeliverable {
                    device: device.clone(),
                    timer_id: event.timer_id.clone(),
                };
                let notice = undeliverable.clone();
                let sent = self.command(device, body, move |outcome| match outcome {
                    CommandOutcome::NotSent(_) => Some(Msg::Notice(notice)),
                    _ => None,
                });
                if !sent {
                    self.notice(undeliverable);
                }
            }
            TimerEventKind::Updated | TimerEventKind::Cancelled => {}
        }
        if let Some(view) = view {
            self.post(device, PostBody::TimerView(view));
        }
        self.publish_timers();
        self.schedule_tick();
    }

    fn schedule_tick(&mut self) {
        let any = self.endpoints.values().any(|e| !e.timers.is_empty());
        self.next_tick = match (any, self.next_tick) {
            (false, _) => None,
            (true, Some(at)) => Some(at),
            (true, None) => Some(Instant::now() + Duration::from_secs(1)),
        };
    }

    fn tick(&mut self) {
        let now = Instant::now();
        self.next_tick = None;
        let grace = self.config.timer_finish_grace;
        let mut posts = Vec::new();
        let mut notices = Vec::new();
        for endpoint in self.endpoints.values_mut() {
            if endpoint.timers.is_empty() {
                continue;
            }
            for missed in endpoint.timers.sweep(now, grace) {
                notices.push(AlertNotice::TimerFinishMissed {
                    device: endpoint.config.device.clone(),
                    timer_id: missed.timer_id,
                    name: missed.name,
                });
            }
            if endpoint.capable(Capability::TimerView) {
                posts.push((endpoint.config.device.clone(), endpoint.timers.view(now)));
            }
        }
        for notice in notices {
            self.notice(notice);
        }
        for (device, view) in posts {
            self.post(&device, PostBody::TimerView(view));
        }
        self.publish_timers();
        self.schedule_tick();
    }
}

/// Hands an occurrence an applied operation already handled to the applier for
/// deleting again (the restore guard).
fn guard_delete(endpoint: &mut Endpoint, entity: &str, occurrence: &Occurrence) {
    if !endpoint.guarding.insert(occurrence.occurrence_id) {
        return;
    }
    let target = if endpoint.cancelled.contains(&occurrence.schedule_id) {
        echomuse_ha::EventTarget {
            uid: occurrence.uid.clone(),
            recurrence_id: None,
        }
    } else {
        ops::occurrence_target(occurrence)
    };
    let _ = endpoint.applier.send(ApplierJob::GuardDelete {
        entity: entity.to_owned(),
        target,
        occurrence: occurrence.occurrence_id,
    });
}

fn pending_effect(row: &OpRow) -> PendingEffect {
    match (row.action, row.schedule_id, &row.occurrence_key) {
        (Some(journal::Action::Cancel), Some(schedule), _) => PendingEffect::Schedule(schedule),
        (Some(action), Some(schedule), Some(key)) if action.is_terminal() => {
            PendingEffect::Occurrence {
                schedule,
                key: key.clone(),
                reason: tombstone_for(action),
            }
        }
        _ => PendingEffect::Nothing,
    }
}

/// The calendar event of a snooze child the device derived: due when the device said,
/// carrying the snooze's operation id and the parent's ring settings.
fn snooze_child_event(
    parent: &Occurrence,
    op: Uuid,
    child: &crate::wire::ChildRef,
) -> echomuse_ha::EventWrite {
    let line = EchomuseLine {
        id: child.schedule_id,
        kind: OccurrenceKind::Snooze,
        op,
        parent: Some(ParentRef {
            id: parent.schedule_id,
            occ: parent.key.clone(),
        }),
        ring: parent.ring.clone(),
    };
    let summary: String = format!("Snoozed: {}", parent.summary)
        .chars()
        .take(echomuse_protocol::alerts::MAX_LABEL_CHARS)
        .collect();
    let end = crate::schedule::shift_same_offset(&child.due_local, 60_000)
        .unwrap_or_else(|| child.due_local.clone());
    echomuse_ha::EventWrite {
        summary,
        dtstart: child.due_local.clone(),
        dtend: end,
        description: Some(line.description(None)),
        rrule: None,
    }
}
