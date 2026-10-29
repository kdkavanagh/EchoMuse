//! What the dashboard's alert panel shows for one speaker (`GET /api/devices/{id}/alerts`):
//! whether its calendar is followed, whether its session is attached, where its alert
//! cache delivery stands, its upcoming occurrences and what the device last said about
//! its own alert state. Read with [`AlertHandle::status`](crate::AlertHandle::status).

use echomuse_protocol::alerts::OccurrenceKind;
use uuid::Uuid;

use crate::wire::AlertState;

/// Something about a speaker's alerts that needs attention. The dashboard shows each as
/// its name with the underscores read as spaces.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum StatusFlag {
    /// Home Assistant is not connected.
    HaUnavailable,
    /// Home Assistant's time zone is unknown, so occurrences cannot be placed.
    TimeZoneUnknown,
    /// The speaker's calendar has not been found or created yet.
    CalendarNotBound,
    /// The device asked for a fresh snapshot of its alert cache, which is still owed.
    SnapshotOwed,
    /// Some calendar events carry a malformed `echomuse:` line and ring with the defaults.
    MalformedEvents,
}

impl StatusFlag {
    /// The flag's name, as the dashboard shows it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HaUnavailable => "home_assistant_unavailable",
            Self::TimeZoneUnknown => "time_zone_unknown",
            Self::CalendarNotBound => "calendar_not_bound",
            Self::SnapshotOwed => "snapshot_owed",
            Self::MalformedEvents => "malformed_calendar_events",
        }
    }
}

/// Where a speaker's alert cache delivery stands within its delivery epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeliveryPosition {
    /// The highest sequence the controller has sent.
    pub sequence: u64,
    /// The highest sequence the device has acknowledged durably.
    pub acked: u64,
}

/// One upcoming occurrence, as the calendar shows it and as far as the device has it.
#[derive(Clone, Debug, PartialEq)]
pub struct OccurrenceStatus {
    /// Its schedule.
    pub schedule_id: Uuid,
    /// The occurrence.
    pub occurrence_id: Uuid,
    /// Alarm or snooze child.
    pub kind: OccurrenceKind,
    /// Its label.
    pub label: String,
    /// When it rings, RFC 3339 in Home Assistant's time zone.
    pub due_local: String,
    /// The device has durably cached it.
    pub armed_on_endpoint: bool,
}

/// One speaker's alert status.
#[derive(Clone, Debug, PartialEq)]
pub struct EndpointStatus {
    /// The calendar is read on the current Home Assistant connection.
    pub calendar_live: bool,
    /// A session that received `session.ready` is attached.
    pub online: bool,
    /// The delivery position; `None` before the delivery cursor is loaded.
    pub delivery: Option<DeliveryPosition>,
    /// What needs attention, in a stable order.
    pub flags: Vec<StatusFlag>,
    /// Upcoming occurrences, earliest first. Empty while the calendar is not live.
    pub occurrences: Vec<OccurrenceStatus>,
    /// The device's latest `alert.state` this session; `None` before one arrived.
    pub alert_state: Option<AlertState>,
}
