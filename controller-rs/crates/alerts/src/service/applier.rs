//! One speaker's applier: applies its journaled operations to the calendar strictly in
//! `seq` order, one at a time (architecture §10.3, §16.4).
//!
//! For each operation it waits until Home Assistant is connected and the calendar has
//! been read on that connection, then performs the recorded writes in order, advancing
//! `step` after each acknowledged write, and marks the operation applied once the
//! subscription shows the final state. If that has not appeared 10 s after the write it
//! re-subscribes to force a fresh read, and if it is still missing the operation is
//! rejected. A create is deduplicated by its operation id on the calendar and a delete
//! of something already gone counts as done, so after an interrupted or unanswered write
//! the operation is recovered by reading the calendar afresh and repeating only what is
//! missing, under its original id.

use std::{sync::Arc, time::Duration};

use echomuse_core::services::EffectOutcome;
use echomuse_db::DbHandle;
use echomuse_ha::{EventTarget, EventWrite, HaStatus};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use super::{
    Msg,
    feed::FeedControl,
    ops::{self, code},
};
use crate::{
    api::AlertNotice,
    calendar::CalendarView,
    journal::{Action, AdvanceStep, OpRow, Settle, Settlement},
    ports::{AlertClock, AlertHa},
};

/// Work for the applier.
pub(crate) enum ApplierJob {
    /// A journaled operation.
    Op(OpRow),
    /// The restore guard: delete what an applied operation already handled. Not
    /// journaled; it repeats an applied operation.
    GuardDelete {
        entity: String,
        target: EventTarget,
        occurrence: Uuid,
    },
}

pub(crate) struct Applier<H, C> {
    pub device: echomuse_core::types::DeviceId,
    pub ha: Arc<H>,
    pub clock: Arc<C>,
    pub db: DbHandle,
    pub jobs: mpsc::UnboundedReceiver<ApplierJob>,
    pub views: watch::Receiver<Option<Arc<CalendarView>>>,
    pub status: watch::Receiver<HaStatus>,
    pub feed: mpsc::UnboundedSender<FeedControl>,
    pub actor: mpsc::UnboundedSender<Msg>,
    pub confirm_wait: Duration,
    pub retry: Duration,
}

/// How one attempt at an operation ended.
enum Step {
    /// Settle it.
    Done(Settlement),
    /// Try again once the calendar has been read afresh.
    Again,
    /// It cannot be applied by this controller; leave it pending.
    Stuck(String),
}

impl<H: AlertHa, C: AlertClock> Applier<H, C> {
    pub(crate) async fn run(mut self) {
        while let Some(job) = self.jobs.recv().await {
            match job {
                ApplierJob::Op(row) => self.apply(row).await,
                ApplierJob::GuardDelete {
                    entity,
                    target,
                    occurrence,
                } => {
                    self.fresh_view().await;
                    let _ = self.ha.delete_event(entity, target).await;
                    let _ = self
                        .actor
                        .send(Msg::GuardDone(self.device.clone(), occurrence));
                }
            }
        }
    }

    /// The current view, once HA is connected and the calendar has been read on that
    /// connection.
    async fn fresh_view(&mut self) -> Arc<CalendarView> {
        loop {
            let generation = self.status.borrow().generation;
            if let Some(generation) = generation {
                let view = self.views.borrow().clone();
                if let Some(view) = view.filter(|v| v.generation == generation) {
                    return view;
                }
            }
            tokio::select! {
                changed = self.status.changed() => if changed.is_err() { std::future::pending::<()>().await },
                changed = self.views.changed() => if changed.is_err() { std::future::pending::<()>().await },
            }
        }
    }

    /// Waits for a view after push `after` in which `holds` is true: up to the confirm
    /// wait, then once more after forcing a fresh read.
    async fn confirm(&mut self, after: u64, holds: impl Fn(&CalendarView) -> bool) -> bool {
        for forced in [false, true] {
            if forced {
                let _ = self.feed.send(FeedControl::Resubscribe);
            }
            let deadline = tokio::time::Instant::now() + self.confirm_wait;
            loop {
                let seen = self.views.borrow_and_update().clone();
                if let Some(view) = seen
                    && view.push > after
                    && holds(&view)
                {
                    return true;
                }
                match tokio::time::timeout_at(deadline, self.views.changed()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return false,
                    Err(_) => break,
                }
            }
        }
        false
    }

    /// Forces a fresh read and waits for it (after an unanswered or refused write).
    async fn reread(&mut self, after: u64) {
        let _ = self.feed.send(FeedControl::Resubscribe);
        let deadline = tokio::time::Instant::now() + self.confirm_wait;
        loop {
            if self.views.borrow().as_ref().is_some_and(|v| v.push > after) {
                return;
            }
            match tokio::time::timeout_at(deadline, self.views.changed()).await {
                Ok(Ok(())) => {}
                _ => return,
            }
        }
    }

    async fn apply(&mut self, row: OpRow) {
        let mut step = row.step;
        loop {
            let attempt = match row.action {
                Some(Action::Create) => self.create(&row).await,
                Some(Action::Cancel) => self.cancel(&row).await,
                Some(Action::Dismiss | Action::Expire) => self.delete_occurrence(&row).await,
                Some(Action::Snooze) => self.snooze(&row, &mut step).await,
                Some(Action::Update) | None => {
                    Step::Stuck("an action this controller does not write".to_owned())
                }
            };
            match attempt {
                Step::Done(settlement) => return self.settle(&row, settlement).await,
                Step::Again => {}
                Step::Stuck(detail) => {
                    let _ = self
                        .actor
                        .send(Msg::Notice(AlertNotice::UnreadableJournalRow {
                            seq: row.seq,
                            detail,
                        }));
                    return;
                }
            }
        }
    }

    async fn settle(&mut self, row: &OpRow, settlement: Settlement) {
        loop {
            let command = Settle {
                op_id: row.op_id,
                settlement: settlement.clone(),
            };
            match self.db.write(command).await {
                Ok(Some(settled)) => {
                    let _ = self.actor.send(Msg::Settled(self.device.clone(), settled));
                    return;
                }
                Ok(None) => return,
                Err(error) => {
                    let _ = self
                        .actor
                        .send(Msg::Notice(AlertNotice::DatabaseProblem(format!(
                            "settling {}: {error}",
                            row.op_id
                        ))));
                    tokio::time::sleep(self.retry).await;
                }
            }
        }
    }

    async fn advance(&mut self, row: &OpRow, step: i64) {
        let command = AdvanceStep {
            op_id: row.op_id,
            step,
        };
        if let Err(error) = self.db.write(command).await {
            // The step is a hint for recovery, which re-reads the calendar anyway.
            let _ = self
                .actor
                .send(Msg::Notice(AlertNotice::DatabaseProblem(format!(
                    "advancing {}: {error}",
                    row.op_id
                ))));
        }
    }

    fn applied(&self, result: Value) -> Step {
        Step::Done(Settlement::Applied {
            at: self.clock.now_utc_ms(),
            result,
        })
    }

    fn rejected(code: &str, detail: &str) -> Step {
        Step::Done(Settlement::Rejected {
            result: ops::rejected_result(code, detail),
        })
    }

    async fn create(&mut self, row: &OpRow) -> Step {
        let Some(event) = row
            .plan()
            .and_then(|p| p.get("event"))
            .and_then(ops::event_from)
        else {
            return Step::Stuck("create without a recorded event".to_owned());
        };
        let op = row.op_id.uuid();
        let view = self.fresh_view().await;
        if let Some(created) = view.created_by(op) {
            return self.applied(ops::created_result(created));
        }
        let before = view.push;
        match self
            .ha
            .create_event(row.calendar_entity.clone(), event)
            .await
        {
            EffectOutcome::Applied(()) => {
                self.advance(row, 1).await;
                if self.confirm(before, |v| v.created_by(op).is_some()).await {
                    let view = self.views.borrow().clone().expect("confirmed on a view");
                    let created = view.created_by(op).expect("confirmed");
                    self.applied(ops::created_result(created))
                } else {
                    Self::rejected(
                        code::NOT_CONFIRMED,
                        "the event never appeared on the calendar",
                    )
                }
            }
            EffectOutcome::Rejected { code: c, message } => {
                Self::rejected(code::HA_REFUSED, &format!("{c}: {message}"))
            }
            EffectOutcome::NotDispatched(_) => {
                self.wait_reconnect().await;
                Step::Again
            }
            EffectOutcome::Unknown(_) => {
                self.reread(before).await;
                Step::Again
            }
        }
    }

    async fn cancel(&mut self, row: &OpRow) -> Step {
        let Some(schedule) = row.schedule_id else {
            return Step::Stuck("cancel without a schedule".to_owned());
        };
        let view = self.fresh_view().await;
        let Some(uid) = view.schedule_uid(schedule).map(str::to_owned) else {
            return self.applied(ops::deleted_result(false));
        };
        let target = EventTarget {
            uid,
            recurrence_id: None,
        };
        self.delete(row, target, view.push, move |v| {
            v.schedule_uid(schedule).is_none()
        })
        .await
    }

    async fn delete_occurrence(&mut self, row: &OpRow) -> Step {
        let (Some(schedule), Some(key)) = (row.schedule_id, row.occurrence_key.clone()) else {
            return Step::Stuck("an occurrence operation without its occurrence".to_owned());
        };
        let view = self.fresh_view().await;
        if let Some(occurrence) = view.find(schedule, &key) {
            let target = ops::occurrence_target(occurrence);
            return self
                .delete(row, target, view.push, move |v| {
                    v.find(schedule, &key).is_none()
                })
                .await;
        }
        // Not in the subscription window. An occurrence due before the window (a missed
        // one from the backlog) is looked up and deleted through REST by its recorded
        // target; anything else is gone.
        let plan = row.plan();
        let recorded = plan
            .and_then(|p| p.get("target"))
            .and_then(ops::target_from);
        let due = plan
            .and_then(|p| p.get("due_utc_ms"))
            .and_then(Value::as_i64);
        let (Some(target), Some(due)) = (recorded, due) else {
            return self.applied(ops::deleted_result(false));
        };
        if due >= view.window_start_ms {
            return self.applied(ops::deleted_result(false));
        }
        match self.present_before_window(row, &target, due).await {
            None => {
                self.wait_reconnect().await;
                return Step::Again;
            }
            Some(false) => return self.applied(ops::deleted_result(false)),
            Some(true) => {}
        }
        match self
            .ha
            .delete_event(row.calendar_entity.clone(), target.clone())
            .await
        {
            EffectOutcome::Applied(()) => self.applied(ops::deleted_result(true)),
            EffectOutcome::Rejected { code: c, message } => {
                match self.present_before_window(row, &target, due).await {
                    Some(false) => self.applied(ops::deleted_result(false)),
                    _ => Self::rejected(code::HA_REFUSED, &format!("{c}: {message}")),
                }
            }
            EffectOutcome::NotDispatched(_) => {
                self.wait_reconnect().await;
                Step::Again
            }
            // The next attempt looks the instance up again before deleting it.
            EffectOutcome::Unknown(_) => Step::Again,
        }
    }

    /// Whether the instance `target` due at `due` is still on the calendar, read through
    /// REST around its due time; `None` when HA could not be read.
    async fn present_before_window(
        &mut self,
        row: &OpRow,
        target: &EventTarget,
        due: i64,
    ) -> Option<bool> {
        let around = |ms: i64| {
            jiff::Timestamp::from_millisecond(ms)
                .map(|ts| ts.to_string())
                .ok()
        };
        let (start, end) = (around(due - 60_000)?, around(due + 60_000)?);
        let items = self
            .ha
            .calendar_events(row.calendar_entity.clone(), start, end)
            .await
            .ok()?;
        Some(items.iter().any(|item| {
            item.uid.as_deref() == Some(target.uid.as_str())
                && item.recurrence_id == target.recurrence_id
        }))
    }

    async fn delete(
        &mut self,
        row: &OpRow,
        target: EventTarget,
        before: u64,
        gone: impl Fn(&CalendarView) -> bool,
    ) -> Step {
        match self
            .ha
            .delete_event(row.calendar_entity.clone(), target)
            .await
        {
            EffectOutcome::Applied(()) => {
                if self.confirm(before, &gone).await {
                    self.applied(ops::deleted_result(true))
                } else {
                    Self::rejected(code::NOT_CONFIRMED, "it is still on the calendar")
                }
            }
            EffectOutcome::Rejected { code: c, message } => {
                // Not found counts as done: read afresh and look.
                self.reread(before).await;
                let view = self.views.borrow().clone();
                if view.is_some_and(|v| v.push > before && gone(&v)) {
                    self.applied(ops::deleted_result(false))
                } else {
                    Self::rejected(code::HA_REFUSED, &format!("{c}: {message}"))
                }
            }
            EffectOutcome::NotDispatched(_) => {
                self.wait_reconnect().await;
                Step::Again
            }
            EffectOutcome::Unknown(_) => {
                self.reread(before).await;
                Step::Again
            }
        }
    }

    /// A snooze: step 0 creates the child, carrying this operation's id; step 1 deletes
    /// the parent occurrence. Adopted only while the parent's schedule exists.
    async fn snooze(&mut self, row: &OpRow, step: &mut i64) -> Step {
        let (Some(schedule), Some(key)) = (row.schedule_id, row.occurrence_key.clone()) else {
            return Step::Stuck("a snooze without its occurrence".to_owned());
        };
        let Some(child) = row
            .plan()
            .and_then(|p| p.get("child"))
            .and_then(ops::event_from)
        else {
            return Step::Stuck("a snooze without its recorded child".to_owned());
        };
        let op = row.op_id.uuid();
        let view = self.fresh_view().await;
        if *step == 0 {
            if view.created_by(op).is_none() {
                if view.schedule_uid(schedule).is_none() {
                    return Self::rejected(code::PARENT_GONE, "the snoozed alarm no longer exists");
                }
                let before = view.push;
                if let Some(step) = self.create_child(row, child, before, op).await {
                    return step;
                }
            }
            *step = 1;
            self.advance(row, 1).await;
        }
        let view = self.fresh_view().await;
        let child_result = |v: &CalendarView| {
            v.created_by(op).map(|c| {
                serde_json::json!({
                    "ok": true,
                    "child_schedule_id": c.schedule_id.to_string(),
                    "child_occurrence_id": c.occurrence_id.to_string(),
                    "due_local": c.due_local,
                    "due_utc_ms": c.due_utc_ms.to_string(),
                })
            })
        };
        let Some(parent) = view.find(schedule, &key) else {
            return self.applied(child_result(&view).unwrap_or_else(|| ops::deleted_result(false)));
        };
        let target = ops::occurrence_target(parent);
        match self
            .delete(row, target, view.push, move |v| {
                v.find(schedule, &key).is_none()
            })
            .await
        {
            Step::Done(Settlement::Applied { at, .. }) => {
                let view = self.views.borrow().clone().expect("confirmed on a view");
                Step::Done(Settlement::Applied {
                    at,
                    result: child_result(&view).unwrap_or_else(|| ops::deleted_result(true)),
                })
            }
            other => other,
        }
    }

    /// Creates a snooze child; `None` once it is on the calendar.
    async fn create_child(
        &mut self,
        row: &OpRow,
        child: EventWrite,
        before: u64,
        op: Uuid,
    ) -> Option<Step> {
        match self
            .ha
            .create_event(row.calendar_entity.clone(), child)
            .await
        {
            EffectOutcome::Applied(()) => {
                if self.confirm(before, |v| v.created_by(op).is_some()).await {
                    None
                } else {
                    Some(Self::rejected(
                        code::NOT_CONFIRMED,
                        "the snooze never appeared on the calendar",
                    ))
                }
            }
            EffectOutcome::Rejected { code: c, message } => {
                Some(Self::rejected(code::HA_REFUSED, &format!("{c}: {message}")))
            }
            EffectOutcome::NotDispatched(_) => {
                self.wait_reconnect().await;
                Some(Step::Again)
            }
            EffectOutcome::Unknown(_) => {
                self.reread(before).await;
                Some(Step::Again)
            }
        }
    }

    /// After a write that was never sent: wait for the connection to change or come
    /// back, at most the retry interval.
    async fn wait_reconnect(&mut self) {
        let _ = tokio::time::timeout(self.retry, self.status.changed()).await;
    }
}
