//! The alert service end to end against a simulated Home Assistant, simulated devices, a
//! controlled wall clock in America/Chicago, and temporary databases built from the
//! contract schema.

mod common;

use std::{num::NonZeroU64, time::Duration};

use common::{
    Fault, LinkProbe, ManualClock, SimEvent, SimHa, SimLink, TempDb, device, start, test_config,
    within,
};
use echomuse_alerts::{
    Act, ActOutcome, AlertHandle, AlertNotice, CancelAlarms, CancelOutcome, DeviceAttach,
    EndpointConfig, OpId, OpSource, Outcome, Refusal, RingRef, ScriptState, SetAlarm,
    description::{EchomuseLine, LineOf},
    ids,
    relay::CancelSelector,
    schedule::{AlarmWhen, Repeat, TimeOfDay, Weekdays, parse_instant},
    wire::{ActAction, ActSource, AlertInbound, CommandBody, PostBody},
};
use echomuse_core::{
    services::{TimerEvent, TimerEventKind},
    types::SessionId,
};
use echomuse_protocol::{
    Capabilities, MessageType,
    alerts::{AlertSound, CacheObject, TombstoneReason, snapshot_digest},
    session::HelloAlerts,
    vocab::AckStatus,
};
use serde_json::{Value, json};
use uuid::Uuid;

const KITCHEN: &str = "G0SYNTHETIC00001";
const OFFICE: &str = "G0SYNTHETIC00002";
const KITCHEN_CALENDAR: &str = "calendar.echomuse_kitchen";
const OFFICE_CALENDAR: &str = "calendar.echomuse_office";
/// A Wednesday morning in Chicago (CDT), four days before clocks go back.
const NOW: &str = "2026-10-28T08:00:00-05:00";

fn endpoint(id: &str, label: &str) -> EndpointConfig {
    EndpointConfig {
        device: device(id),
        label: label.to_owned(),
        area: Some(format!("{label} area")),
        alarm_sound: AlertSound::Fallback,
        timer_sound: AlertSound::Asset(echomuse_protocol::Sha256::of(b"timer")),
        timer_loop_gap_ms: 2_000,
        timer_max_ring_ms: NonZeroU64::new(900_000).unwrap(),
    }
}

fn capabilities(extra: &[&str]) -> Capabilities {
    let mut names = vec![
        "audio_timeline_v1",
        "uplink_leases_v1",
        "device_wake_v1",
        "render_reference_v1",
        "render_progress_v1",
        "focus_leases_v1",
        "alert_cache_v1",
        "turn_protocol_v1",
    ];
    names.extend_from_slice(extra);
    Capabilities::from_names(names)
}

fn hello(epoch: Option<&str>, acked: u64) -> HelloAlerts {
    HelloAlerts {
        delivery_epoch: epoch.map(str::to_owned),
        acked_sequence: Some(acked),
        wakeup: None,
    }
}

fn inbound(message_type: MessageType, body: Value) -> AlertInbound {
    let Value::Object(body) = body else {
        unreachable!()
    };
    AlertInbound::decode(message_type, body).unwrap().unwrap()
}

fn weekly(time: (u8, u8), days: &[&str]) -> AlarmWhen {
    AlarmWhen {
        time: TimeOfDay::new(time.0, time.1, 0).unwrap(),
        date: None,
        repeat: Repeat::on(Weekdays::from_names(days.iter().copied()).unwrap()),
    }
}

fn once(time: (u8, u8)) -> AlarmWhen {
    weekly(time, &[])
}

fn set(id: &str, op: OpId, when: AlarmWhen, label: &str) -> SetAlarm {
    SetAlarm {
        op_id: op,
        endpoint: device(id),
        when,
        label: Some(label.to_owned()),
        source: OpSource::Voice,
    }
}

/// Waits until the speaker's calendar has been read on the current connection.
async fn ready(handle: &AlertHandle, id: &str) {
    within(async {
        loop {
            if handle.list_alarms(device(id), 1).await.unwrap().is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

/// Waits until `condition` holds.
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let waited = tokio::time::timeout(common::PATIENCE, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "never: {what}");
}

fn op_state(db: &TempDb, op: OpId) -> Option<String> {
    db.query(
        &format!("SELECT state FROM alert_ops WHERE op_id = '{op}'"),
        |row| row.get::<_, String>(0),
    )
    .pop()
}

fn line_of(event: &SimEvent) -> EchomuseLine {
    match EchomuseLine::parse(event.description.as_deref()) {
        LineOf::Valid(line) => line,
        other => panic!("no valid line: {other:?}"),
    }
}

struct World {
    db: TempDb,
    ha: std::sync::Arc<SimHa>,
    results: tokio::sync::mpsc::UnboundedReceiver<Value>,
    link: std::sync::Arc<SimLink>,
    probe: LinkProbe,
    clock: std::sync::Arc<ManualClock>,
    handle: AlertHandle,
}

impl World {
    fn new() -> Self {
        Self::with(test_config())
    }

    fn with(config: echomuse_alerts::AlertConfig) -> Self {
        let db = TempDb::new();
        let (ha, results) = SimHa::new("America/Chicago");
        let (link, probe) = SimLink::new();
        let clock = ManualClock::at(NOW);
        let (handle, _task) = start(config, &db, &ha, &link, &clock);
        Self {
            db,
            ha,
            results,
            link,
            probe,
            clock,
            handle,
        }
    }

    fn restart(&mut self, config: echomuse_alerts::AlertConfig) {
        self.handle.shutdown();
        let (handle, _task) = start(config, &self.db, &self.ha, &self.link, &self.clock);
        self.handle = handle;
    }

    async fn kitchen(&self) {
        self.handle.register(endpoint(KITCHEN, "Kitchen")).unwrap();
        ready(&self.handle, KITCHEN).await;
    }

    fn attach(&self, id: &str, session: u64, hello: Option<HelloAlerts>, extra: &[&str]) {
        self.link.connect(id, session);
        self.handle
            .attach(
                device(id),
                DeviceAttach {
                    session: SessionId(session),
                    capabilities: capabilities(extra),
                    alerts: hello,
                },
            )
            .unwrap();
    }

    fn send(&self, id: &str, session: u64, message: AlertInbound) {
        self.handle
            .device_message(device(id), SessionId(session), message)
            .unwrap();
    }

    fn ringing(&self, id: &str, session: u64, ring: &str, kind: &str) {
        self.send(
            id,
            session,
            inbound(
                MessageType::AlertState,
                json!({"active": {"id": ring, "kind": kind, "name": "Wake up", "foreground": true,
                                  "started_mono_ns": "1", "deadline_mono_ns": "600000000001"},
                       "queue": [], "clock_trusted": true, "wakelock": "held", "store": "ok"}),
            ),
        );
    }

    async fn result(&mut self) -> Value {
        within(self.results.recv()).await.unwrap()
    }
}

// ── Calendar operations ──────────────────────────────────────────────────────

#[tokio::test]
async fn an_alarm_is_created_once_and_a_repeat_answers_from_the_journal() {
    let w = World::new();
    w.kitchen().await;
    // The calendar was created through the config flow under the speaker's title.
    assert_eq!(w.ha.state().calendars_created, 1);

    let op = OpId::voice_alarm("turn-1");
    let request = set(KITCHEN, op, weekly((7, 0), &["mon"]), "Wake up");
    let outcome = within(w.handle.set_alarm(request.clone())).await.unwrap();
    let Outcome::Applied { op_id, result } = &outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(*op_id, op);
    // Monday 2 November is after the autumn change: CST.
    assert_eq!(result.first_due_local, "2026-11-02T07:00:00-06:00");
    assert_eq!(result.schedule_id, ids::new_schedule_id(op));

    let events = w.ha.events(KITCHEN_CALENDAR);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].summary, "Wake up");
    assert_eq!(events[0].rrule.as_deref(), Some("FREQ=WEEKLY;BYDAY=MO"));
    let line = line_of(&events[0]);
    assert_eq!((line.id, line.op), (ids::new_schedule_id(op), op.uuid()));
    assert!(
        events[0]
            .description
            .as_deref()
            .unwrap()
            .starts_with("echomuse: {\"id\":")
    );

    // The same request under the same id is the same operation.
    let again = within(w.handle.set_alarm(request.clone())).await.unwrap();
    assert_eq!(again, outcome);
    assert_eq!(w.ha.state().creates, 1);

    // A different request under that id is refused before anything is written.
    let conflict = set(KITCHEN, op, weekly((8, 0), &["mon"]), "Wake up");
    assert_eq!(
        within(w.handle.set_alarm(conflict)).await.unwrap(),
        Outcome::Refused(Refusal::OpIdConflict)
    );

    let rows = w.db.query(
        "SELECT op_id, action, source, state, applied_ms, payload_sha256, occurrence_key FROM alert_ops",
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        },
    );
    assert_eq!(rows.len(), 1);
    let (id, action, source, state, applied, sha, key) = &rows[0];
    assert_eq!(
        (
            id.as_str(),
            action.as_str(),
            source.as_str(),
            state.as_str()
        ),
        (op.to_string().as_str(), "create", "voice", "applied")
    );
    assert_eq!(*applied, Some(parse_instant(NOW).unwrap().as_millisecond()));
    assert_eq!(sha.len(), 64);
    assert_eq!(*key, None);
}

#[tokio::test]
async fn home_assistant_away_refuses_new_operations_explicitly() {
    let w = World::new();
    w.kitchen().await;
    w.ha.disconnect();
    // The service learns of the loss from the connection status.
    within(async {
        while w.handle.list_alarms(device(KITCHEN), 1).await.unwrap() != Err(Refusal::HaUnavailable)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert_eq!(
        within(
            w.handle
                .set_alarm(set(KITCHEN, OpId::voice_alarm("t2"), once((9, 0)), "Tea"))
        )
        .await
        .unwrap(),
        Outcome::Refused(Refusal::HaUnavailable)
    );
    let cancel = CancelAlarms {
        op_id: OpId::voice_alarm("t3"),
        endpoint: device(KITCHEN),
        selector: CancelSelector::All,
        source: OpSource::Voice,
    };
    assert_eq!(
        within(w.handle.cancel_alarms(cancel)).await.unwrap(),
        CancelOutcome::Refused(Refusal::HaUnavailable)
    );
    assert_eq!(
        w.handle.list_alarms(device(KITCHEN), 10).await.unwrap(),
        Err(Refusal::HaUnavailable)
    );
    assert!(
        w.db.query("SELECT op_id FROM alert_ops", |row| row.get::<_, String>(0))
            .is_empty(),
        "nothing was journaled"
    );
}

#[tokio::test]
async fn a_pending_create_recovers_after_a_restart_without_a_duplicate_event() {
    for fault in [Fault::ApplyThenDrop, Fault::DropBeforeApply] {
        let short = echomuse_alerts::AlertConfig {
            settle_wait: Duration::from_millis(300),
            ..test_config()
        };
        let mut w = World::with(short);
        w.kitchen().await;
        w.ha.state().create_fault = Some(fault);
        let op = OpId::voice_alarm("turn-recover");
        let outcome = within(w.handle.set_alarm(set(KITCHEN, op, once((9, 30)), "Tea")))
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Pending { op_id: op }, "{fault:?}");
        assert_eq!(op_state(&w.db, op).as_deref(), Some("pending"));

        // The controller restarts while HA is still away; HA then comes back.
        w.restart(test_config());
        w.handle.register(endpoint(KITCHEN, "Kitchen")).unwrap();
        w.ha.reconnect();
        eventually("the journaled create applied", || {
            op_state(&w.db, op).as_deref() == Some("applied")
        })
        .await;
        assert_eq!(w.ha.events(KITCHEN_CALENDAR).len(), 1, "{fault:?}");
        assert_eq!(w.ha.state().creates, 1, "{fault:?}");
        // The caller's repeat now reads the outcome under the original id.
        let again = within(w.handle.set_alarm(set(KITCHEN, op, once((9, 30)), "Tea")))
            .await
            .unwrap();
        assert!(matches!(again, Outcome::Applied { op_id, .. } if op_id == op));
    }
}

#[tokio::test]
async fn a_group_cancellation_uses_the_root_id_then_derived_ids() {
    let w = World::new();
    w.kitchen().await;
    for (turn, time) in [("a", (9, 0)), ("b", (10, 0))] {
        let outcome =
            within(
                w.handle
                    .set_alarm(set(KITCHEN, OpId::voice_alarm(turn), once(time), "Pills")),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Applied { .. }));
    }
    let root = OpId::voice_alarm("turn-cancel");
    let request = CancelAlarms {
        op_id: root,
        endpoint: device(KITCHEN),
        selector: CancelSelector::Label("pills".to_owned()),
        source: OpSource::Voice,
    };
    let outcome = within(w.handle.cancel_alarms(request.clone()))
        .await
        .unwrap();
    let CancelOutcome::Journaled { root: got, targets } = &outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(*got, root);
    assert_eq!(targets.len(), 2);
    // The first target (the earliest) takes the root id, the next uuid5(root, schedule).
    let first = ids::new_schedule_id(OpId::voice_alarm("a"));
    let second = ids::new_schedule_id(OpId::voice_alarm("b"));
    assert_eq!(
        targets.iter().map(|t| t.schedule_id).collect::<Vec<_>>(),
        [first, second]
    );
    assert!(
        targets
            .iter()
            .all(|t| matches!(t.outcome, Outcome::Applied { .. }))
    );
    assert_eq!(op_state(&w.db, root).as_deref(), Some("applied"));
    assert_eq!(
        op_state(&w.db, OpId::group_member(root, second)).as_deref(),
        Some("applied")
    );
    assert!(w.ha.events(KITCHEN_CALENDAR).is_empty());
    // A repeat answers from the journal and deletes nothing more.
    let deletes = w.ha.state().deletes.len();
    assert_eq!(
        within(w.handle.cancel_alarms(request)).await.unwrap(),
        outcome
    );
    assert_eq!(w.ha.state().deletes.len(), deletes);
}

// ── Device delivery ──────────────────────────────────────────────────────────

fn snapshot_pages(
    posts: &[(echomuse_alerts::SessionRef, PostBody)],
) -> Vec<echomuse_protocol::alerts::AlertSnapshotPage> {
    posts
        .iter()
        .filter_map(|(_, body)| match body {
            PostBody::Snapshot(page) => Some(page.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn delivery_sends_snapshots_then_deltas_and_resyncs_on_request() {
    let mut w = World::new();
    w.kitchen().await;
    let alarm =
        within(
            w.handle
                .set_alarm(set(KITCHEN, OpId::voice_alarm("t1"), once((9, 0)), "Tea")),
        )
        .await
        .unwrap();
    assert!(matches!(alarm, Outcome::Applied { .. }));

    w.attach(KITCHEN, 1, Some(hello(None, 0)), &[]);
    let (to, body) = w.probe.alert_post().await;
    assert_eq!(to.session, SessionId(1));
    let PostBody::Snapshot(page) = body else {
        panic!("{body:?}");
    };
    let epoch: String =
        w.db.query("SELECT delivery_epoch FROM alert_delivery", |row| {
            row.get(0)
        })
        .pop()
        .unwrap();
    assert_eq!(page.delivery_epoch, epoch);
    assert_eq!(
        (page.high_water_mark, page.page_index, page.page_count),
        (1, 0, 1)
    );
    assert_eq!(page.objects.len(), 1);
    assert_eq!(page.objects[0].label, "Tea");
    assert_eq!(page.objects[0].due_local, "2026-10-28T09:00:00-05:00");
    assert_eq!(page.sha256, snapshot_digest(&page.objects).unwrap());

    w.send(KITCHEN, 1, inbound(MessageType::AlertAck,
        json!({"delivery_epoch": epoch, "applied_through": 1, "durable": true, "need_snapshot": false})));
    eventually("the acknowledged position was recorded", || {
        w.db.query("SELECT acked_sequence FROM alert_delivery", |row| {
            row.get::<_, i64>(0)
        }) == [1]
    })
    .await;
    let listed = w
        .handle
        .list_alarms(device(KITCHEN), 10)
        .await
        .unwrap()
        .unwrap();
    assert!(listed[0].armed_on_endpoint, "the device durably holds it");

    // A new alarm reaches the device as the next delta.
    let op = OpId::voice_alarm("t2");
    let outcome = within(w.handle.set_alarm(set(KITCHEN, op, once((10, 0)), "Walk")))
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Applied { .. }));
    let (_, body) = w.probe.alert_post().await;
    let PostBody::Delta(delta) = body else {
        panic!("{body:?}");
    };
    assert_eq!(
        (delta.delivery_epoch.as_str(), delta.sequence),
        (epoch.as_str(), 2)
    );
    let [CacheObject::Occurrence(object)] = &delta.objects[..] else {
        panic!("{:?}", delta.objects);
    };
    assert_eq!((object.label.as_str(), object.revision), ("Walk", 2));

    // The device asks for a snapshot.
    w.send(KITCHEN, 1, inbound(MessageType::AlertAck,
        json!({"delivery_epoch": epoch, "applied_through": 1, "durable": true, "need_snapshot": true})));
    let (_, body) = w.probe.alert_post().await;
    let PostBody::Snapshot(page) = body else {
        panic!("{body:?}");
    };
    assert_eq!(page.high_water_mark, 3);
    assert_eq!(page.objects.len(), 2);
    w.send(KITCHEN, 1, inbound(MessageType::AlertAck,
        json!({"delivery_epoch": epoch, "applied_through": 3, "durable": true, "need_snapshot": false})));
    eventually("the position was recorded", || {
        w.db.query(
            "SELECT sequence, acked_sequence FROM alert_delivery",
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        ) == [(3, 3)]
    })
    .await;

    // A reconnect in step gets nothing.
    w.handle.detach(device(KITCHEN), SessionId(1)).unwrap();
    w.attach(KITCHEN, 2, Some(hello(Some(&epoch), 3)), &[]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(snapshot_pages(&w.probe.drain()).is_empty());

    // After a controller restart the delivered set is unknown: a snapshot even though
    // the positions match, at the next sequence of the same epoch.
    w.restart(test_config());
    w.handle.register(endpoint(KITCHEN, "Kitchen")).unwrap();
    w.attach(KITCHEN, 3, Some(hello(Some(&epoch), 3)), &[]);
    let (_, body) = w.probe.alert_post().await;
    let PostBody::Snapshot(page) = body else {
        panic!("{body:?}");
    };
    assert_eq!(
        (page.delivery_epoch.as_str(), page.high_water_mark),
        (epoch.as_str(), 4)
    );
}

// ── Rings ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_ring_on_another_speaker_is_checked_again_and_acted_on_through_its_device() {
    let mut w = World::new();
    w.kitchen().await;
    w.handle.register(endpoint(OFFICE, "Office")).unwrap();
    ready(&w.handle, OFFICE).await;
    let created = within(w.handle.set_alarm(set(
        OFFICE,
        OpId::voice_alarm("o1"),
        once((8, 1)),
        "Wake up",
    )))
    .await
    .unwrap();
    let Outcome::Applied { result, .. } = created else {
        panic!("{created:?}");
    };
    let occurrence = result.occurrence_id;
    w.attach(OFFICE, 7, Some(hello(None, 0)), &[]);
    w.ringing(OFFICE, 7, &occurrence.to_string(), "alarm");
    within(async {
        let mut rings = w.handle.rings();
        rings
            .wait_for(|r| r.devices.get(OFFICE).is_some_and(|d| d.active.is_some()))
            .await
            .unwrap();
    })
    .await;
    assert_eq!(
        w.handle
            .rings()
            .borrow()
            .ringing_elsewhere(&device(KITCHEN))
            .len(),
        1
    );

    // "Stop" near the kitchen, but the office now rings something else.
    let stale = Act {
        op_id: OpId::local_action("turn-k", "dismiss"),
        target: RingRef {
            device: device(OFFICE),
            ring_id: "something-else".to_owned(),
        },
        action: ActAction::Dismiss,
        source: ActSource::Voice,
    };
    assert_eq!(
        within(w.handle.act(stale)).await.unwrap(),
        ActOutcome::Refused(Refusal::RingChanged {
            now: occurrence.to_string()
        })
    );

    let op = OpId::local_action("turn-k2", "dismiss");
    let act = Act {
        op_id: op,
        target: RingRef {
            device: device(OFFICE),
            ring_id: occurrence.to_string(),
        },
        action: ActAction::Dismiss,
        source: ActSource::Voice,
    };
    let call = tokio::spawn({
        let handle = w.handle.clone();
        let act = act.clone();
        async move { handle.act(act).await }
    });
    let sent = w.probe.act().await;
    assert_eq!(sent.to.device, device(OFFICE));
    let CommandBody::Act(body) = &sent.body else {
        panic!();
    };
    assert_eq!(
        serde_json::to_value(body).unwrap(),
        json!({"op_id": op.to_string(), "target_id": occurrence.to_string(),
               "action": "dismiss", "source": "voice"})
    );
    sent.ack(AckStatus::Durable, None);
    // The device uploads the operation it journaled under the same id.
    let schedule = ids::new_schedule_id(OpId::voice_alarm("o1"));
    w.send(
        OFFICE,
        7,
        inbound(
            MessageType::AlertLocalOperation,
            json!({
        "op_id": op.to_string(), "action": "dismiss", "occurrence_id": occurrence.to_string(),
        "schedule_id": schedule.to_string(), "revision": 1, "reason": null, "source": "voice"}),
        ),
    );
    let outcome = within(call).await.unwrap().unwrap();
    assert_eq!(
        outcome,
        ActOutcome::Applied {
            op_id: op,
            kind: echomuse_alerts::wire::RingKind::Alarm
        }
    );
    assert!(w.ha.events(OFFICE_CALENDAR).is_empty());
    let posts = w.probe.drain();
    assert!(posts.iter().any(|(_, body)| matches!(body,
        PostBody::OpResult(r) if r.op_id == op.uuid() && r.state == echomuse_alerts::wire::OpResultState::Applied)));
    // A repeat of the act is answered without sending anything.
    assert_eq!(within(w.handle.act(act)).await.unwrap(), outcome);

    // Timers cannot be snoozed.
    w.ringing(OFFICE, 7, "timer-1", "timer");
    eventually("the timer rings", || {
        w.handle
            .rings()
            .borrow()
            .devices
            .get(OFFICE)
            .and_then(|d| d.active.as_ref())
            .is_some_and(|a| a.id == "timer-1")
    })
    .await;
    let snooze = Act {
        op_id: OpId::local_action("turn-k3", "snooze"),
        target: RingRef {
            device: device(OFFICE),
            ring_id: "timer-1".to_owned(),
        },
        action: ActAction::Snooze,
        source: ActSource::Voice,
    };
    assert_eq!(
        within(w.handle.act(snooze)).await.unwrap(),
        ActOutcome::Refused(Refusal::NotSnoozable)
    );
}

#[tokio::test]
async fn a_repeated_act_the_device_has_not_confirmed_is_never_sent_again() {
    // Remember a single outcome, so later acts push earlier ones out of that memory.
    let short = echomuse_alerts::AlertConfig {
        settle_wait: Duration::from_millis(300),
        remembered: 1,
        ..test_config()
    };
    let mut w = World::with(short);
    w.kitchen().await;
    let created = within(w.handle.set_alarm(set(
        KITCHEN,
        OpId::voice_alarm("k1"),
        once((8, 1)),
        "Wake up",
    )))
    .await
    .unwrap();
    let Outcome::Applied { result, .. } = created else {
        panic!("{created:?}");
    };
    let occurrence = result.occurrence_id;
    w.attach(KITCHEN, 3, Some(hello(None, 0)), &[]);
    w.ringing(KITCHEN, 3, &occurrence.to_string(), "alarm");
    eventually("the alarm rings", || {
        w.handle
            .rings()
            .borrow()
            .devices
            .get(KITCHEN)
            .is_some_and(|d| d.active.is_some())
    })
    .await;

    let op = OpId::local_action("turn-r", "dismiss");
    let act = Act {
        op_id: op,
        target: RingRef {
            device: device(KITCHEN),
            ring_id: occurrence.to_string(),
        },
        action: ActAction::Dismiss,
        source: ActSource::Voice,
    };
    let call = tokio::spawn({
        let handle = w.handle.clone();
        let act = act.clone();
        async move { handle.act(act).await }
    });
    // The device journals the act but has not uploaded its operation by the settle wait.
    w.probe.act().await.ack(AckStatus::Durable, None);
    assert_eq!(
        within(call).await.unwrap().unwrap(),
        ActOutcome::Pending { op_id: op }
    );

    // The alarm still shows ringing; the repeat waits again and sends nothing.
    assert_eq!(
        within(w.handle.act(act.clone())).await.unwrap(),
        ActOutcome::Pending { op_id: op }
    );
    assert!(w.probe.acts.try_recv().is_err(), "the act was sent again");

    // Acts the device never answered are remembered as unknown; once later acts push them
    // out of that bounded memory, a repeat still waits instead of sending them again.
    let unanswered = |turn: &str| Act {
        op_id: OpId::local_action(turn, "dismiss"),
        ..act.clone()
    };
    for turn in ["turn-u1", "turn-u2"] {
        let call = tokio::spawn({
            let handle = w.handle.clone();
            let act = unanswered(turn);
            async move { handle.act(act).await }
        });
        w.probe.act().await.lose();
        let outcome = within(call).await.unwrap().unwrap();
        assert!(matches!(outcome, ActOutcome::Unknown { .. }), "{outcome:?}");
    }
    let forgotten = unanswered("turn-u1");
    assert_eq!(
        within(w.handle.act(forgotten.clone())).await.unwrap(),
        ActOutcome::Pending {
            op_id: forgotten.op_id
        }
    );
    assert!(
        w.probe.acts.try_recv().is_err(),
        "an unanswered act was sent again"
    );

    // The operation arrives under the same id and answers the next repeat.
    let schedule = ids::new_schedule_id(OpId::voice_alarm("k1"));
    w.send(
        KITCHEN,
        3,
        inbound(
            MessageType::AlertLocalOperation,
            json!({
        "op_id": op.to_string(), "action": "dismiss", "occurrence_id": occurrence.to_string(),
        "schedule_id": schedule.to_string(), "revision": 1, "reason": null, "source": "voice"}),
        ),
    );
    eventually("the device's operation applied", || {
        op_state(&w.db, op).as_deref() == Some("applied")
    })
    .await;
    assert_eq!(
        within(w.handle.act(act)).await.unwrap(),
        ActOutcome::Applied {
            op_id: op,
            kind: echomuse_alerts::wire::RingKind::Alarm
        }
    );
    assert!(w.probe.acts.try_recv().is_err());
}

#[tokio::test]
async fn a_device_snooze_creates_its_child_then_removes_the_parent() {
    let mut w = World::new();
    w.kitchen().await;
    let parent_op = OpId::voice_alarm("p1");
    let created = within(w.handle.set_alarm(set(
        KITCHEN,
        parent_op,
        weekly((8, 5), &["wed", "thu"]),
        "Wake up",
    )))
    .await
    .unwrap();
    let Outcome::Applied { result, .. } = created else {
        panic!("{created:?}");
    };
    w.attach(KITCHEN, 1, Some(hello(None, 0)), &[]);
    let _ = w.probe.alert_post().await;

    // The button snoozed it at 08:05:30.4; the device derived the child (§16.3).
    let child_schedule = ids::snooze_child_schedule_id(result.occurrence_id);
    let child_due_local = "2026-10-28T08:14:31-05:00";
    let child_occurrence = ids::occurrence_id(child_schedule, child_due_local);
    let op = Uuid::new_v4();
    let body = json!({"op_id": op.to_string(), "action": "snooze",
        "occurrence_id": result.occurrence_id.to_string(), "schedule_id": result.schedule_id.to_string(),
        "revision": 1, "reason": null, "source": "button",
        "child": {"schedule_id": child_schedule.to_string(), "occurrence_id": child_occurrence.to_string(),
                  "due_utc_ms": parse_instant(child_due_local).unwrap().as_millisecond().to_string(),
                  "due_local": child_due_local}});
    w.send(
        KITCHEN,
        1,
        inbound(MessageType::AlertLocalOperation, body.clone()),
    );
    let op_id = OpId::from_device(op);
    eventually("the snooze applied", || {
        op_state(&w.db, op_id).as_deref() == Some("applied")
    })
    .await;

    let events = w.ha.events(KITCHEN_CALENDAR);
    let child = events
        .iter()
        .find(|e| e.summary == "Snoozed: Wake up")
        .expect("child event");
    assert_eq!(child.start, parse_instant(child_due_local).unwrap());
    let line = line_of(child);
    assert_eq!((line.id, line.op), (child_schedule, op));
    assert_eq!(line.parent.unwrap().occ, "20261028T080500");
    let parent = events.iter().find(|e| e.summary == "Wake up").unwrap();
    assert!(
        parent.excluded.contains("20261028T080500"),
        "only today's instance is gone"
    );

    let source: String =
        w.db.query(
            &format!("SELECT source FROM alert_ops WHERE op_id = '{op}'"),
            |row| row.get(0),
        )
        .pop()
        .unwrap();
    assert_eq!(source, "device");
    // The device re-sends it at its next session; it is answered and nothing is written.
    let creates = w.ha.state().creates;
    w.send(KITCHEN, 1, inbound(MessageType::AlertLocalOperation, body));
    eventually("answered again", || {
        w.probe
            .drain()
            .iter()
            .any(|(_, b)| matches!(b, PostBody::OpResult(r) if r.op_id == op))
    })
    .await;
    assert_eq!(w.ha.state().creates, creates);
}

#[tokio::test]
async fn a_snooze_of_an_alarm_deleted_meanwhile_is_rejected_and_its_child_retracted() {
    let mut w = World::new();
    w.kitchen().await;
    let created = within(w.handle.set_alarm(set(
        KITCHEN,
        OpId::voice_alarm("p2"),
        once((8, 5)),
        "Wake up",
    )))
    .await
    .unwrap();
    let Outcome::Applied { result, .. } = created else {
        panic!();
    };
    w.attach(KITCHEN, 1, Some(hello(None, 0)), &[]);
    let (_, PostBody::Snapshot(page)) = w.probe.alert_post().await else {
        panic!();
    };
    w.send(
        KITCHEN,
        1,
        inbound(
            MessageType::AlertAck,
            json!({"delivery_epoch": page.delivery_epoch,
        "applied_through": page.high_water_mark, "durable": true, "need_snapshot": false}),
        ),
    );
    // Someone deletes it in HA's calendar UI while it rings.
    w.ha.replace_events(KITCHEN_CALENDAR, Vec::new());
    // The device hears of the deletion first, so the service has read it.
    within(async {
        loop {
            if let (_, PostBody::Delta(delta)) = w.probe.alert_post().await
                && delta.objects.iter().any(|o| {
                    matches!(o,
                    CacheObject::Tombstone(t) if t.occurrence_id == result.occurrence_id
                        && t.tombstone == TombstoneReason::Deleted)
                })
            {
                return;
            }
        }
    })
    .await;
    let child_schedule = ids::snooze_child_schedule_id(result.occurrence_id);
    let child_due = "2026-10-28T08:14:31-05:00";
    let child_occurrence = ids::occurrence_id(child_schedule, child_due);
    let op = Uuid::new_v4();
    w.send(KITCHEN, 1, inbound(MessageType::AlertLocalOperation, json!({"op_id": op.to_string(),
        "action": "snooze", "occurrence_id": result.occurrence_id.to_string(),
        "schedule_id": result.schedule_id.to_string(), "revision": 1, "reason": null, "source": "button",
        "child": {"schedule_id": child_schedule.to_string(), "occurrence_id": child_occurrence.to_string(),
                  "due_utc_ms": parse_instant(child_due).unwrap().as_millisecond().to_string(),
                  "due_local": child_due}})));
    let mut rejected = false;
    let mut retracted = false;
    within(async {
        while !(rejected && retracted) {
            let (_, body) = w.probe.alert_post().await;
            match body {
                PostBody::OpResult(r) if r.op_id == op => {
                    assert_eq!(r.state, echomuse_alerts::wire::OpResultState::Rejected);
                    assert_eq!(r.error.as_deref(), Some("parent_gone"));
                    rejected = true;
                }
                PostBody::Delta(delta) => {
                    retracted |= delta.objects.iter().any(|o| {
                        matches!(o,
                        CacheObject::Tombstone(t) if t.occurrence_id == child_occurrence)
                    });
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(
        w.ha.events(KITCHEN_CALENDAR).is_empty(),
        "no child was created"
    );
}

#[tokio::test]
async fn an_occurrence_restored_from_a_backup_is_deleted_again_before_it_rings() {
    let mut w = World::new();
    w.kitchen().await;
    let created = within(w.handle.set_alarm(set(
        KITCHEN,
        OpId::voice_alarm("r1"),
        weekly((9, 0), &["wed", "thu"]),
        "Gym",
    )))
    .await
    .unwrap();
    let Outcome::Applied { result, .. } = created else {
        panic!();
    };
    w.attach(KITCHEN, 1, Some(hello(None, 0)), &[]);
    let _ = w.probe.alert_post().await;
    let op = Uuid::new_v4();
    w.send(KITCHEN, 1, inbound(MessageType::AlertLocalOperation, json!({"op_id": op.to_string(),
        "action": "dismiss", "occurrence_id": result.occurrence_id.to_string(),
        "schedule_id": result.schedule_id.to_string(), "revision": 1, "reason": null, "source": "button"})));
    eventually("dismissed", || {
        op_state(&w.db, OpId::from_device(op)).as_deref() == Some("applied")
    })
    .await;

    // HA is restored from a backup taken before the dismissal.
    let mut event = w.ha.events(KITCHEN_CALENDAR).remove(0);
    event.excluded.clear();
    w.ha.replace_events(KITCHEN_CALENDAR, vec![event]);
    eventually("deleted again", || {
        w.ha.events(KITCHEN_CALENDAR)[0]
            .excluded
            .contains("20261028T090000")
    })
    .await;
    assert_eq!(
        op_state(&w.db, OpId::from_device(op)).as_deref(),
        Some("applied")
    );
    assert_eq!(
        w.db.query("SELECT count(*) FROM alert_ops", |row| row.get::<_, i64>(0)),
        [2],
        "the guard repeats an applied operation without journaling another"
    );
    // The device never got the restored occurrence back.
    let upserts = w.probe.drain().into_iter().any(|(_, body)| match body {
        PostBody::Delta(d) => d.objects.iter().any(|o| {
            matches!(o,
            CacheObject::Occurrence(o) if o.occurrence_id == result.occurrence_id)
        }),
        _ => false,
    });
    assert!(!upserts);
}

#[tokio::test]
async fn ui_events_ring_with_the_defaults_and_missed_occurrences_expire() {
    let w = World::new();
    w.ha.add_calendar("EchoMuse Kitchen", KITCHEN_CALENDAR);
    let now = parse_instant(NOW).unwrap();
    w.ha.put_event(
        KITCHEN_CALENDAR,
        SimEvent {
            uid: "ui-1".to_owned(),
            summary: "Dentist".to_owned(),
            start: now + jiff::SignedDuration::from_hours(1),
            description: Some("Bring the forms".to_owned()),
            rrule: None,
            excluded: Default::default(),
        },
    );
    // Due two hours ago: past the 30-minute catch-up, found by the backlog read.
    w.ha.put_event(
        KITCHEN_CALENDAR,
        SimEvent {
            uid: "ui-2".to_owned(),
            summary: "Bins".to_owned(),
            start: now - jiff::SignedDuration::from_hours(2),
            description: None,
            rrule: None,
            excluded: Default::default(),
        },
    );
    w.kitchen().await;
    assert_eq!(
        w.ha.state().calendars_created,
        0,
        "the existing calendar was found by title"
    );
    let alarms = w
        .handle
        .list_alarms(device(KITCHEN), 10)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(alarms.len(), 1);
    assert_eq!(alarms[0].label, "Dentist");
    assert_eq!(
        alarms[0].schedule_id,
        ids::ui_schedule_id(KITCHEN_CALENDAR, "ui-1")
    );

    let missed_schedule = ids::ui_schedule_id(KITCHEN_CALENDAR, "ui-2");
    let key = "2026-10-28T06:00:00-05:00";
    let missed = OpId::missed(ids::occurrence_id(missed_schedule, key));
    eventually("the missed occurrence expired", || {
        op_state(&w.db, missed).as_deref() == Some("applied")
    })
    .await;
    let row: (String, String, String) =
        w.db.query(
            &format!(
                "SELECT action, source, occurrence_key FROM alert_ops WHERE op_id = '{missed}'"
            ),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .pop()
        .unwrap();
    assert_eq!(
        row,
        ("expire".to_owned(), "engine".to_owned(), key.to_owned())
    );
    assert!(
        w.ha.events(KITCHEN_CALENDAR)
            .iter()
            .all(|e| e.uid != "ui-2")
    );
}

// ── LLM relay ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_relay_request_is_answered_and_a_repeat_returns_the_same_result() {
    let mut w = World::new();
    w.kitchen().await;
    eventually("the relay is subscribed", || w.ha.relay_subscribers() > 0).await;
    let request_id = "01K8CTX|set_alarm|{\"time\":\"06:45:00\",\"days\":[\"fri\"],\"name\":\"\",\"speaker\":\"kitchen\"}";
    let event = json!({"request_id": request_id, "action": "set_alarm",
        "args": {"time": "06:45:00", "days": ["fri"], "name": "", "speaker": "kitchen"}});
    w.ha.relay_request(event.clone());
    let first = w.result().await;
    assert_eq!(first["request_id"], request_id);
    assert_eq!(first["result"]["ok"], true);
    assert_eq!(first["result"]["stored_in_ha"], true);
    assert_eq!(first["result"]["first_due"], "2026-10-30T06:45:00-05:00");
    assert_eq!(
        first["result"]["op_id"],
        OpId::relay(request_id).to_string()
    );

    w.ha.relay_request(event);
    assert_eq!(w.result().await, first);
    assert_eq!(w.ha.state().creates, 1);
    let source: String =
        w.db.query("SELECT source FROM alert_ops", |row| row.get(0))
            .pop()
            .unwrap();
    assert_eq!(source, "llm");

    // Invalid requests are answered without a write.
    w.ha.relay_request(json!({"request_id": "r2", "action": "cancel_alarm",
        "args": {"name": "", "time": "", "all": false, "speaker": "kitchen"}}));
    assert_eq!(
        w.result().await["result"],
        json!({"ok": false, "error": "cancel_alarm needs name, time or all"})
    );
    // No speaker named and no turn waiting: the model asks.
    w.ha.relay_request(
        json!({"request_id": "r3", "action": "list_alarms", "args": {"speaker": ""}}),
    );
    assert_eq!(w.result().await["result"]["error"], "which speaker?");
    // One turn waiting on an intent run picks the speaker.
    let _waiting = w.handle.intent_wait(device(KITCHEN)).unwrap();
    w.ha.relay_request(
        json!({"request_id": "r4", "action": "list_alarms", "args": {"speaker": ""}}),
    );
    let listed = w.result().await;
    assert_eq!(listed["result"]["speaker"], "Kitchen");
    assert_eq!(
        listed["result"]["alarms"][0]["due"],
        "2026-10-30T06:45:00-05:00"
    );
    // The area name works too.
    w.ha.relay_request(
        json!({"request_id": "r5", "action": "dismiss_alert", "args": {"speaker": "Kitchen area"}}),
    );
    assert_eq!(w.result().await["result"]["error"], "nothing is ringing");
}

// ── Scripts ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scripts_are_installed_and_a_user_edited_one_is_left_alone() {
    let db = TempDb::new();
    let (ha, _results) = SimHa::new("America/Chicago");
    let edited = json!({"alias": "My own list", "sequence": []});
    ha.state()
        .scripts
        .insert("echomuse_list_alarms".to_owned(), edited.clone());
    let (link, _probe) = SimLink::new();
    let clock = ManualClock::at(NOW);
    let (handle, _task) = start(test_config(), &db, &ha, &link, &clock);
    let mut scripts = handle.scripts();
    within(scripts.wait_for(|s| s.len() == 5)).await.unwrap();
    let states = scripts.borrow().clone();
    assert_eq!(
        states
            .iter()
            .find(|(id, _)| id == "echomuse_list_alarms")
            .unwrap()
            .1,
        ScriptState::LeftEdited
    );
    assert_eq!(
        states
            .iter()
            .filter(|(_, s)| *s == ScriptState::Installed)
            .count(),
        4
    );
    assert_eq!(ha.state().scripts["echomuse_list_alarms"], edited);
    assert!(
        !ha.state()
            .script_writes
            .contains(&"echomuse_list_alarms".to_owned())
    );
    assert_eq!(ha.state().reloads, 1);
    assert_eq!(ha.state().exposed.len(), 4);
    let recorded: Vec<(String, String, i64)> = db.query(
        "SELECT object_id, sha256, revision FROM alert_scripts ORDER BY object_id",
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );
    assert_eq!(recorded.len(), 4);
    for (id, sha, revision) in &recorded {
        let written = &ha.state().scripts[id];
        assert_eq!(
            Some(sha.clone()),
            echomuse_alerts::scripts::config_digest(written)
        );
        assert_eq!(*revision, echomuse_alerts::scripts::SCRIPT_REVISION);
    }

    // On the next connection, the user edits another; it is left alone too.
    ha.state().scripts.get_mut("echomuse_set_alarm").unwrap()["description"] = json!("Mine now");
    let writes = ha.state().script_writes.len();
    ha.disconnect();
    ha.reconnect();
    within(scripts.wait_for(|s| {
        s.iter()
            .any(|(id, state)| id == "echomuse_set_alarm" && *state == ScriptState::LeftEdited)
    }))
    .await
    .unwrap();
    assert_eq!(ha.state().script_writes.len(), writes);
}

// ── Timers ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn finished_timers_ring_running_ones_are_shown_and_a_missing_finish_is_reported() {
    let mut w = World::new();
    w.kitchen().await;
    let mut notices = w.handle.notices();
    w.attach(
        KITCHEN,
        1,
        Some(hello(None, 0)),
        &["timer_view_v1", "alert_prefetch"],
    );
    // The hello's timer view is always sent, empty included.
    let first_view = loop {
        if let (_, PostBody::TimerView(view)) = w.probe.post().await {
            break view;
        }
    };
    assert!(first_view.timers.is_empty());

    let started = |id: &str, left: u32| TimerEvent {
        kind: TimerEventKind::Started,
        timer_id: id.to_owned(),
        name: Some("pasta".to_owned()),
        total_seconds: 300,
        seconds_left: left,
        is_active: true,
    };
    w.handle
        .timer_event(device(KITCHEN), started("t1", 300))
        .unwrap();
    let view = loop {
        if let (_, PostBody::TimerView(view)) = w.probe.post().await
            && !view.timers.is_empty()
        {
            break view;
        }
    };
    assert_eq!(view.timers[0].id, "t1");
    assert_eq!(view.timers[0].total_ms, 300_000);

    w.handle
        .timer_event(
            device(KITCHEN),
            TimerEvent {
                kind: TimerEventKind::Finished,
                ..started("t1", 0)
            },
        )
        .unwrap();
    eventually("the ring was sent", || {
        w.link.others.lock().unwrap().iter().any(|(_, body)| {
            matches!(body,
            CommandBody::Ring(r) if r.ring_id == "t1" && r.name == "pasta"
                && r.max_ring_ms.get() == 900_000 && r.loop_gap_ms == 2_000)
        })
    })
    .await;
    assert!(
        w.link
            .others
            .lock()
            .unwrap()
            .iter()
            .any(|(_, body)| matches!(body, CommandBody::Prefetch(_)))
    );

    // A timer whose finish never comes is dropped and reported.
    w.handle
        .timer_event(device(KITCHEN), started("t2", 1))
        .unwrap();
    let missed = within(async {
        loop {
            if let Ok(AlertNotice::TimerFinishMissed { timer_id, .. }) = notices.recv().await {
                return timer_id;
            }
        }
    })
    .await;
    assert_eq!(missed, "t2");

    // A finish while the speaker is away cannot ring; it is reported.
    w.handle.detach(device(KITCHEN), SessionId(1)).unwrap();
    w.link.disconnect(KITCHEN);
    w.handle
        .timer_event(device(KITCHEN), started("t3", 60))
        .unwrap();
    w.handle
        .timer_event(
            device(KITCHEN),
            TimerEvent {
                kind: TimerEventKind::Finished,
                ..started("t3", 0)
            },
        )
        .unwrap();
    let undeliverable = within(async {
        loop {
            if let Ok(AlertNotice::TimerRingUndeliverable { timer_id, .. }) = notices.recv().await {
                return timer_id;
            }
        }
    })
    .await;
    assert_eq!(undeliverable, "t3");
}

#[tokio::test]
async fn a_newly_chosen_timer_sound_is_prefetched_on_a_connected_speaker() {
    let w = World::new();
    w.kitchen().await;
    w.attach(KITCHEN, 1, Some(hello(None, 0)), &["alert_prefetch"]);
    let bell = echomuse_protocol::Sha256::of(b"bell");
    let prefetched =
        |w: &World, sha: &echomuse_protocol::Sha256| {
            w.link.others.lock().unwrap().iter().any(
                |(_, body)| matches!(body, CommandBody::Prefetch(p) if p.sounds == [sha.clone()]),
            )
        };
    // The hello's timer sound first.
    let first = echomuse_protocol::Sha256::of(b"timer");
    eventually("the attach's prefetch", || prefetched(&w, &first)).await;
    assert!(!prefetched(&w, &bell));
    w.handle
        .update(EndpointConfig {
            timer_sound: AlertSound::Asset(bell.clone()),
            ..endpoint(KITCHEN, "Kitchen")
        })
        .unwrap();
    eventually("the new sound's prefetch", || prefetched(&w, &bell)).await;
}

#[test]
fn tombstone_reasons_have_the_wire_spellings() {
    assert_eq!(TombstoneReason::Dismissed.as_str(), "dismissed");
}
