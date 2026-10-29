//! Alarms from the dashboard (spec §4.9; dashboard.jsx `AlertPanel`): the alert panel's
//! status, and alarms set and cancelled through the alert service under fresh dashboard
//! operation ids (`OpId::dashboard`).
//!
//! The panel's `status` joins the service's view of the speaker
//! ([`AlertHandle::status`]), its running timers as the display copy shows them, and the
//! speaker's journal operations of the last 24 hours plus every pending one, read from
//! `alert_ops`. A delivery position the service has not loaded is `null`, not zero.
//!
//! Outcomes follow §3.3.3: an operation still being confirmed is reported `pending`, a
//! rejected one with its reason; nothing is retried under a new id.

use std::{
    collections::BTreeSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use echomuse_alerts::{
    AlertHandle, CancelAlarms, CancelOutcome, OpId, OpSource, Outcome, Refusal, SetAlarm,
    relay::CancelSelector,
    schedule::{AlarmWhen, Repeat, TimeOfDay, Weekdays},
    status::EndpointStatus,
};
use echomuse_api::{AlarmCancel, AlarmReply, AlarmRequest, AlertControl, BoxFuture, ServiceError};
use echomuse_core::types::DeviceId;
use echomuse_db::{DbHandle, ReadQuery};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

/// The longest alarm label the device accepts (§3.1.8).
const MAX_LABEL_CHARS: usize = 120;
/// How far back the panel lists finished operations.
const OPERATIONS_WINDOW: Duration = Duration::from_secs(24 * 3600);
/// The most operations the panel lists.
const OPERATIONS_LIMIT: u32 = 50;
/// The most upcoming alarms read to match a cancellation.
const CANCEL_SCAN: usize = 4_096;

/// [`AlertControl`] over the alert service.
pub struct DashboardAlerts {
    pub alerts: AlertHandle,
    pub db: DbHandle,
}

fn stopped() -> ServiceError {
    ServiceError::Unavailable("The alert service has stopped".to_owned())
}

/// The speaker's journal operations: every pending one and the others since `since_ms`.
struct RecentOperations {
    device: DeviceId,
    since_ms: i64,
}

impl ReadQuery for RecentOperations {
    type Output = Vec<Value>;
    type Error = rusqlite::Error;

    fn read(self, conn: &Connection) -> rusqlite::Result<Vec<Value>> {
        let mut statement = conn.prepare_cached(
            "SELECT op_id, action, state, step, source, created_ms FROM alert_ops \
             WHERE endpoint_id = ?1 AND (state = 'pending' OR created_ms >= ?2) \
             ORDER BY seq DESC LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![self.device.0, self.since_ms, OPERATIONS_LIMIT],
            |row| {
                Ok(json!({
                    "op_id": row.get::<_, String>(0)?,
                    "action": row.get::<_, String>(1)?,
                    "state": row.get::<_, String>(2)?,
                    "step": row.get::<_, i64>(3)?,
                    "source": row.get::<_, String>(4)?,
                    "created_ms": row.get::<_, i64>(5)?,
                }))
            },
        )?;
        rows.collect()
    }
}

/// The panel's `status` object.
fn status_json(status: EndpointStatus, timers: Value, operations: Vec<Value>) -> Value {
    let alert_state = status.alert_state.map(|state| {
        json!({
            "active": state.active.map(|ring| json!({
                "kind": ring.kind.as_str(),
                "id": ring.id,
                "name": ring.name,
                "foreground": ring.foreground,
            })),
            "queue": state.queue,
            "clock_trusted": state.clock_trusted,
            "wakelock": state.wakelock.as_str(),
            "store": state.store.as_str(),
        })
    });
    json!({
        "calendar_live": status.calendar_live,
        "online": status.online,
        "sequence": status.delivery.map(|d| d.sequence),
        "acked_sequence": status.delivery.map(|d| d.acked),
        "flags": status.flags.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
        "timers": timers,
        "occurrences": status.occurrences.iter().map(|o| json!({
            "occurrence_id": o.occurrence_id,
            "schedule_id": o.schedule_id,
            "kind": o.kind.as_str(),
            "label": o.label,
            "due_local": o.due_local,
            "armed_on_endpoint": o.armed_on_endpoint,
            "delivery_pending": !o.armed_on_endpoint,
            "stored_in_ha": true,
        })).collect::<Vec<_>>(),
        "operations": operations,
        "alert_state": alert_state,
    })
}

/// What a dashboard alarm request asks for, or why it cannot be asked.
fn alarm_when(request: &AlarmRequest) -> Result<(AlarmWhen, Option<String>), String> {
    let time =
        TimeOfDay::parse(request.time.trim()).ok_or_else(|| "The time must be HH:MM".to_owned())?;
    let days = Weekdays::from_names(request.days.iter().map(String::as_str))
        .ok_or_else(|| "Days must be mon, tue, wed, thu, fri, sat or sun".to_owned())?;
    let date = match request.date.as_deref() {
        None => None,
        Some(text) => Some(
            text.parse::<jiff::civil::Date>()
                .map_err(|_| "The date must be YYYY-MM-DD".to_owned())?,
        ),
    };
    if date.is_some() && !days.is_empty() {
        return Err("An alarm repeats on days or rings on one date, not both".to_owned());
    }
    let label = request.name.trim();
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(format!(
            "An alarm name is at most {MAX_LABEL_CHARS} characters"
        ));
    }
    Ok((
        AlarmWhen {
            time,
            date,
            repeat: Repeat::on(days),
        },
        (!label.is_empty()).then(|| label.to_owned()),
    ))
}

fn refused(refusal: &Refusal) -> AlarmReply {
    AlarmReply {
        ok: false,
        error: Some(refusal.to_string()),
        ..AlarmReply::default()
    }
}

/// Whether an occurrence due at `due_local` (RFC 3339) rings at `time`; seconds count
/// only when the request named them.
fn same_time(due_local: &str, time: TimeOfDay, with_seconds: bool) -> bool {
    let Some((_, clock)) = due_local.split_once('T') else {
        return false;
    };
    let Some(due) = clock.get(..8).and_then(TimeOfDay::parse) else {
        return false;
    };
    due.hour() == time.hour()
        && due.minute() == time.minute()
        && (!with_seconds || due.second() == time.second())
}

impl DashboardAlerts {
    async fn status_now(&self, device: DeviceId) -> Result<Option<Value>, ServiceError> {
        let Some(status) = self
            .alerts
            .status(device.clone())
            .await
            .map_err(|_| stopped())?
        else {
            return Ok(None);
        };
        let timers = self
            .alerts
            .timer_views()
            .borrow()
            .get(&device.0)
            .map(|view| {
                json!(
                    view.timers
                        .iter()
                        .map(|t| json!({
                            "id": t.id,
                            "name": t.name,
                            "remaining_seconds": t.remaining_ms / 1_000,
                            "active": t.active,
                        }))
                        .collect::<Vec<_>>()
                )
            })
            .unwrap_or_else(|| json!([]));
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let since_ms = now_ms.saturating_sub(OPERATIONS_WINDOW.as_millis() as i64);
        let operations = self
            .db
            .read(RecentOperations { device, since_ms })
            .await
            .map_err(|error| ServiceError::Failed(format!("reading alert_ops: {error}")))?;
        Ok(Some(status_json(status, timers, operations)))
    }

    async fn set_now(
        &self,
        device: DeviceId,
        request: AlarmRequest,
    ) -> Result<AlarmReply, ServiceError> {
        let (when, label) = alarm_when(&request).map_err(ServiceError::Invalid)?;
        let outcome = self
            .alerts
            .set_alarm(SetAlarm {
                op_id: OpId::dashboard(),
                endpoint: device,
                when,
                label,
                source: OpSource::Dashboard,
            })
            .await
            .map_err(|_| stopped())?;
        Ok(match outcome {
            Outcome::Applied { result, .. } => AlarmReply {
                ok: true,
                armed_on_endpoint: result.armed_on_endpoint,
                delivery_pending: !result.armed_on_endpoint,
                ..AlarmReply::default()
            },
            Outcome::Pending { .. } => AlarmReply {
                ok: true,
                pending: true,
                ..AlarmReply::default()
            },
            Outcome::Rejected { reason, .. } => AlarmReply {
                ok: false,
                error: Some(format!(
                    "Home Assistant did not store it: {}",
                    reason.detail
                )),
                ..AlarmReply::default()
            },
            Outcome::Refused(refusal) => refused(&refusal),
        })
    }

    async fn cancel_now(
        &self,
        device: DeviceId,
        request: AlarmCancel,
    ) -> Result<AlarmReply, ServiceError> {
        let selector = if request.all {
            CancelSelector::All
        } else {
            let text = request.time.trim();
            let time = TimeOfDay::parse(text)
                .ok_or_else(|| ServiceError::Invalid("The time must be HH:MM".to_owned()))?;
            let with_seconds = text.matches(':').count() == 2;
            let listed = self
                .alerts
                .list_alarms(device.clone(), CANCEL_SCAN)
                .await
                .map_err(|_| stopped())?;
            let alarms = match listed {
                Ok(alarms) => alarms,
                Err(refusal) => return Ok(refused(&refusal)),
            };
            let name = request.name.trim();
            let schedules: BTreeSet<uuid::Uuid> = alarms
                .iter()
                .filter(|a| a.label.eq_ignore_ascii_case(name))
                .filter(|a| same_time(&a.due_local, time, with_seconds))
                .map(|a| a.schedule_id)
                .collect();
            if schedules.is_empty() {
                return Ok(refused(&Refusal::NothingToCancel));
            }
            CancelSelector::Schedules(schedules.into_iter().collect())
        };
        let outcome = self
            .alerts
            .cancel_alarms(CancelAlarms {
                op_id: OpId::dashboard(),
                endpoint: device,
                selector,
                source: OpSource::Dashboard,
            })
            .await
            .map_err(|_| stopped())?;
        Ok(match outcome {
            CancelOutcome::Refused(refusal) => refused(&refusal),
            CancelOutcome::Journaled { targets, .. } => {
                let mut reply = AlarmReply {
                    ok: true,
                    ..AlarmReply::default()
                };
                for target in targets {
                    match target.outcome {
                        Outcome::Applied { .. } => {}
                        Outcome::Pending { .. } => reply.pending = true,
                        Outcome::Rejected { reason, .. } => {
                            reply.ok = false;
                            reply.error = Some(format!(
                                "{} was not cancelled: {}",
                                target.label, reason.detail
                            ));
                        }
                        Outcome::Refused(refusal) => {
                            reply.ok = false;
                            reply.error =
                                Some(format!("{} was not cancelled: {refusal}", target.label));
                        }
                    }
                }
                reply
            }
        })
    }
}

impl AlertControl for DashboardAlerts {
    fn status(&self, device: DeviceId) -> BoxFuture<'_, Result<Option<Value>, ServiceError>> {
        Box::pin(self.status_now(device))
    }

    fn set_alarm(
        &self,
        device: DeviceId,
        request: AlarmRequest,
    ) -> BoxFuture<'_, Result<AlarmReply, ServiceError>> {
        Box::pin(self.set_now(device, request))
    }

    fn cancel_alarm(
        &self,
        device: DeviceId,
        request: AlarmCancel,
    ) -> BoxFuture<'_, Result<AlarmReply, ServiceError>> {
        Box::pin(self.cancel_now(device, request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(time: &str, days: &[&str], date: Option<&str>) -> AlarmRequest {
        AlarmRequest {
            time: time.to_owned(),
            days: days.iter().map(|d| (*d).to_owned()).collect(),
            name: " Wake up ".to_owned(),
            date: date.map(str::to_owned),
        }
    }

    #[test]
    fn dashboard_requests_become_schedules() {
        let (when, label) = alarm_when(&request("07:30", &["mon", "fri"], None)).unwrap();
        assert_eq!(label.as_deref(), Some("Wake up"));
        assert_eq!(when.time, TimeOfDay::new(7, 30, 0).unwrap());
        assert!(matches!(when.repeat, Repeat::Weekly(_)));
        let all: Vec<&str> = vec!["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
        assert_eq!(
            alarm_when(&request("6:00", &all, None)).unwrap().0.repeat,
            Repeat::Daily
        );
        let (dated, _) = alarm_when(&request("09:00", &[], Some("2026-12-24"))).unwrap();
        assert!(dated.date.is_some() && dated.repeat == Repeat::Once);
        assert!(alarm_when(&request("25:00", &[], None)).is_err());
        assert!(alarm_when(&request("07:00", &["someday"], None)).is_err());
        assert!(alarm_when(&request("07:00", &["mon"], Some("2026-12-24"))).is_err());
    }

    #[test]
    fn cancellations_match_the_time_of_day() {
        let seven = TimeOfDay::new(7, 0, 0).unwrap();
        assert!(same_time("2026-10-30T07:00:00+00:00", seven, false));
        assert!(!same_time("2026-10-30T07:01:00+00:00", seven, false));
        assert!(same_time("2026-10-30T07:00:30+00:00", seven, false));
        assert!(!same_time("2026-10-30T07:00:30+00:00", seven, true));
        assert!(!same_time("garbage", seven, false));
    }
}
