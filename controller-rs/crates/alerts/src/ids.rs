//! The identifiers the alert service derives (§3.3.4). The device caches them and the
//! journal stores them, so every formula here is a contract and cannot change.
//!
//! Every UUIDv5 below hashes with SHA-1 over a namespace and the UTF-8 bytes of a string,
//! as RFC 4122 and Python's `uuid.uuid5` do; the firmware's `uuid.V5` is the same function.

use std::fmt;

use uuid::Uuid;

/// RFC 4122's URL namespace, `6ba7b811-9dad-11d1-80b4-00c04fd430c8`.
pub const NAMESPACE_URL: Uuid = Uuid::NAMESPACE_URL;

/// An alert operation's id: the `op_id` of the journal, of `alert.act` and of
/// `alert.local_operation`. Built only through the formulas below, so a caller cannot
/// supply an id that breaks idempotency.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OpId(Uuid);

impl OpId {
    /// A voice alarm operation: `uuid5(URL, origin_turn_id)`, where the origin is the turn
    /// that first asked, kept across an AM/PM clarification.
    pub fn voice_alarm(origin_turn_id: &str) -> Self {
        Self(Uuid::new_v5(&NAMESPACE_URL, origin_turn_id.as_bytes()))
    }

    /// A local alert action of a turn: `uuid5(URL, turn_id + "|" + action)`.
    pub fn local_action(turn_id: &str, action: &str) -> Self {
        Self(Uuid::new_v5(
            &NAMESPACE_URL,
            format!("{turn_id}|{action}").as_bytes(),
        ))
    }

    /// An LLM relay operation: `uuid5(URL, request_id)`.
    pub fn relay(request_id: &str) -> Self {
        Self(Uuid::new_v5(&NAMESPACE_URL, request_id.as_bytes()))
    }

    /// An expiry of a missed occurrence: `uuid5(URL, "echomuse-missed:" + occurrence_id)`.
    pub fn missed(occurrence_id: Uuid) -> Self {
        Self(Uuid::new_v5(
            &NAMESPACE_URL,
            format!("echomuse-missed:{occurrence_id}").as_bytes(),
        ))
    }

    /// A further target of a group cancellation: `uuid5(root_op_id, schedule_id)`. The
    /// first target uses the root id itself.
    pub fn group_member(root: OpId, schedule_id: Uuid) -> Self {
        Self(Uuid::new_v5(&root.0, schedule_id.to_string().as_bytes()))
    }

    /// A fresh random id, for a dashboard request (UUIDv4, as the firmware expects for
    /// anything that is not a voice command or an LLM tool call).
    pub fn dashboard() -> Self {
        Self(Uuid::new_v4())
    }

    /// An operation the device journaled and uploaded under its own id.
    pub fn from_device(id: Uuid) -> Self {
        Self(id)
    }

    /// An id read back from the journal.
    pub(crate) fn from_journal(id: Uuid) -> Self {
        Self(id)
    }

    /// The UUID.
    pub fn uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The schedule a new alarm creates: `uuid5(op_id, "schedule")`.
pub fn new_schedule_id(op: OpId) -> Uuid {
    Uuid::new_v5(&op.0, b"schedule")
}

/// An occurrence: `uuid5(schedule_id, key)`, where the key is the instance's
/// `recurrence_id`, or for a one-shot event its start in ISO 8601 with offset at second
/// precision.
pub fn occurrence_id(schedule_id: Uuid, key: &str) -> Uuid {
    Uuid::new_v5(&schedule_id, key.as_bytes())
}

/// A snooze child's schedule (also its `echomuse:` `id`):
/// `uuid5(URL, "echomuse-snooze:" + parent occurrence_id)`.
pub fn snooze_child_schedule_id(parent_occurrence: Uuid) -> Uuid {
    Uuid::new_v5(
        &NAMESPACE_URL,
        format!("echomuse-snooze:{parent_occurrence}").as_bytes(),
    )
}

/// The schedule of an event created in HA's UI or by an automation, which carries no
/// `echomuse:` line: `uuid5(URL, "<calendar entity>/<uid>")`.
pub fn ui_schedule_id(calendar_entity: &str, uid: &str) -> Uuid {
    Uuid::new_v5(
        &NAMESPACE_URL,
        format!("{calendar_entity}/{uid}").as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The firmware's own vectors (`device/internal/alerts/canonical_test.go`), which pin
    /// the snooze child, occurrence and UI-event formulas on both sides of the link.
    #[test]
    fn the_formulas_match_the_firmware_vectors() {
        let parent: Uuid = "0b8f4e0a-3c4b-5e6d-9f10-112233445566".parse().unwrap();
        let child = snooze_child_schedule_id(parent);
        assert_eq!(child.to_string(), "9cc967de-5fa3-5373-b4f0-d9841f3f79f7");
        assert_eq!(
            occurrence_id(child, "2026-09-23T06:39:01-05:00").to_string(),
            "164ed469-897e-5b59-b53b-23431a19381b"
        );
        assert_eq!(
            ui_schedule_id("calendar.echomuse_office", "abc").to_string(),
            "09e72c24-77e1-5473-859a-025b5a1115df"
        );
    }

    /// Vectors from Python's `uuid.uuid5`, which the journal and the device share.
    #[test]
    fn the_operation_formulas_match_python_uuid5() {
        let text = |id: Uuid| id.to_string();
        assert_eq!(
            text(OpId::voice_alarm("turn-1").uuid()),
            "d44277be-3b12-5cfb-b366-521a156067cd"
        );
        assert_eq!(
            text(OpId::local_action("t7", "dismiss").uuid()),
            "4f55f6f9-5ff2-54ca-b76c-78f087ea1d22"
        );
        let root = OpId::relay("ctx|cancel_alarm|{}");
        assert_eq!(text(root.uuid()), "ee34985f-1d96-505a-9ca7-b8b91d683a76");
        let schedule = Uuid::from_u128(5);
        assert_eq!(
            text(OpId::group_member(root, schedule).uuid()),
            "1f0900f8-40de-5cda-9914-0506a3b7854f"
        );
        assert_eq!(
            text(new_schedule_id(root)),
            "520c1611-284c-58a1-bebf-cb2eebf84dfa"
        );
        assert_eq!(
            text(OpId::missed(schedule).uuid()),
            "49c2765e-a8b8-548c-84c8-d283a2ed3ab0"
        );
        assert_eq!(OpId::dashboard().uuid().get_version_num(), 4);
    }
}
