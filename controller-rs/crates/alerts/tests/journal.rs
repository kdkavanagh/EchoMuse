//! The alert tables' commands against a temporary database from the contract schema.

mod common;

use common::TempDb;
use echomuse_alerts::{
    OpId,
    journal::{
        Action, AdvanceStep, BindCalendar, JournalOps, Journaled, LoadDelivery, LoadScripts, NewOp,
        OpsById, Payload, PendingOps, RecordScript, RestoreGuard, ScriptRecord,
        SetDeliveryPosition, Settle, Settlement, Source, State, request_digest,
    },
};
use echomuse_protocol::{Sha256, canonical::canonical_json};
use serde_json::json;
use uuid::Uuid;

fn op(id: OpId, action: Action, key: Option<&str>, request: serde_json::Value) -> NewOp {
    NewOp {
        op_id: id,
        endpoint_id: "G0SYNTHETIC00001".to_owned(),
        calendar_entity: "calendar.echomuse_kitchen".to_owned(),
        action,
        schedule_id: Uuid::from_u128(7),
        occurrence_key: key.map(str::to_owned),
        source: Source::Voice,
        request,
        plan: json!({"event": {"summary": "Tea", "volume": 0.5}}),
        created_ms: 1_790_000_000_000,
    }
}

#[tokio::test]
async fn an_operation_is_journaled_once_and_a_different_request_conflicts() {
    let db = TempDb::new();
    let first = OpId::voice_alarm("turn-1");
    let request = json!({"type": "set_alarm", "time": "07:00:00", "label": "Tea"});
    let outcomes = db
        .handle
        .write(JournalOps(vec![op(
            first,
            Action::Create,
            None,
            request.clone(),
        )]))
        .await
        .unwrap();
    let [Journaled::Inserted(row)] = &outcomes[..] else {
        panic!("{outcomes:?}");
    };
    assert_eq!(row.state, Some(State::Pending));
    assert_eq!(row.payload_sha256, request_digest(&request).unwrap());
    assert_eq!(
        row.payload_sha256,
        Sha256::of(canonical_json(&request).unwrap().as_bytes()).as_str()
    );
    assert_eq!(
        row.payload,
        Payload::Ours {
            request: request.clone(),
            plan: json!({"event": {"summary": "Tea", "volume": 0.5}})
        }
    );
    // The stored text is canonical JSON of the plan and request.
    let stored: Vec<String> = db.query("SELECT payload_json FROM alert_ops", |row| row.get(0));
    assert_eq!(
        stored,
        [
            r#"{"plan":{"event":{"summary":"Tea","volume":0.5}},"request":{"label":"Tea","time":"07:00:00","type":"set_alarm"}}"#
        ]
    );

    let again = db
        .handle
        .write(JournalOps(vec![op(
            first,
            Action::Create,
            None,
            request.clone(),
        )]))
        .await
        .unwrap();
    assert!(matches!(&again[..], [Journaled::Existing(r)] if r.seq == row.seq));
    let other = json!({"type": "set_alarm", "time": "08:00:00", "label": "Tea"});
    let conflict = db
        .handle
        .write(JournalOps(vec![op(first, Action::Create, None, other)]))
        .await
        .unwrap();
    assert!(matches!(&conflict[..], [Journaled::Conflict(_)]));

    db.handle
        .write(AdvanceStep {
            op_id: first,
            step: 1,
        })
        .await
        .unwrap();
    let settled = db
        .handle
        .write(Settle {
            op_id: first,
            settlement: Settlement::Applied {
                at: 1_790_000_001_000,
                result: json!({"ok": true}),
            },
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (settled.state, settled.step, settled.applied_ms),
        (Some(State::Applied), 1, Some(1_790_000_001_000))
    );
    // A settled row stays as it is.
    let unchanged = db
        .handle
        .write(Settle {
            op_id: first,
            settlement: Settlement::Rejected {
                result: json!({"ok": false}),
            },
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.state, Some(State::Applied));
    assert_eq!(unchanged.result, Some(json!({"ok": true})));
}

#[tokio::test]
async fn pending_rows_come_back_in_order_and_applied_terminal_ones_guard() {
    let db = TempDb::new();
    let ids: Vec<OpId> = (0..3).map(|i| OpId::relay(&format!("r{i}"))).collect();
    db.handle
        .write(JournalOps(vec![
            op(
                ids[0],
                Action::Dismiss,
                Some("20261028T090000"),
                json!({"n": 0}),
            ),
            op(ids[1], Action::Cancel, None, json!({"n": 1})),
            op(ids[2], Action::Create, None, json!({"n": 2})),
        ]))
        .await
        .unwrap();
    for id in &ids[..2] {
        db.handle
            .write(Settle {
                op_id: *id,
                settlement: Settlement::Applied {
                    at: 1_790_000_000_500,
                    result: json!({"ok": true}),
                },
            })
            .await
            .unwrap();
    }
    let pending = db.handle.read(PendingOps).await.unwrap();
    assert_eq!(
        pending.iter().map(|r| r.op_id).collect::<Vec<_>>(),
        [ids[2]]
    );
    let guard = db
        .handle
        .read(RestoreGuard {
            endpoint_id: "G0SYNTHETIC00001".to_owned(),
            since_ms: 0,
        })
        .await
        .unwrap();
    assert_eq!(guard.len(), 2);
    assert_eq!(guard[0].occurrence_key.as_deref(), Some("20261028T090000"));
    assert_eq!(
        (guard[1].action, guard[1].occurrence_key.clone()),
        (Action::Cancel, None)
    );
    let found = db.handle.read(OpsById(ids.clone())).await.unwrap();
    assert_eq!(found.len(), 3);
}

#[tokio::test]
async fn the_delivery_position_only_rises_and_scripts_are_recorded() {
    let db = TempDb::new();
    let row = db
        .handle
        .write(BindCalendar {
            endpoint_id: "G0SYNTHETIC00001".to_owned(),
            calendar_entry_id: "01K1".to_owned(),
            calendar_entity: "calendar.echomuse_kitchen".to_owned(),
            new_epoch: "epoch-1".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(
        (
            row.delivery_epoch.as_str(),
            row.sequence,
            row.acked_sequence
        ),
        ("epoch-1", 0, 0)
    );
    let set = |sequence, acked| SetDeliveryPosition {
        endpoint_id: "G0SYNTHETIC00001".to_owned(),
        sequence,
        acked_sequence: acked,
    };
    db.handle.write(set(5, 3)).await.unwrap();
    db.handle.write(set(4, 2)).await.unwrap();
    db.handle.write(set(5, 9)).await.unwrap();
    let row = db
        .handle
        .read(LoadDelivery("G0SYNTHETIC00001".to_owned()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.sequence, row.acked_sequence), (5, 5));
    // Binding again keeps the epoch and position.
    let rebound = db
        .handle
        .write(BindCalendar {
            endpoint_id: "G0SYNTHETIC00001".to_owned(),
            calendar_entry_id: "01K1".to_owned(),
            calendar_entity: "calendar.echomuse_kitchen".to_owned(),
            new_epoch: "epoch-2".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(
        (rebound.delivery_epoch.as_str(), rebound.sequence),
        ("epoch-1", 5)
    );

    let record = |sha: &str, revision| {
        RecordScript(ScriptRecord {
            object_id: "echomuse_set_alarm".to_owned(),
            sha256: sha.to_owned(),
            revision,
        })
    };
    db.handle.write(record("aa", 1)).await.unwrap();
    db.handle.write(record("bb", 2)).await.unwrap();
    assert_eq!(
        db.handle.read(LoadScripts).await.unwrap(),
        [ScriptRecord {
            object_id: "echomuse_set_alarm".to_owned(),
            sha256: "bb".to_owned(),
            revision: 2
        }]
    );
}
