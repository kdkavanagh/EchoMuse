//! Simulated Home Assistant, device link and clock for the alert service, and a temporary
//! database built from the contract schema. No row or event comes from a real
//! installation.
//!
//! The simulated HA keeps Local Calendars in memory and expands `FREQ=DAILY` and
//! `FREQ=WEEKLY;BYDAY=…` series in its time zone the way HA's calendar library does for
//! these rules (a local time skipped in spring rings after the gap). After every write it
//! pushes the full window to each subscriber, as HA's debounced listener does.

#![allow(dead_code)]

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use echomuse_alerts::{
    AlertClock, AlertConfig, AlertHa, AlertHandle, AlertLink, AlertService, CommandOutcome,
    LinkRefusal, SessionRef,
    ports::{CalendarFeed, RelayFeed},
    schedule::{HaTimeZone, local_text, parse_instant},
    wire::{CommandBody, PostBody},
};
use echomuse_core::{
    services::{EffectOutcome, HaConnectionGeneration, HaError, NotDispatched, UnknownCause},
    types::{DeviceId, SessionId},
};
use echomuse_db::{DatabaseService, DbHandle, OpenedDatabase, ServiceConfig};
use echomuse_ha::{
    CalendarItem, CalendarPush, CalendarTime, ConfigEntry, EventTarget, EventWrite, HaStatus,
};
use echomuse_protocol::vocab::AckStatus;
use jiff::{Timestamp, civil::Weekday};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

pub const PATIENCE: Duration = Duration::from_secs(10);

/// Awaits `future`, failing the test when it takes unreasonably long.
pub async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(PATIENCE, future)
        .await
        .expect("the simulated exchange stalled")
}

// ── Clock ────────────────────────────────────────────────────────────────────

/// Wall time the test sets.
#[derive(Debug)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    pub fn at(text: &str) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(
            parse_instant(text).unwrap().as_millisecond(),
        )))
    }

    pub fn set(&self, text: &str) {
        self.0.store(
            parse_instant(text).unwrap().as_millisecond(),
            Ordering::SeqCst,
        );
    }

    pub fn advance(&self, by: Duration) {
        self.0.fetch_add(by.as_millis() as i64, Ordering::SeqCst);
    }
}

impl AlertClock for ManualClock {
    fn now_utc_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

// ── Database ─────────────────────────────────────────────────────────────────

pub struct TempDb {
    pub dir: tempfile::TempDir,
    pub service: Option<DatabaseService>,
    pub handle: DbHandle,
}

impl TempDb {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().join("echomuse.db");
        let opened = OpenedDatabase::open(&path).unwrap();
        let service = DatabaseService::start(opened, ServiceConfig::default()).unwrap();
        let handle = service.handle();
        Self {
            dir,
            service: Some(service),
            handle,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join("echomuse.db")
    }

    /// A direct read-only look at the file.
    pub fn query<T>(
        &self,
        sql: &str,
        read: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Vec<T> {
        let conn = rusqlite::Connection::open_with_flags(
            self.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut statement = conn.prepare(sql).unwrap();
        statement
            .query_map([], read)
            .unwrap()
            .collect::<rusqlite::Result<Vec<T>>>()
            .unwrap()
    }
}

// ── Simulated Home Assistant ─────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct SimEvent {
    pub uid: String,
    pub summary: String,
    pub start: Timestamp,
    pub description: Option<String>,
    pub rrule: Option<String>,
    pub excluded: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct SimCalendar {
    entry_id: String,
    title: String,
    events: Vec<SimEvent>,
}

/// How the next create or delete misbehaves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    /// HA applies it, but the reply is lost and the connection drops.
    ApplyThenDrop,
    /// The request is lost with the connection before HA applies it.
    DropBeforeApply,
    /// HA refuses it.
    Refuse,
}

struct Subscriber {
    entity: String,
    start: Timestamp,
    end: Timestamp,
    pushes: mpsc::UnboundedSender<CalendarPush>,
}

#[derive(Default)]
pub struct SimState {
    calendars: BTreeMap<String, SimCalendar>,
    subscribers: Vec<Subscriber>,
    relay: Vec<mpsc::UnboundedSender<Value>>,
    next_uid: u64,
    pub creates: usize,
    pub deletes: Vec<EventTarget>,
    pub create_fault: Option<Fault>,
    pub delete_fault: Option<Fault>,
    pub scripts: BTreeMap<String, Value>,
    pub script_writes: Vec<String>,
    pub reloads: usize,
    pub exposed: Vec<String>,
    pub calendars_created: usize,
}

pub struct SimHa {
    pub zone: HaTimeZone,
    status: watch::Sender<HaStatus>,
    generation: Mutex<u64>,
    pub state: Mutex<SimState>,
    fired: mpsc::UnboundedSender<Value>,
}

pub struct SimCalendarFeed(mpsc::UnboundedReceiver<CalendarPush>);

impl CalendarFeed for SimCalendarFeed {
    async fn next(&mut self) -> Option<CalendarPush> {
        self.0.recv().await
    }
}

pub struct SimRelayFeed(mpsc::UnboundedReceiver<Value>);

impl RelayFeed for SimRelayFeed {
    async fn next(&mut self) -> Option<Value> {
        self.0.recv().await
    }
}

fn weekday_code(day: Weekday) -> &'static str {
    match day {
        Weekday::Monday => "MO",
        Weekday::Tuesday => "TU",
        Weekday::Wednesday => "WE",
        Weekday::Thursday => "TH",
        Weekday::Friday => "FR",
        Weekday::Saturday => "SA",
        Weekday::Sunday => "SU",
    }
}

impl SimHa {
    /// An HA in `zone`, connected, with no calendars. The receiver gets every
    /// `echomuse_alert_result` it fires.
    pub fn new(zone: &str) -> (Arc<Self>, mpsc::UnboundedReceiver<Value>) {
        let (fired, results) = mpsc::unbounded_channel();
        let (status, _) = watch::channel(HaStatus {
            generation: Some(HaConnectionGeneration(1)),
            ha_version: Some("2026.8.1".to_owned()),
            last_fault: None,
        });
        let ha = Arc::new(Self {
            zone: HaTimeZone::named(zone).unwrap(),
            status,
            generation: Mutex::new(1),
            state: Mutex::new(SimState::default()),
            fired,
        });
        (ha, results)
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, SimState> {
        self.state.lock().unwrap()
    }

    /// The connection drops: subscriptions end.
    pub fn disconnect(&self) {
        let mut state = self.state();
        state.subscribers.clear();
        state.relay.clear();
        drop(state);
        self.status.send_modify(|s| s.generation = None);
    }

    /// A new connection.
    pub fn reconnect(&self) {
        let mut generation = self.generation.lock().unwrap();
        *generation += 1;
        let g = *generation;
        self.status
            .send_modify(|s| s.generation = Some(HaConnectionGeneration(g)));
    }

    fn connected(&self) -> bool {
        self.status.borrow().generation.is_some()
    }

    /// Adds a calendar entry and entity, as if made earlier.
    pub fn add_calendar(&self, title: &str, entity: &str) {
        let mut state = self.state();
        let entry_id = format!("entry-{entity}");
        state.calendars.insert(
            entity.to_owned(),
            SimCalendar {
                entry_id,
                title: title.to_owned(),
                events: Vec::new(),
            },
        );
    }

    /// Replaces every event of `entity` (a restore from a backup, or deletions in HA's
    /// UI).
    pub fn replace_events(&self, entity: &str, events: Vec<SimEvent>) {
        self.state()
            .calendars
            .get_mut(entity)
            .expect("calendar exists")
            .events = events;
        self.push_all(entity);
    }

    /// Adds an event directly (HA's UI, or a restore from a backup).
    pub fn put_event(&self, entity: &str, event: SimEvent) {
        let mut state = self.state();
        state
            .calendars
            .get_mut(entity)
            .expect("calendar exists")
            .events
            .push(event);
        drop(state);
        self.push_all(entity);
    }

    pub fn events(&self, entity: &str) -> Vec<SimEvent> {
        self.state()
            .calendars
            .get(entity)
            .map(|c| c.events.clone())
            .unwrap_or_default()
    }

    /// The expanded instances of `entity` between `start` and `end`.
    pub fn items(&self, entity: &str, start: Timestamp, end: Timestamp) -> Vec<CalendarItem> {
        let state = self.state();
        let Some(calendar) = state.calendars.get(entity) else {
            return Vec::new();
        };
        let mut items = Vec::new();
        for event in &calendar.events {
            for (instant, recurrence_id) in self.instances(event, start, end) {
                let zoned = instant.to_zoned(jiff::tz::TimeZone::get(self.zone.name()).unwrap());
                let end_text = local_text(
                    &(instant + jiff::SignedDuration::from_secs(60))
                        .to_zoned(jiff::tz::TimeZone::get(self.zone.name()).unwrap()),
                );
                items.push(CalendarItem {
                    summary: event.summary.clone(),
                    start: CalendarTime::DateTime(local_text(&zoned)),
                    end: CalendarTime::DateTime(end_text),
                    description: event.description.clone(),
                    uid: Some(event.uid.clone()),
                    recurrence_id,
                    rrule: event.rrule.clone(),
                });
            }
        }
        items.sort_by(|a, b| format!("{:?}", a.start).cmp(&format!("{:?}", b.start)));
        items
    }

    fn instances(
        &self,
        event: &SimEvent,
        start: Timestamp,
        end: Timestamp,
    ) -> Vec<(Timestamp, Option<String>)> {
        let tz = jiff::tz::TimeZone::get(self.zone.name()).unwrap();
        let Some(rule) = &event.rrule else {
            return if event.start >= start && event.start < end {
                vec![(event.start, None)]
            } else {
                Vec::new()
            };
        };
        let first = event.start.to_zoned(tz.clone());
        let time = first.time();
        let days: Option<Vec<&str>> = rule
            .strip_prefix("FREQ=WEEKLY;BYDAY=")
            .map(|d| d.split(',').collect());
        assert!(
            days.is_some() || rule == "FREQ=DAILY",
            "simulated rule {rule}"
        );
        let mut date = first.date();
        let last = end.to_zoned(tz.clone()).date();
        let mut out = Vec::new();
        while date <= last {
            let allowed = days
                .as_ref()
                .is_none_or(|days| days.contains(&weekday_code(date.weekday())));
            let local = date.to_datetime(time);
            let recurrence_id = local.strftime("%Y%m%dT%H%M%S").to_string();
            if allowed && !event.excluded.contains(&recurrence_id) {
                let instant = local.to_zoned(tz.clone()).unwrap().timestamp();
                if instant >= start && instant < end && instant >= event.start {
                    out.push((instant, Some(recurrence_id)));
                }
            }
            date = date.tomorrow().unwrap();
        }
        out
    }

    fn push_all(&self, entity: &str) {
        let subscribers: Vec<(Timestamp, Timestamp, mpsc::UnboundedSender<CalendarPush>)> = self
            .state()
            .subscribers
            .iter()
            .filter(|s| s.entity == entity)
            .map(|s| (s.start, s.end, s.pushes.clone()))
            .collect();
        for (start, end, pushes) in subscribers {
            let _ = pushes.send(CalendarPush::Events(self.items(entity, start, end)));
        }
    }

    /// An `echomuse_alert_request` event.
    pub fn relay_request(&self, data: Value) {
        for relay in &self.state().relay {
            let _ = relay.send(data.clone());
        }
    }

    pub fn relay_subscribers(&self) -> usize {
        self.state().relay.len()
    }

    pub fn subscribers(&self, entity: &str) -> usize {
        self.state()
            .subscribers
            .iter()
            .filter(|s| s.entity == entity && !s.pushes.is_closed())
            .count()
    }

    fn fault(&self, fault: Option<Fault>) -> Option<EffectOutcome<()>> {
        match fault? {
            Fault::Refuse => Some(EffectOutcome::Rejected {
                code: "failed".to_owned(),
                message: "simulated refusal".to_owned(),
            }),
            Fault::ApplyThenDrop | Fault::DropBeforeApply => {
                self.disconnect();
                Some(EffectOutcome::Unknown(UnknownCause::ConnectionLost))
            }
        }
    }
}

impl AlertHa for SimHa {
    type Calendar = SimCalendarFeed;
    type Relay = SimRelayFeed;

    fn status(&self) -> watch::Receiver<HaStatus> {
        self.status.subscribe()
    }

    async fn time_zone(&self) -> Result<String, HaError> {
        if !self.connected() {
            return Err(HaError::NotDispatched(NotDispatched::Unavailable));
        }
        Ok(self.zone.name().to_owned())
    }

    async fn local_calendars(&self) -> Result<Vec<ConfigEntry>, HaError> {
        Ok(self
            .state()
            .calendars
            .values()
            .map(|c| ConfigEntry {
                entry_id: c.entry_id.clone(),
                title: c.title.clone(),
            })
            .collect())
    }

    async fn calendar_entities(&self, entry_id: String) -> Result<Vec<String>, HaError> {
        Ok(self
            .state()
            .calendars
            .iter()
            .filter(|(_, c)| c.entry_id == entry_id)
            .map(|(entity, _)| entity.clone())
            .collect())
    }

    async fn create_local_calendar(&self, title: String) -> EffectOutcome<ConfigEntry> {
        let entity = format!("calendar.{}", title.to_lowercase().replace(' ', "_"));
        self.add_calendar(&title, &entity);
        self.state().calendars_created += 1;
        EffectOutcome::Applied(ConfigEntry {
            entry_id: format!("entry-{entity}"),
            title,
        })
    }

    async fn subscribe_calendar(
        &self,
        entity: String,
        start: String,
        end: String,
    ) -> Result<SimCalendarFeed, HaError> {
        if !self.connected() {
            return Err(HaError::NotDispatched(NotDispatched::Unavailable));
        }
        let (pushes, rx) = mpsc::unbounded_channel();
        let start = parse_instant(&start).unwrap();
        let end = parse_instant(&end).unwrap();
        let _ = pushes.send(CalendarPush::Events(self.items(&entity, start, end)));
        self.state().subscribers.push(Subscriber {
            entity,
            start,
            end,
            pushes,
        });
        Ok(SimCalendarFeed(rx))
    }

    async fn unsubscribe_calendar(&self, _feed: SimCalendarFeed) {}

    async fn calendar_events(
        &self,
        entity: String,
        start: String,
        end: String,
    ) -> Result<Vec<CalendarItem>, HaError> {
        if !self.connected() {
            return Err(HaError::NotDispatched(NotDispatched::Unavailable));
        }
        Ok(self.items(
            &entity,
            parse_instant(&start).unwrap(),
            parse_instant(&end).unwrap(),
        ))
    }

    async fn create_event(&self, entity: String, event: EventWrite) -> EffectOutcome<()> {
        if !self.connected() {
            return EffectOutcome::NotDispatched(NotDispatched::Unavailable);
        }
        let fault = self.state().create_fault.take();
        if fault == Some(Fault::DropBeforeApply) || fault == Some(Fault::Refuse) {
            return self.fault(fault).unwrap();
        }
        {
            let mut state = self.state();
            state.creates += 1;
            state.next_uid += 1;
            let uid = format!("uid-{}", state.next_uid);
            let start = parse_instant(&event.dtstart).expect("dtstart with offset");
            assert!(
                parse_instant(&event.dtend).unwrap().as_millisecond() - start.as_millisecond()
                    == 60_000,
                "one-minute events"
            );
            state
                .calendars
                .get_mut(&entity)
                .expect("calendar exists")
                .events
                .push(SimEvent {
                    uid,
                    summary: event.summary,
                    start,
                    description: event.description,
                    rrule: event.rrule,
                    excluded: BTreeSet::new(),
                });
        }
        if let Some(outcome) = self.fault(fault) {
            return outcome;
        }
        self.push_all(&entity);
        EffectOutcome::Applied(())
    }

    async fn delete_event(&self, entity: String, target: EventTarget) -> EffectOutcome<()> {
        if !self.connected() {
            return EffectOutcome::NotDispatched(NotDispatched::Unavailable);
        }
        let fault = self.state().delete_fault.take();
        if fault == Some(Fault::DropBeforeApply) || fault == Some(Fault::Refuse) {
            return self.fault(fault).unwrap();
        }
        let found = {
            let mut state = self.state();
            state.deletes.push(target.clone());
            let calendar = state.calendars.get_mut(&entity).expect("calendar exists");
            match calendar.events.iter().position(|e| e.uid == target.uid) {
                None => false,
                Some(index) => {
                    match &target.recurrence_id {
                        None => {
                            calendar.events.remove(index);
                        }
                        Some(id) => {
                            calendar.events[index].excluded.insert(id.clone());
                        }
                    }
                    true
                }
            }
        };
        if let Some(outcome) = self.fault(fault) {
            return outcome;
        }
        if !found {
            return EffectOutcome::Rejected {
                code: "failed".to_owned(),
                message: "Error while deleting event: not found".to_owned(),
            };
        }
        self.push_all(&entity);
        EffectOutcome::Applied(())
    }

    async fn script_config(&self, object_id: String) -> Result<Option<Value>, HaError> {
        Ok(self.state().scripts.get(&object_id).cloned())
    }

    async fn write_script_config(&self, object_id: String, config: Value) -> EffectOutcome<()> {
        let mut state = self.state();
        state.script_writes.push(object_id.clone());
        state.scripts.insert(object_id, config);
        EffectOutcome::Applied(())
    }

    async fn reload_scripts(&self) -> EffectOutcome<()> {
        self.state().reloads += 1;
        EffectOutcome::Applied(())
    }

    async fn expose_scripts(&self, entity_ids: Vec<String>) -> EffectOutcome<()> {
        self.state().exposed = entity_ids;
        EffectOutcome::Applied(())
    }

    async fn subscribe_relay(&self) -> Result<SimRelayFeed, HaError> {
        if !self.connected() {
            return Err(HaError::NotDispatched(NotDispatched::Unavailable));
        }
        let (events, rx) = mpsc::unbounded_channel();
        self.state().relay.push(events);
        Ok(SimRelayFeed(rx))
    }

    async fn fire_result(&self, data: Value) -> EffectOutcome<()> {
        let _ = self.fired.send(data);
        EffectOutcome::Applied(())
    }
}

// ── Simulated device link ────────────────────────────────────────────────────

/// One `alert.act` sent to a device, waiting for the test to acknowledge it.
pub struct SentCommand {
    pub to: SessionRef,
    pub body: CommandBody,
    reply: oneshot::Sender<CommandOutcome>,
}

impl SentCommand {
    pub fn ack(self, status: AckStatus, error: Option<&str>) {
        let _ = self.reply.send(CommandOutcome::Acked {
            status,
            error: error.map(str::to_owned),
        });
    }

    pub fn lose(self) {
        let _ = self
            .reply
            .send(CommandOutcome::Unknown(UnknownCause::ConnectionLost));
    }
}

/// Devices' sessions. Posts and acknowledged commands other than `alert.act` are
/// recorded and acknowledged `accepted`; each `alert.act` waits for the test.
pub struct SimLink {
    current: Mutex<HashMap<String, SessionId>>,
    posts: mpsc::UnboundedSender<(SessionRef, PostBody)>,
    acts: mpsc::UnboundedSender<SentCommand>,
    pub others: Mutex<Vec<(SessionRef, CommandBody)>>,
}

pub struct LinkProbe {
    pub posts: mpsc::UnboundedReceiver<(SessionRef, PostBody)>,
    pub acts: mpsc::UnboundedReceiver<SentCommand>,
}

impl SimLink {
    pub fn new() -> (Arc<Self>, LinkProbe) {
        let (posts, posts_rx) = mpsc::unbounded_channel();
        let (acts, acts_rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                current: Mutex::new(HashMap::new()),
                posts,
                acts,
                others: Mutex::new(Vec::new()),
            }),
            LinkProbe {
                posts: posts_rx,
                acts: acts_rx,
            },
        )
    }

    pub fn connect(&self, device: &str, session: u64) {
        self.current
            .lock()
            .unwrap()
            .insert(device.to_owned(), SessionId(session));
    }

    pub fn disconnect(&self, device: &str) {
        self.current.lock().unwrap().remove(device);
    }

    fn is_current(&self, to: &SessionRef) -> bool {
        self.current.lock().unwrap().get(&to.device.0) == Some(&to.session)
    }
}

impl AlertLink for SimLink {
    fn post(&self, to: &SessionRef, body: PostBody) -> Result<(), LinkRefusal> {
        if !self.is_current(to) {
            return Err(LinkRefusal::NotCurrent);
        }
        let _ = self.posts.send((to.clone(), body));
        Ok(())
    }

    async fn command(&self, to: &SessionRef, body: CommandBody) -> CommandOutcome {
        if !self.is_current(to) {
            return CommandOutcome::NotSent(LinkRefusal::NotCurrent);
        }
        if let CommandBody::Act(_) = &body {
            let (reply, answer) = oneshot::channel();
            let _ = self.acts.send(SentCommand {
                to: to.clone(),
                body,
                reply,
            });
            return answer
                .await
                .unwrap_or(CommandOutcome::Unknown(UnknownCause::ConnectionLost));
        }
        self.others.lock().unwrap().push((to.clone(), body));
        CommandOutcome::Acked {
            status: AckStatus::Accepted,
            error: None,
        }
    }
}

impl LinkProbe {
    pub async fn post(&mut self) -> (SessionRef, PostBody) {
        within(self.posts.recv()).await.expect("link closed")
    }

    /// The next post that is not a timer view.
    pub async fn alert_post(&mut self) -> (SessionRef, PostBody) {
        loop {
            let (to, body) = self.post().await;
            if !matches!(body, PostBody::TimerView(_)) {
                return (to, body);
            }
        }
    }

    pub async fn act(&mut self) -> SentCommand {
        within(self.acts.recv()).await.expect("link closed")
    }

    pub fn drain(&mut self) -> Vec<(SessionRef, PostBody)> {
        let mut out = Vec::new();
        while let Ok(post) = self.posts.try_recv() {
            out.push(post);
        }
        out
    }
}

/// Timing for tests: short waits, real monotonic time.
pub fn test_config() -> AlertConfig {
    AlertConfig {
        settle_wait: Duration::from_secs(3),
        confirm_wait: Duration::from_millis(500),
        relay_answer: Duration::from_secs(3),
        retry_initial: Duration::from_millis(20),
        retry_max: Duration::from_millis(100),
        timer_finish_grace: Duration::from_millis(200),
        ..AlertConfig::default()
    }
}

pub fn device(id: &str) -> DeviceId {
    DeviceId(id.to_owned())
}

/// A service over the given pieces.
pub fn start(
    config: AlertConfig,
    db: &TempDb,
    ha: &Arc<SimHa>,
    link: &Arc<SimLink>,
    clock: &Arc<ManualClock>,
) -> (
    AlertHandle,
    tokio::task::JoinHandle<Result<(), echomuse_alerts::ServiceFailure>>,
) {
    AlertService::start(
        config,
        db.handle.clone(),
        ha.clone(),
        link.clone(),
        clock.clone(),
    )
}
