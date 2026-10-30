//! The alert service under the controller's task supervisor: a panic in one of the
//! service's own tasks ends the service with an error, which fires the fatal hook the
//! moment it is returned, not when the controller next shuts down; a service stopped on
//! purpose fires nothing.

use std::{sync::Arc, time::Duration};

use echomuse_alerts::{
    AlertConfig, AlertHa, AlertLink, AlertService, CommandOutcome, LinkRefusal, ServiceFailure,
    SessionRef, SystemClock,
    ports::{CalendarFeed, RelayFeed},
    wire::{CommandBody, PostBody},
};
use echomuse_controller::supervision::{FatalHook, TaskSupervisor};
use echomuse_core::services::{EffectOutcome, HaConnectionGeneration, HaError};
use echomuse_db::{DatabaseService, OpenedDatabase, ServiceConfig};
use echomuse_ha::{CalendarItem, CalendarPush, ConfigEntry, EventTarget, EventWrite, HaStatus};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

const WAIT: Duration = Duration::from_secs(10);
const INJECTED: &str = "injected Home Assistant panic";

/// Home Assistant as the alert service's session task sees it. Connected, its first
/// call (the time zone) panics; disconnected, the service never calls it.
struct PanickingHa {
    status: watch::Sender<HaStatus>,
}

impl PanickingHa {
    fn new(connected: bool) -> Arc<Self> {
        let (status, _) = watch::channel(HaStatus {
            generation: connected.then_some(HaConnectionGeneration(1)),
            ..HaStatus::default()
        });
        Arc::new(Self { status })
    }
}

const UNUSED: &str = "the alert service asks for the time zone first";

impl AlertHa for PanickingHa {
    type Calendar = NoFeed;
    type Relay = NoFeed;

    fn status(&self) -> watch::Receiver<HaStatus> {
        self.status.subscribe()
    }

    async fn time_zone(&self) -> Result<String, HaError> {
        panic!("{INJECTED}")
    }

    async fn local_calendars(&self) -> Result<Vec<ConfigEntry>, HaError> {
        unreachable!("{UNUSED}")
    }

    async fn calendar_entities(&self, _entry_id: String) -> Result<Vec<String>, HaError> {
        unreachable!("{UNUSED}")
    }

    async fn create_local_calendar(&self, _title: String) -> EffectOutcome<ConfigEntry> {
        unreachable!("{UNUSED}")
    }

    async fn subscribe_calendar(
        &self,
        _entity: String,
        _start: String,
        _end: String,
    ) -> Result<NoFeed, HaError> {
        unreachable!("{UNUSED}")
    }

    async fn unsubscribe_calendar(&self, _feed: NoFeed) {
        unreachable!("{UNUSED}")
    }

    async fn calendar_events(
        &self,
        _entity: String,
        _start: String,
        _end: String,
    ) -> Result<Vec<CalendarItem>, HaError> {
        unreachable!("{UNUSED}")
    }

    async fn create_event(&self, _entity: String, _event: EventWrite) -> EffectOutcome<()> {
        unreachable!("{UNUSED}")
    }

    async fn delete_event(&self, _entity: String, _target: EventTarget) -> EffectOutcome<()> {
        unreachable!("{UNUSED}")
    }

    async fn script_config(&self, _object_id: String) -> Result<Option<Value>, HaError> {
        unreachable!("{UNUSED}")
    }

    async fn write_script_config(&self, _object_id: String, _config: Value) -> EffectOutcome<()> {
        unreachable!("{UNUSED}")
    }

    async fn reload_scripts(&self) -> EffectOutcome<()> {
        unreachable!("{UNUSED}")
    }

    async fn expose_scripts(&self, _entity_ids: Vec<String>) -> EffectOutcome<()> {
        unreachable!("{UNUSED}")
    }

    async fn subscribe_relay(&self) -> Result<NoFeed, HaError> {
        unreachable!("{UNUSED}")
    }

    async fn fire_result(&self, _data: Value) -> EffectOutcome<()> {
        unreachable!("{UNUSED}")
    }
}

/// A subscription that has ended.
struct NoFeed;

impl CalendarFeed for NoFeed {
    async fn next(&mut self) -> Option<CalendarPush> {
        None
    }
}

impl RelayFeed for NoFeed {
    async fn next(&mut self) -> Option<Value> {
        None
    }
}

/// A device link with no device on it.
struct NoDevices;

impl AlertLink for NoDevices {
    fn post(&self, _to: &SessionRef, _body: PostBody) -> Result<(), LinkRefusal> {
        Err(LinkRefusal::NotCurrent)
    }

    async fn command(&self, _to: &SessionRef, _body: CommandBody) -> CommandOutcome {
        CommandOutcome::NotSent(LinkRefusal::NotCurrent)
    }
}

/// A supervisor whose fatal hook records each reason instead of ending the process.
fn supervisor() -> (TaskSupervisor, mpsc::UnboundedReceiver<String>) {
    let (sender, reasons) = mpsc::unbounded_channel();
    let hook = FatalHook::new(move |reason| {
        let _ = sender.send(reason.to_owned());
    });
    (TaskSupervisor::new(hook), reasons)
}

fn database(dir: &tempfile::TempDir) -> DatabaseService {
    DatabaseService::start(
        OpenedDatabase::open(dir.path().join("echomuse.db")).unwrap(),
        ServiceConfig::default(),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panic_inside_the_alert_service_fires_the_fatal_hook_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    let (tasks, mut reasons) = supervisor();
    let (_alerts, task) = AlertService::start(
        AlertConfig::default(),
        db.handle(),
        PanickingHa::new(true),
        Arc::new(NoDevices),
        Arc::new(SystemClock),
    );
    let task = tasks.observe_fallible("alert service", task);

    // Nothing shuts the service down or waits on its task: the hook fires by itself.
    let reason = tokio::time::timeout(WAIT, reasons.recv())
        .await
        .expect("the fatal hook fired while the service was left running")
        .unwrap();
    assert!(reason.contains("alert service"), "{reason}");
    assert!(reason.contains(INJECTED), "{reason}");

    // Whoever waits on the task still gets the service's own error.
    let ended = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert!(
        matches!(&ended, Err(ServiceFailure::TaskPanicked(what)) if what.contains(INJECTED)),
        "{ended:?}"
    );
    assert!(reasons.try_recv().is_err(), "fired once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_alert_service_shut_down_on_purpose_fires_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    let (tasks, mut reasons) = supervisor();
    let (alerts, task) = AlertService::start(
        AlertConfig::default(),
        db.handle(),
        PanickingHa::new(false),
        Arc::new(NoDevices),
        Arc::new(SystemClock),
    );
    let task = tasks.observe_fallible("alert service", task);

    alerts.shutdown();
    let ended = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert!(ended.is_ok(), "{ended:?}");
    assert!(reasons.try_recv().is_err(), "a clean stop is not a failure");
}
