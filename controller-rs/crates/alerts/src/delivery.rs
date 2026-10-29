//! Delivery of a speaker's alert cache (§3.1.8, architecture §16.4): one delivery epoch and
//! sequence per speaker, snapshots of the live occurrences, and deltas after them.
//!
//! This module decides what to send; the service persists the position it reports
//! before sending anything, so a sequence number is never reused with other content.
//!
//! - **Resync.** At hello a snapshot is sent unless the device's epoch and acknowledged
//!   sequence match this speaker's. A match is trusted only for what this process sent:
//!   after a controller restart the delivered set is unknown, and the first session gets
//!   a snapshot. Deltas the device had not acknowledged when its last session ended are
//!   sent again, in order, under their original sequences.
//! - **Deltas** carry upserted occurrences and tombstones; each is the previous sequence
//!   plus one. The device applies one only at its acknowledged sequence plus one and
//!   otherwise asks for a snapshot (`need_snapshot`), as it does when its store is damaged.
//! - **Revisions.** An object's `revision` is the sequence at which it last changed.

use std::collections::{BTreeMap, HashMap, VecDeque};

use echomuse_core::types::SessionId;
use echomuse_protocol::{
    alerts::{
        AlertDelta, CacheObject, OccurrenceObject, SNAPSHOT_PAGE_OBJECTS, SnapshotError,
        TombstoneObject, TombstoneReason, snapshot_pages,
    },
    session::HelloAlerts,
};
use uuid::Uuid;

use crate::wire::{AlertAck, PostBody};

/// The live set a speaker should hold: occurrence objects (their revisions are assigned
/// here) and, for occurrences that left it, why.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Desired {
    /// Live occurrences.
    pub live: Vec<OccurrenceObject>,
    /// Why an occurrence is no longer live; an occurrence missing here is `deleted`.
    pub reasons: HashMap<Uuid, TombstoneReason>,
}

/// What a step of delivery produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outbox {
    /// Bodies to send, in order, after persisting the position.
    pub bodies: Vec<PostBody>,
    /// The position to persist first, when it moved: `(sequence, acked_sequence)`.
    pub position: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq)]
struct Attached {
    session: SessionId,
    snapshot_owed: bool,
}

/// One speaker's delivery state.
#[derive(Clone, Debug, PartialEq)]
pub struct Delivery {
    epoch: String,
    sequence: u64,
    acked: u64,
    /// The objects the device holds once it has applied everything sent, with their
    /// revisions; `None` when this process has not sent this epoch's state.
    sent: Option<BTreeMap<Uuid, OccurrenceObject>>,
    /// Deltas sent after the acknowledged sequence.
    unacked: VecDeque<AlertDelta>,
    attached: Option<Attached>,
}

/// Two objects are the same occurrence content whatever their revisions.
fn same_content(a: &OccurrenceObject, b: &OccurrenceObject) -> bool {
    OccurrenceObject {
        revision: 0,
        ..a.clone()
    } == OccurrenceObject {
        revision: 0,
        ..b.clone()
    }
}

impl Delivery {
    /// The state of a speaker whose row holds `epoch`, `sequence` and `acked`.
    pub fn new(epoch: String, sequence: u64, acked: u64) -> Self {
        Self {
            epoch,
            sequence,
            acked: acked.min(sequence),
            sent: None,
            unacked: VecDeque::new(),
            attached: None,
        }
    }

    /// The delivery epoch.
    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// `(sequence, acked_sequence)`.
    pub fn position(&self) -> (u64, u64) {
        (self.sequence, self.acked)
    }

    /// The session deliveries go to.
    pub fn session(&self) -> Option<SessionId> {
        self.attached.as_ref().map(|a| a.session)
    }

    /// Whether the device has durably applied the object for `occurrence_id` as this
    /// process last sent it.
    pub fn armed(&self, occurrence_id: Uuid) -> bool {
        self.sent
            .as_ref()
            .and_then(|sent| sent.get(&occurrence_id))
            .is_some_and(|object| object.revision <= self.acked)
    }

    /// A session started with `hello`. Unacknowledged deltas the device still lacks are
    /// returned for resending; otherwise a snapshot is owed when the positions differ.
    pub fn attach(&mut self, session: SessionId, hello: Option<&HelloAlerts>) -> Outbox {
        let mut outbox = Outbox::default();
        let device = hello.and_then(|h| Some((h.delivery_epoch.as_deref()?, h.acked_sequence?)));
        let in_step = match device {
            Some((epoch, acked)) if epoch == self.epoch && self.sent.is_some() => {
                acked <= self.sequence
                    && (acked == self.sequence
                        || self
                            .unacked
                            .front()
                            .is_some_and(|first| first.sequence <= acked + 1))
            }
            _ => false,
        };
        if in_step {
            let (_, device_acked) = device.expect("matched above");
            if device_acked > self.acked {
                self.acked = device_acked;
                outbox.position = Some((self.sequence, self.acked));
            }
            self.unacked.retain(|delta| delta.sequence > device_acked);
            outbox
                .bodies
                .extend(self.unacked.iter().cloned().map(PostBody::Delta));
        }
        self.attached = Some(Attached {
            session,
            snapshot_owed: !in_step,
        });
        outbox
    }

    /// The session ended; nothing more is sent until the next one.
    pub fn detach(&mut self, session: SessionId) {
        if self.session() == Some(session) {
            self.attached = None;
        }
    }

    /// An `alert.ack` from the current session. Returns the position to persist; a
    /// request for a snapshot is remembered for the next [`Delivery::refresh`].
    pub fn ack(&mut self, ack: &AlertAck) -> Outbox {
        let mut outbox = Outbox::default();
        if ack.need_snapshot
            && let Some(attached) = &mut self.attached
        {
            attached.snapshot_owed = true;
        }
        if ack.durable
            && ack.delivery_epoch.as_deref() == Some(self.epoch.as_str())
            && ack.applied_through <= self.sequence
            && ack.applied_through > self.acked
        {
            self.acked = ack.applied_through;
            self.unacked.retain(|delta| delta.sequence > self.acked);
            outbox.position = Some((self.sequence, self.acked));
        }
        outbox
    }

    /// Whether a snapshot is owed to the current session.
    pub fn snapshot_owed(&self) -> bool {
        self.attached.as_ref().is_some_and(|a| a.snapshot_owed)
    }

    /// Brings the current session to `desired`: a snapshot when one is owed, otherwise a
    /// delta per [`SNAPSHOT_PAGE_OBJECTS`] changed objects. Nothing without a session.
    pub fn refresh(&mut self, desired: &Desired) -> Result<Outbox, SnapshotError> {
        let Some(attached) = &self.attached else {
            return Ok(Outbox::default());
        };
        if attached.snapshot_owed || self.sent.is_none() {
            return self.snapshot(desired);
        }
        let sent = self.sent.as_ref().expect("checked above");
        let mut changes: Vec<CacheObject> = Vec::new();
        let live: BTreeMap<Uuid, &OccurrenceObject> =
            desired.live.iter().map(|o| (o.occurrence_id, o)).collect();
        for (id, object) in sent {
            if !live.contains_key(id) {
                changes.push(CacheObject::Tombstone(TombstoneObject {
                    occurrence_id: *id,
                    revision: 0,
                    tombstone: desired
                        .reasons
                        .get(id)
                        .copied()
                        .unwrap_or(TombstoneReason::Deleted),
                }));
                let _ = object;
            }
        }
        for (id, object) in &live {
            if sent.get(id).is_none_or(|old| !same_content(old, object)) {
                changes.push(CacheObject::Occurrence((*object).clone()));
            }
        }
        let mut outbox = Outbox::default();
        for chunk in changes.chunks(SNAPSHOT_PAGE_OBJECTS) {
            self.sequence += 1;
            let sequence = self.sequence;
            let objects: Vec<CacheObject> = chunk
                .iter()
                .cloned()
                .map(|object| match object {
                    CacheObject::Occurrence(o) => CacheObject::Occurrence(OccurrenceObject {
                        revision: sequence,
                        ..o
                    }),
                    CacheObject::Tombstone(t) => CacheObject::Tombstone(TombstoneObject {
                        revision: sequence,
                        ..t
                    }),
                })
                .collect();
            let sent = self.sent.as_mut().expect("checked above");
            for object in &objects {
                match object {
                    CacheObject::Occurrence(o) => {
                        sent.insert(o.occurrence_id, o.clone());
                    }
                    CacheObject::Tombstone(t) => {
                        sent.remove(&t.occurrence_id);
                    }
                }
            }
            let delta = AlertDelta {
                delivery_epoch: self.epoch.clone(),
                sequence,
                objects,
            };
            self.unacked.push_back(delta.clone());
            outbox.bodies.push(PostBody::Delta(delta));
        }
        if !outbox.bodies.is_empty() {
            outbox.position = Some((self.sequence, self.acked));
        }
        Ok(outbox)
    }

    /// Tombstones occurrences the device may hold that this speaker never delivered (a
    /// snooze child the device derived whose snooze was rejected), in one delta.
    pub fn retract(&mut self, removed: &[(Uuid, TombstoneReason)]) -> Outbox {
        let in_step = self.attached.as_ref().is_some_and(|a| !a.snapshot_owed);
        let Some(sent) = self
            .sent
            .as_mut()
            .filter(|_| in_step && !removed.is_empty())
        else {
            return Outbox::default();
        };
        self.sequence += 1;
        let sequence = self.sequence;
        let objects = removed
            .iter()
            .map(|(id, reason)| {
                sent.remove(id);
                CacheObject::Tombstone(TombstoneObject {
                    occurrence_id: *id,
                    revision: sequence,
                    tombstone: *reason,
                })
            })
            .collect();
        let delta = AlertDelta {
            delivery_epoch: self.epoch.clone(),
            sequence,
            objects,
        };
        self.unacked.push_back(delta.clone());
        Outbox {
            bodies: vec![PostBody::Delta(delta)],
            position: Some((self.sequence, self.acked)),
        }
    }

    fn snapshot(&mut self, desired: &Desired) -> Result<Outbox, SnapshotError> {
        let high_water_mark = self.sequence + 1;
        let previous = self.sent.take().unwrap_or_default();
        let objects: Vec<OccurrenceObject> = desired
            .live
            .iter()
            .map(|object| {
                let revision = previous
                    .get(&object.occurrence_id)
                    .filter(|old| same_content(old, object))
                    .map_or(high_water_mark, |old| old.revision);
                OccurrenceObject {
                    revision,
                    ..object.clone()
                }
            })
            .collect();
        let pages = match snapshot_pages(&self.epoch, high_water_mark, objects.clone()) {
            Ok(pages) => pages,
            Err(error) => {
                self.sent = Some(previous);
                return Err(error);
            }
        };
        self.sequence = high_water_mark;
        self.sent = Some(objects.into_iter().map(|o| (o.occurrence_id, o)).collect());
        self.unacked.clear();
        if let Some(attached) = &mut self.attached {
            attached.snapshot_owed = false;
        }
        Ok(Outbox {
            bodies: pages.into_iter().map(PostBody::Snapshot).collect(),
            position: Some((self.sequence, self.acked)),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use echomuse_protocol::{
        Sha256,
        alerts::{AlertSound, OccurrenceKind, snapshot_digest},
        canonical::canonical_json_text,
    };

    use super::*;

    fn object(id: u128, due: u64, label: &str) -> OccurrenceObject {
        OccurrenceObject {
            due_local: "2026-09-23T06:30:00-05:00".to_owned(),
            due_utc_ms: due,
            kind: OccurrenceKind::Alarm,
            label: label.to_owned(),
            loop_gap_ms: 2_000,
            max_ring_ms: NonZeroU64::new(600_000).unwrap(),
            occurrence_id: Uuid::from_u128(id),
            ramp_ms: 20_000,
            revision: 0,
            schedule_id: Uuid::from_u128(1_000 + id),
            snooze_ms: 540_000,
            sound: AlertSound::Fallback,
            volume: None,
        }
    }

    fn desired(objects: Vec<OccurrenceObject>) -> Desired {
        Desired {
            live: objects,
            reasons: HashMap::new(),
        }
    }

    fn hello(epoch: Option<&str>, acked: u64) -> HelloAlerts {
        HelloAlerts {
            delivery_epoch: epoch.map(str::to_owned),
            acked_sequence: Some(acked),
            wakeup: None,
        }
    }

    fn ack(epoch: &str, through: u64) -> AlertAck {
        AlertAck {
            delivery_epoch: Some(epoch.to_owned()),
            applied_through: through,
            durable: true,
            need_snapshot: false,
        }
    }

    fn pages(outbox: &Outbox) -> Vec<&echomuse_protocol::alerts::AlertSnapshotPage> {
        outbox
            .bodies
            .iter()
            .map(|body| match body {
                PostBody::Snapshot(page) => page,
                other => panic!("expected a snapshot page, got {other:?}"),
            })
            .collect()
    }

    fn deltas(outbox: &Outbox) -> Vec<&AlertDelta> {
        outbox
            .bodies
            .iter()
            .map(|body| match body {
                PostBody::Delta(delta) => delta,
                other => panic!("expected a delta, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_new_device_gets_a_paged_snapshot_whose_digest_the_firmware_recomputes() {
        let mut delivery = Delivery::new("e1".to_owned(), 0, 0);
        assert!(
            delivery
                .attach(SessionId(1), Some(&hello(None, 0)))
                .bodies
                .is_empty()
        );
        assert!(delivery.snapshot_owed());
        let live: Vec<_> = (0..300u128)
            .map(|i| object(i, 10_000 - i as u64, "A"))
            .collect();
        let outbox = delivery.refresh(&desired(live)).unwrap();
        assert_eq!(outbox.position, Some((1, 0)));
        let pages = pages(&outbox);
        assert_eq!(
            pages.iter().map(|p| p.objects.len()).collect::<Vec<_>>(),
            [128, 128, 44]
        );
        assert!(
            pages
                .iter()
                .all(|p| p.high_water_mark == 1 && p.delivery_epoch == "e1")
        );
        // The digest is over the canonical JSON of every page's objects in order, as the
        // device re-encodes them (`device/internal/alerts/delivery.go`).
        let all: Vec<OccurrenceObject> = pages.iter().flat_map(|p| p.objects.clone()).collect();
        let wire = serde_json::to_string(&all).unwrap();
        let firmware = canonical_json_text(&wire).unwrap();
        assert_eq!(pages[0].sha256, Sha256::of(firmware.as_bytes()));
        assert_eq!(snapshot_digest(&all).unwrap(), pages[0].sha256);
        assert!(all.windows(2).all(|w| w[0].due_utc_ms <= w[1].due_utc_ms));
        assert!(all.iter().all(|o| o.revision == 1));
        assert!(!delivery.snapshot_owed());
        assert!(!delivery.armed(Uuid::from_u128(5)));
        delivery.ack(&ack("e1", 1));
        assert!(delivery.armed(Uuid::from_u128(5)));
    }

    #[test]
    fn deltas_follow_in_sequence_with_upserts_and_tombstones() {
        let mut delivery = Delivery::new("e1".to_owned(), 0, 0);
        let _ = delivery.attach(SessionId(1), None);
        let first = desired(vec![object(1, 100, "A"), object(2, 200, "B")]);
        let _ = delivery.refresh(&first).unwrap();
        delivery.ack(&ack("e1", 1));

        // Nothing changed: nothing is sent and the sequence stays.
        assert_eq!(delivery.refresh(&first).unwrap(), Outbox::default());

        let mut next = desired(vec![object(2, 200, "B renamed"), object(3, 300, "C")]);
        next.reasons
            .insert(Uuid::from_u128(1), TombstoneReason::Dismissed);
        let outbox = delivery.refresh(&next).unwrap();
        assert_eq!(outbox.position, Some((2, 1)));
        let [delta] = deltas(&outbox)[..] else {
            panic!("one delta");
        };
        assert_eq!(delta.sequence, 2);
        assert_eq!(
            delta.objects,
            vec![
                CacheObject::Tombstone(TombstoneObject {
                    occurrence_id: Uuid::from_u128(1),
                    revision: 2,
                    tombstone: TombstoneReason::Dismissed
                }),
                CacheObject::Occurrence(OccurrenceObject {
                    revision: 2,
                    ..object(2, 200, "B renamed")
                }),
                CacheObject::Occurrence(OccurrenceObject {
                    revision: 2,
                    ..object(3, 300, "C")
                }),
            ]
        );

        // A stale ack changes nothing; the device asking for a snapshot gets one at the
        // next sequence, keeping unchanged objects' revisions.
        assert_eq!(delivery.ack(&ack("e1", 0)).position, None);
        let mut need = ack("e1", 1);
        need.need_snapshot = true;
        let _ = delivery.ack(&need);
        let outbox = delivery.refresh(&next).unwrap();
        let pages = pages(&outbox);
        assert_eq!(pages[0].high_water_mark, 3);
        assert!(pages[0].objects.iter().all(|o| o.revision == 2));
    }

    #[test]
    fn a_matching_hello_resends_only_what_the_device_lacks() {
        let mut delivery = Delivery::new("e1".to_owned(), 0, 0);
        let _ = delivery.attach(SessionId(1), None);
        let _ = delivery
            .refresh(&desired(vec![object(1, 100, "A")]))
            .unwrap();
        delivery.ack(&ack("e1", 1));
        let _ = delivery
            .refresh(&desired(vec![object(1, 100, "A"), object(2, 200, "B")]))
            .unwrap();
        let _ = delivery
            .refresh(&desired(vec![object(2, 200, "B")]))
            .unwrap();
        delivery.detach(SessionId(1));
        assert_eq!(delivery.position(), (3, 1));

        // The device applied delta 2 durably before the link dropped.
        let outbox = delivery.attach(SessionId(2), Some(&hello(Some("e1"), 2)));
        assert_eq!(outbox.position, Some((3, 2)));
        assert_eq!(
            deltas(&outbox)
                .iter()
                .map(|d| d.sequence)
                .collect::<Vec<_>>(),
            [3]
        );
        assert!(!delivery.snapshot_owed());

        // In step: nothing to send.
        delivery.ack(&ack("e1", 3));
        delivery.detach(SessionId(2));
        assert!(
            delivery
                .attach(SessionId(3), Some(&hello(Some("e1"), 3)))
                .bodies
                .is_empty()
        );
        assert!(!delivery.snapshot_owed());

        // Another epoch, or a sequence ahead of ours, needs a snapshot.
        delivery.detach(SessionId(3));
        let _ = delivery.attach(SessionId(4), Some(&hello(Some("other"), 3)));
        assert!(delivery.snapshot_owed());
        delivery.detach(SessionId(4));
        let _ = delivery.attach(SessionId(5), Some(&hello(Some("e1"), 9)));
        assert!(delivery.snapshot_owed());
    }

    #[test]
    fn a_restarted_controller_does_not_trust_a_matching_hello() {
        // The row says the device is at 7; this process never sent epoch e1's state.
        let mut delivery = Delivery::new("e1".to_owned(), 7, 7);
        let _ = delivery.attach(SessionId(1), Some(&hello(Some("e1"), 7)));
        assert!(delivery.snapshot_owed());
        let outbox = delivery.refresh(&desired(vec![object(1, 1, "A")])).unwrap();
        assert_eq!(pages(&outbox)[0].high_water_mark, 8);
    }

    #[test]
    fn without_a_session_nothing_is_sent_and_changes_wait_for_the_next() {
        let mut delivery = Delivery::new("e1".to_owned(), 0, 0);
        let _ = delivery.attach(SessionId(1), None);
        let _ = delivery.refresh(&desired(vec![object(1, 1, "A")])).unwrap();
        delivery.ack(&ack("e1", 1));
        delivery.detach(SessionId(1));
        assert_eq!(
            delivery.refresh(&desired(vec![])).unwrap(),
            Outbox::default()
        );
        let _ = delivery.attach(SessionId(2), Some(&hello(Some("e1"), 1)));
        let outbox = delivery.refresh(&desired(vec![])).unwrap();
        let [delta] = deltas(&outbox)[..] else {
            panic!("one delta");
        };
        assert_eq!(delta.sequence, 2);
        assert!(matches!(
            delta.objects[0],
            CacheObject::Tombstone(TombstoneObject {
                tombstone: TombstoneReason::Deleted,
                ..
            })
        ));
    }
}
