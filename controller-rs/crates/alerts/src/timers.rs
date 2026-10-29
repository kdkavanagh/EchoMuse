//! Display copies of a speaker's Home Assistant timers (§4.9, architecture §10.8).
//!
//! HA owns every timer and reports `started`, `updated`, `cancelled` and `finished` over
//! the ESPHome API. The copy here ticks down for `timer.view` and the dashboard; it never
//! decides that a timer finished. When a copy reaches zero and no `finished` arrives
//! within a grace period, the timer is dropped and reported as `timer_finish_missed`.

use std::collections::BTreeMap;

use tokio::time::{Duration, Instant};

use crate::wire::{TimerView, TimerViewEntry};

/// What HA reported about a timer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerEventKind {
    /// It started.
    Started,
    /// It changed (paused, resumed, or time added or removed).
    Updated,
    /// It was cancelled.
    Cancelled,
    /// It finished; the speaker rings.
    Finished,
}

/// One timer event of a speaker (ESPHome `VoiceAssistantTimerEventResponse`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimerEvent {
    /// What happened.
    pub kind: TimerEventKind,
    /// HA's timer id.
    pub timer_id: String,
    /// HA's name; `None` for an unnamed timer.
    pub name: Option<String>,
    /// The timer's whole length in seconds.
    pub total_seconds: u32,
    /// The time left in seconds when HA sent the event.
    pub seconds_left: u32,
    /// False while paused.
    pub is_active: bool,
}

#[derive(Clone, Debug)]
struct TimerCopy {
    name: Option<String>,
    total_ms: u64,
    /// Time left at `anchor`.
    left_ms: u64,
    anchor: Instant,
    active: bool,
}

impl TimerCopy {
    fn remaining(&self, now: Instant) -> u64 {
        if !self.active {
            return self.left_ms;
        }
        let elapsed = u64::try_from(now.saturating_duration_since(self.anchor).as_millis())
            .unwrap_or(u64::MAX);
        self.left_ms.saturating_sub(elapsed)
    }

    /// When an active copy reaches zero.
    fn zero_at(&self) -> Option<Instant> {
        self.active
            .then(|| self.anchor + Duration::from_millis(self.left_ms))
    }
}

/// A timer whose `finished` never arrived.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissedFinish {
    /// HA's timer id.
    pub timer_id: String,
    /// Its name.
    pub name: Option<String>,
}

/// A speaker's timer copies.
#[derive(Clone, Debug, Default)]
pub struct Timers {
    copies: BTreeMap<String, TimerCopy>,
}

impl Timers {
    /// Applies an event at `now`. A finished or cancelled timer leaves the list.
    pub fn apply(&mut self, event: &TimerEvent, now: Instant) {
        match event.kind {
            TimerEventKind::Started | TimerEventKind::Updated => {
                self.copies.insert(
                    event.timer_id.clone(),
                    TimerCopy {
                        name: event.name.clone().filter(|n| !n.is_empty()),
                        total_ms: u64::from(event.total_seconds) * 1_000,
                        left_ms: u64::from(event.seconds_left) * 1_000,
                        anchor: now,
                        active: event.is_active,
                    },
                );
            }
            TimerEventKind::Cancelled | TimerEventKind::Finished => {
                self.copies.remove(&event.timer_id);
            }
        }
    }

    /// Whether the speaker has any timer.
    pub fn is_empty(&self) -> bool {
        self.copies.is_empty()
    }

    /// Drops copies that reached zero more than `grace` ago without a `finished`.
    pub fn sweep(&mut self, now: Instant, grace: Duration) -> Vec<MissedFinish> {
        let missed: Vec<String> = self
            .copies
            .iter()
            .filter(|(_, copy)| copy.zero_at().is_some_and(|zero| now >= zero + grace))
            .map(|(id, _)| id.clone())
            .collect();
        missed
            .into_iter()
            .filter_map(|id| {
                self.copies.remove(&id).map(|copy| MissedFinish {
                    timer_id: id,
                    name: copy.name,
                })
            })
            .collect()
    }

    /// `timer.view` at `now`: active timers soonest first, then paused ones by time left.
    pub fn view(&self, now: Instant) -> TimerView {
        let mut timers: Vec<TimerViewEntry> = self
            .copies
            .iter()
            .map(|(id, copy)| TimerViewEntry {
                id: id.clone(),
                name: copy.name.clone(),
                total_ms: copy.total_ms,
                remaining_ms: copy.remaining(now),
                active: copy.active,
            })
            .collect();
        timers.sort_by(|a, b| {
            (!a.active, a.remaining_ms, &a.id).cmp(&(!b.active, b.remaining_ms, &b.id))
        });
        TimerView { timers }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: TimerEventKind, id: &str, left: u32, active: bool) -> TimerEvent {
        TimerEvent {
            kind,
            timer_id: id.to_owned(),
            name: (id == "pasta").then(|| "pasta".to_owned()),
            total_seconds: 300,
            seconds_left: left,
            is_active: active,
        }
    }

    #[test]
    fn copies_tick_down_and_order_active_before_paused() {
        let start = Instant::now();
        let mut timers = Timers::default();
        timers.apply(&event(TimerEventKind::Started, "pasta", 300, true), start);
        timers.apply(&event(TimerEventKind::Started, "eggs", 60, true), start);
        timers.apply(&event(TimerEventKind::Updated, "tea", 30, false), start);
        let view = timers.view(start + Duration::from_secs(10));
        let order: Vec<(&str, u64)> = view
            .timers
            .iter()
            .map(|t| (t.id.as_str(), t.remaining_ms))
            .collect();
        assert_eq!(
            order,
            [("eggs", 50_000), ("pasta", 290_000), ("tea", 30_000)]
        );
        assert_eq!(view.timers[1].name.as_deref(), Some("pasta"));
        assert_eq!(view.timers[0].name, None);

        timers.apply(&event(TimerEventKind::Finished, "eggs", 0, true), start);
        assert_eq!(timers.view(start).timers.len(), 2);
    }

    #[test]
    fn a_copy_at_zero_without_a_finish_is_dropped_after_the_grace() {
        let start = Instant::now();
        let mut timers = Timers::default();
        timers.apply(&event(TimerEventKind::Started, "pasta", 60, true), start);
        timers.apply(&event(TimerEventKind::Started, "tea", 1, false), start);
        let grace = Duration::from_secs(5);
        assert!(
            timers
                .sweep(start + Duration::from_secs(64), grace)
                .is_empty()
        );
        assert_eq!(
            timers.sweep(start + Duration::from_secs(65), grace),
            [MissedFinish {
                timer_id: "pasta".to_owned(),
                name: Some("pasta".to_owned())
            }]
        );
        // A paused timer never reaches zero.
        assert!(
            timers
                .sweep(start + Duration::from_secs(3_600), grace)
                .is_empty()
        );
        assert!(!timers.is_empty());
    }
}
