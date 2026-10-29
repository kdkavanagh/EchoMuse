//! The alert contract as a device runtime, the dashboard API and the integrator use it
//! (§5.4): requests, outcomes, the snapshot of what rings where, and notices.
//!
//! # Outcomes
//!
//! Every operation that can change a calendar ends in exactly one of:
//!
//! - **refused** ([`Refusal`]): nothing was journaled or sent, so nothing will happen;
//! - **rejected**: journaled under its id, then refused or never confirmed; nothing is
//!   stored in Home Assistant for it;
//! - **applied**: Home Assistant's calendar shows the result;
//! - **pending**: journaled and not yet confirmed. The service keeps applying it under the
//!   same id, after a restart too. Never repeat it under a new id, and never report it as
//!   stored.
//!
//! An `alert.act` has a fifth: **unknown**, when it may have reached the device but no
//! acknowledgement came back. It is never resent.

use std::{collections::BTreeMap, fmt, num::NonZeroU64};

use echomuse_core::{
    services::UnknownCause,
    types::{DeviceId, SessionId},
};
use echomuse_protocol::{
    Capabilities,
    alerts::{AlertSound, OccurrenceKind},
    session::HelloAlerts,
};
use uuid::Uuid;

use crate::{
    ids::OpId,
    journal,
    relay::CancelSelector,
    schedule::AlarmWhen,
    wire::{ActAction, ActSource, ActiveRing, AlertRingEnded, RingKind, TimerView},
};

/// A speaker the alert service serves: an approved device.
#[derive(Clone, Debug, PartialEq)]
pub struct EndpointConfig {
    /// The device.
    pub device: DeviceId,
    /// Its label; its calendar is titled `EchoMuse {label}` when first created.
    pub label: String,
    /// The HA area of its satellite, for the relay's speaker matching.
    pub area: Option<String>,
    /// The effective alarm sound for new alarms and events made in HA's UI.
    pub alarm_sound: AlertSound,
    /// The effective timer sound.
    pub timer_sound: AlertSound,
    /// The pause between timer sound loops, ms (`timerRingGapSeconds`).
    pub timer_loop_gap_ms: u64,
    /// How long a finished timer rings, ms (`timerRingSeconds`).
    pub timer_max_ring_ms: NonZeroU64,
}

impl EndpointConfig {
    /// The title of its Local Calendar.
    pub fn calendar_title(&self) -> String {
        format!("EchoMuse {}", self.label)
    }
}

/// A device session that has received `session.ready`.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceAttach {
    /// The controller's identity of the session.
    pub session: SessionId,
    /// What the device announced.
    pub capabilities: Capabilities,
    /// The hello's alert cache position; `None` when the hello carried none.
    pub alerts: Option<HelloAlerts>,
}

/// Who asked for a change to the calendar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpSource {
    /// A voice command.
    Voice,
    /// An LLM tool call.
    Llm,
    /// The dashboard.
    Dashboard,
}

impl OpSource {
    pub(crate) fn journal(self) -> journal::Source {
        match self {
            OpSource::Voice => journal::Source::Voice,
            OpSource::Llm => journal::Source::Llm,
            OpSource::Dashboard => journal::Source::Dashboard,
        }
    }
}

/// Set an alarm on a speaker.
#[derive(Clone, Debug, PartialEq)]
pub struct SetAlarm {
    /// The operation id: [`OpId::voice_alarm`], [`OpId::relay`] or [`OpId::dashboard`].
    pub op_id: OpId,
    /// The speaker.
    pub endpoint: DeviceId,
    /// When it rings.
    pub when: AlarmWhen,
    /// Its label; `None` is "Alarm".
    pub label: Option<String>,
    /// Who asked.
    pub source: OpSource,
}

/// Cancel alarms on a speaker.
#[derive(Clone, Debug, PartialEq)]
pub struct CancelAlarms {
    /// The root operation id; further targets get [`OpId::group_member`].
    pub op_id: OpId,
    /// The speaker.
    pub endpoint: DeviceId,
    /// Which alarms.
    pub selector: CancelSelector,
    /// Who asked.
    pub source: OpSource,
}

/// A ring as the requester saw it in [`RingsSnapshot`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RingRef {
    /// The device it rings on.
    pub device: DeviceId,
    /// The occurrence id or HA timer id.
    pub ring_id: String,
}

/// Stop or snooze a ring, on this speaker or another one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Act {
    /// The operation id: [`OpId::local_action`], [`OpId::relay`] or [`OpId::dashboard`].
    pub op_id: OpId,
    /// The ring; the service checks again that it is still what rings there.
    pub target: RingRef,
    /// Dismiss or snooze.
    pub action: ActAction,
    /// Who asked.
    pub source: ActSource,
}

/// Why nothing was journaled or sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// The device is not a speaker the service serves.
    UnknownEndpoint,
    /// Home Assistant is not connected, or the speaker's calendar has not been read on
    /// the current connection; a new alarm or cancellation is never queued.
    HaUnavailable,
    /// The request is malformed; the text says how.
    InvalidRequest(String),
    /// A one-shot alarm at a time already past.
    InThePast,
    /// The speaker has as many schedules as a calendar may hold (256).
    CapacityExceeded,
    /// No alarm matches the cancellation.
    NothingToCancel,
    /// The operation id was journaled with a different request.
    OpIdConflict,
    /// The device is not connected.
    DeviceNotConnected,
    /// Nothing rings on the device.
    NothingRinging,
    /// Something else rings there now; `now` is what does.
    RingChanged {
        /// The id that rings now.
        now: String,
    },
    /// Timers cannot be snoozed.
    NotSnoozable,
    /// The device refused the act before journaling it; its error code.
    DeviceRefused(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::UnknownEndpoint => f.write_str("unknown speaker"),
            Refusal::HaUnavailable => f.write_str("Home Assistant is unavailable"),
            Refusal::InvalidRequest(why) => write!(f, "invalid request: {why}"),
            Refusal::InThePast => f.write_str("that time has already passed"),
            Refusal::CapacityExceeded => f.write_str("the speaker has too many alarms"),
            Refusal::NothingToCancel => f.write_str("no alarm matches"),
            Refusal::OpIdConflict => f.write_str("op_id_conflict"),
            Refusal::DeviceNotConnected => f.write_str("the speaker is not connected"),
            Refusal::NothingRinging => f.write_str("nothing is ringing"),
            Refusal::RingChanged { now } => write!(f, "the ring changed; {now} rings now"),
            Refusal::NotSnoozable => f.write_str("timers cannot be snoozed"),
            Refusal::DeviceRefused(code) => write!(f, "the speaker refused: {code}"),
        }
    }
}

/// Why a journaled operation was rejected, as its stored result says.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rejection {
    /// A stable code (for example `ha_refused`, `not_confirmed`, `parent_gone`).
    pub code: String,
    /// The detail.
    pub detail: String,
}

/// The outcome of a calendar operation.
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub enum Outcome<T> {
    /// Nothing was journaled or sent.
    Refused(Refusal),
    /// Journaled, then rejected; nothing is stored for it.
    Rejected {
        /// The operation.
        op_id: OpId,
        /// Why.
        reason: Rejection,
    },
    /// Home Assistant's calendar shows it.
    Applied {
        /// The operation.
        op_id: OpId,
        /// What it did.
        result: T,
    },
    /// Journaled and still being applied under the same id.
    Pending {
        /// The operation.
        op_id: OpId,
    },
}

/// A new alarm, stored in Home Assistant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlarmSet {
    /// Its schedule.
    pub schedule_id: Uuid,
    /// Its first occurrence.
    pub occurrence_id: Uuid,
    /// When it first rings, RFC 3339 in HA's time zone.
    pub first_due_local: String,
    /// When it first rings, UTC ms.
    pub first_due_utc_ms: i64,
    /// Whether the speaker has durably cached that occurrence.
    pub armed_on_endpoint: bool,
}

/// One cancelled (or not) schedule of a cancellation.
#[derive(Clone, Debug, PartialEq)]
pub struct CancelTarget {
    /// The schedule.
    pub schedule_id: Uuid,
    /// Its label.
    pub label: String,
    /// What became of it.
    pub outcome: Outcome<()>,
}

/// The outcome of a cancellation.
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub enum CancelOutcome {
    /// Nothing was journaled.
    Refused(Refusal),
    /// One operation per schedule, the root first.
    Journaled {
        /// The root operation.
        root: OpId,
        /// Each schedule's outcome.
        targets: Vec<CancelTarget>,
    },
}

/// The outcome of an act.
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub enum ActOutcome {
    /// Nothing was sent, or the device refused it before journaling.
    Refused(Refusal),
    /// The device journaled the alarm operation and the controller rejected it (a snooze
    /// whose alarm was deleted meanwhile).
    Rejected {
        /// The operation.
        op_id: OpId,
        /// Why.
        reason: Rejection,
    },
    /// The ring stopped; for an alarm, the calendar shows the operation.
    Applied {
        /// The operation.
        op_id: OpId,
        /// What rang.
        kind: RingKind,
    },
    /// The ring stopped and the device journaled the operation; the calendar is not
    /// updated yet (Home Assistant may be away). It completes under the same id.
    Pending {
        /// The operation.
        op_id: OpId,
    },
    /// It may have reached the device, but no acknowledgement came back. Not resent.
    Unknown {
        /// The operation.
        op_id: OpId,
        /// Why the outcome is unknown.
        cause: UnknownCause,
    },
}

/// One upcoming alarm of a speaker.
#[derive(Clone, Debug, PartialEq)]
pub struct AlarmEntry {
    /// Its schedule.
    pub schedule_id: Uuid,
    /// The occurrence.
    pub occurrence_id: Uuid,
    /// Alarm or snooze child.
    pub kind: OccurrenceKind,
    /// Its label.
    pub label: String,
    /// When it rings, RFC 3339 in HA's time zone.
    pub due_local: String,
    /// When it rings, UTC ms.
    pub due_utc_ms: i64,
    /// The series' recurrence rule; `None` for a one-shot.
    pub rrule: Option<String>,
    /// Whether the speaker has durably cached it.
    pub armed_on_endpoint: bool,
}

/// What rings on one connected speaker, from its latest `alert.state`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceRings {
    /// The speaker's label.
    pub label: String,
    /// The session the state came from.
    pub session: SessionId,
    /// What rings; `None` when nothing does.
    pub active: Option<ActiveRing>,
    /// What waits behind it.
    pub queue: Vec<String>,
}

/// Every served speaker's timer view, by device id ([`AlertHandle::timer_views`]).
///
/// [`AlertHandle::timer_views`]: crate::AlertHandle::timer_views
pub type TimerViews = BTreeMap<String, TimerView>;

/// What rings where. Speakers that are not connected, or have not reported since they
/// connected, are absent: unknown, not silent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RingsSnapshot {
    /// By device id.
    pub devices: BTreeMap<String, DeviceRings>,
}

impl RingsSnapshot {
    /// The speakers other than `near` with something ringing.
    pub fn ringing_elsewhere(&self, near: &DeviceId) -> Vec<(DeviceId, &ActiveRing)> {
        self.devices
            .iter()
            .filter(|(id, _)| **id != near.0)
            .filter_map(|(id, rings)| Some((DeviceId(id.clone()), rings.active.as_ref()?)))
            .collect()
    }
}

/// The state of one LLM tool script.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptState {
    /// Installed or upgraded on this connection.
    Installed,
    /// HA holds exactly the configuration this controller renders.
    Current,
    /// The user edited it; it is left alone.
    LeftEdited,
    /// It could not be checked or written; the text says why.
    Failed(String),
}

/// Something the dashboard or the logs should show.
#[derive(Clone, Debug, PartialEq)]
pub enum AlertNotice {
    /// A timer's copy reached zero and HA never said it finished (`timer_finish_missed`).
    TimerFinishMissed {
        /// The speaker.
        device: DeviceId,
        /// HA's timer id.
        timer_id: String,
        /// Its name.
        name: Option<String>,
    },
    /// A finished timer could not ring because the speaker is not connected
    /// (`timer_ring_undeliverable`).
    TimerRingUndeliverable {
        /// The speaker.
        device: DeviceId,
        /// HA's timer id.
        timer_id: String,
    },
    /// A ring ended on a speaker (telemetry).
    RingEnded {
        /// The speaker.
        device: DeviceId,
        /// The device's report.
        ended: AlertRingEnded,
    },
    /// A journaled operation was rejected.
    OperationRejected {
        /// The speaker.
        device: DeviceId,
        /// The operation.
        op_id: OpId,
        /// Why.
        reason: Rejection,
    },
    /// An event's `echomuse:` line is malformed; it rings with the defaults.
    MalformedEvent {
        /// The speaker.
        device: DeviceId,
        /// The event's uid.
        uid: String,
        /// What is wrong.
        detail: String,
    },
    /// The speaker's calendar could not be found, created, read or subscribed.
    CalendarProblem {
        /// The speaker.
        device: DeviceId,
        /// What failed.
        detail: String,
    },
    /// More than 4,096 occurrences in the window; the earliest are delivered.
    TooManyOccurrences {
        /// The speaker.
        device: DeviceId,
        /// How many there are.
        count: usize,
    },
    /// A journal row this controller cannot apply is left pending.
    UnreadableJournalRow {
        /// The row's sequence.
        seq: i64,
        /// Why.
        detail: String,
    },
    /// A database write failed; the text says which.
    DatabaseProblem(String),
    /// A script's state changed.
    Script {
        /// Its object id.
        object_id: String,
        /// Its state.
        state: ScriptState,
    },
    /// Home Assistant's time zone is not one the time zone database knows; alarms
    /// cannot be placed until it is.
    TimeZoneUnknown(String),
    /// A device alert message was malformed and ignored.
    MalformedMessage {
        /// The speaker.
        device: DeviceId,
        /// What is wrong.
        detail: String,
    },
}
