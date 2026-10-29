//! The alert service: alarms and timers, the operation journal, calendar reconciliation
//! with Home Assistant and delivery of the alert cache to devices (spec §3.1.8, §3.3.4,
//! §3.5, §4.9, §5.4; architecture §10 and §16.3–§16.7).
//!
//! - **Alarms** live in one Local Calendar per speaker, titled `EchoMuse {label}`, as
//!   one-minute events in HA's time zone with an `echomuse:` line ([`description`]).
//!   Every change is an operation journaled in `alert_ops` before anything is written
//!   ([`journal`]) and applied to the calendar in order, recovering under its original id
//!   after a restart or an HA outage. While HA is unavailable, new alarms and
//!   cancellations are refused rather than queued.
//! - **Delivery** keeps each device's alert cache in step through snapshots and deltas
//!   under a per-speaker epoch and sequence ([`delivery`]), with the firmware's own
//!   wire bodies ([`wire`]). Devices ring alarms from their cache; operations they
//!   journal locally (dismiss, snooze, expire) are merged back into the calendar.
//! - **Timers** belong to HA; the service rings them when they finish, shows them on
//!   devices that draw them, and reports a finish that never came ([`timers`]).
//! - **LLM tools**: five scripts relay requests to the service and wait for its answer
//!   ([`scripts`], [`relay`]).
//!
//! The service ([`AlertService`]) owns all of this state and is reached through
//! [`AlertHandle`]; [`ports`] lists what the integrator supplies. Identifiers follow the
//! fixed formulas in [`ids`].
//!
//! Not covered here: editing an existing alarm (the journal's `update` action), the
//! dashboard's HTTP routes and their JSON, resolving catalog sound ids to asset hashes
//! (the caller passes the effective sounds in [`EndpointConfig`]), and the HA startup
//! probe's dashboard report.

#![warn(missing_docs)]

pub mod api;
pub mod calendar;
pub mod delivery;
pub mod description;
pub mod ids;
pub mod journal;
pub mod ports;
pub mod relay;
pub mod schedule;
pub mod scripts;
mod service;
pub mod status;
pub mod timers;
pub mod wire;

pub use api::{
    Act, ActOutcome, AlarmEntry, AlarmSet, AlertNotice, CancelAlarms, CancelOutcome, CancelTarget,
    DeviceAttach, DeviceRings, EndpointConfig, OpSource, Outcome, Refusal, Rejection, RingRef,
    RingsSnapshot, ScriptState, SetAlarm, TimerViews,
};
pub use ids::OpId;
pub use ports::{
    AlertClock, AlertHa, AlertLink, CommandOutcome, LinkRefusal, SessionRef, SystemClock,
};
pub use service::{
    AlertConfig, AlertHandle, AlertService, IntentWaitGuard, ServiceFailure, ServiceStopped,
};
