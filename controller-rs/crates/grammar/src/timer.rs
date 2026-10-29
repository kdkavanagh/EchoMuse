//! Cancelling timers on this speaker (§4.4, architecture §10.8).
//!
//! Home Assistant owns every timer, and its own timer intents answer every timer request
//! but two: "cancel the timer" and "cancel all timers", whose answers from HA cannot say
//! which timer went. Those two are decided here, against HA's `HassTimerStatus` list, and
//! carried out through HA's intent API:
//!
//! - **This speaker's timers** are the ones HA lists under the satellite's HA device. HA's
//!   status lists every timer in the home.
//! - **One timer** is cancelled with one `HassCancelTimer` whose slots (its name and the
//!   start units it was given) pick exactly that timer by HA's own search: the name, then
//!   the start units, then the asking device. When they cannot (two of this speaker's timers
//!   share a length and a name), nothing is sent and the speaker says so.
//! - **All of them**: when every timer in HA is this speaker's, one `HassCancelAllTimers`
//!   (it cancels every timer in HA). Otherwise one `HassCancelTimer` per timer, skipping
//!   those HA could not pick out; the answer counts what was not cancelled.
//!
//! A cancel that names a timer by name or length goes to HA's own intent handling, which
//! matches names and lengths itself.

use std::collections::VecDeque;

use echomuse_core::services::{DirectIntent, HaDeviceId, HaTimer, TimerSelector};

use crate::normalize::normalize;

/// Which timers a local cancel names.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TimerCancel {
    /// `cancel the timer`: the only one, or the controller asks "Which one?".
    Any,
    /// `cancel all timers`: every timer of this speaker.
    All,
}

/// What the answer to "Which one?" picked among the timers it offered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChosenTimers {
    /// One timer.
    One(HaTimer),
    /// Every timer offered.
    All(Vec<HaTimer>),
}

/// Every timer HA reported, and the speaker's HA device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpeakerTimers {
    every: Vec<HaTimer>,
    device: HaDeviceId,
}

/// What to do about a timer cancel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TimerCancelPlan {
    /// The speaker has no timers (of those asked about).
    NoTimers,
    /// Several run: ask "Which one?" and choose with [`crate::WhichReply::choose_timer`].
    WhichOne(Vec<HaTimer>),
    /// The timer chosen in answer to "Which one?" is gone.
    AlreadyFinished(HaTimer),
    /// HA cannot pick this timer out: `count` of the speaker's timers share its length and
    /// name. Nothing is sent.
    Indistinct {
        /// The timer.
        timer: HaTimer,
        /// How many of the speaker's timers it cannot be told apart from (itself
        /// included).
        count: usize,
    },
    /// Send these requests one after another.
    Cancel(TimerCancelRun),
}

impl SpeakerTimers {
    /// `every` is HA's `HassTimerStatus` list; `device` the satellite's HA device.
    pub fn new(every: Vec<HaTimer>, device: HaDeviceId) -> Self {
        Self { every, device }
    }

    /// This speaker's timers, in HA's order.
    pub fn mine(&self) -> Vec<HaTimer> {
        self.every
            .iter()
            .filter(|t| t.ha_device.as_ref() == Some(&self.device))
            .cloned()
            .collect()
    }

    /// Decides a cancel just asked for.
    pub fn plan(&self, cancel: TimerCancel) -> TimerCancelPlan {
        let mine = self.mine();
        match (cancel, mine.as_slice()) {
            (TimerCancel::All, _) => self.cancel_set(mine),
            (TimerCancel::Any, []) => TimerCancelPlan::NoTimers,
            (TimerCancel::Any, [one]) => self.cancel_one(one.clone()),
            (TimerCancel::Any, _) => TimerCancelPlan::WhichOne(mine),
        }
    }

    /// Decides the answer to "Which one?" against the timers as they are now: a timer
    /// that finished meanwhile is not cancelled.
    pub fn plan_chosen(&self, chosen: &ChosenTimers) -> TimerCancelPlan {
        let mine = self.mine();
        match chosen {
            ChosenTimers::One(timer) => match mine.into_iter().find(|t| t.id == timer.id) {
                Some(now) => self.cancel_one(now),
                None => TimerCancelPlan::AlreadyFinished(timer.clone()),
            },
            ChosenTimers::All(offered) => self.cancel_set(
                mine.into_iter()
                    .filter(|t| offered.iter().any(|o| o.id == t.id))
                    .collect(),
            ),
        }
    }

    fn cancel_one(&self, timer: HaTimer) -> TimerCancelPlan {
        match pick(&timer, &self.every, &self.device) {
            Some(selector) => TimerCancelPlan::Cancel(TimerCancelRun {
                every: self.every.clone(),
                device: self.device.clone(),
                queue: VecDeque::new(),
                sent: Some((DirectIntent::CancelTimer(selector), vec![timer])),
                cancelled: Vec::new(),
                total: 1,
            }),
            None => {
                let count = self
                    .mine()
                    .iter()
                    .filter(|t| {
                        timer_seconds(t) == timer_seconds(&timer)
                            && timer
                                .name
                                .as_deref()
                                .is_none_or(|name| same_name(t.name.as_deref(), name))
                    })
                    .count();
                TimerCancelPlan::Indistinct { timer, count }
            }
        }
    }

    fn cancel_set(&self, timers: Vec<HaTimer>) -> TimerCancelPlan {
        if timers.is_empty() {
            return TimerCancelPlan::NoTimers;
        }
        let total = timers.len();
        let all_at_once = total == self.every.len();
        TimerCancelPlan::Cancel(TimerCancelRun {
            every: self.every.clone(),
            device: self.device.clone(),
            sent: all_at_once.then(|| (DirectIntent::CancelAllTimers, timers.clone())),
            queue: if all_at_once {
                VecDeque::new()
            } else {
                timers.into()
            },
            cancelled: Vec::new(),
            total,
        })
    }
}

/// Timer cancels carried out one request at a time: [`TimerCancelRun::next_intent`], then
/// HA's answer to it through [`TimerCancelRun::answered`], until it gives no more.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimerCancelRun {
    every: Vec<HaTimer>,
    device: HaDeviceId,
    queue: VecDeque<HaTimer>,
    /// The request to send next (or waiting for its answer), and the timers it cancels.
    sent: Option<(DirectIntent, Vec<HaTimer>)>,
    cancelled: Vec<HaTimer>,
    total: usize,
}

impl TimerCancelRun {
    /// The next request to send; `None` once every timer was tried. A timer HA could not
    /// pick out (it shares a length and a name with another of the speaker's) is skipped
    /// and counts as not cancelled.
    pub fn next_intent(&mut self) -> Option<DirectIntent> {
        if let Some((intent, _)) = &self.sent {
            return Some(intent.clone());
        }
        while let Some(timer) = self.queue.pop_front() {
            if let Some(selector) = pick(&timer, &self.every, &self.device) {
                let intent = DirectIntent::CancelTimer(selector);
                self.sent = Some((intent.clone(), vec![timer]));
                return Some(intent);
            }
        }
        None
    }

    /// HA answered the request [`TimerCancelRun::next_intent`] gave: `applied` when it
    /// cancelled, false when it refused or the request was never sent.
    pub fn answered(&mut self, applied: bool) {
        let Some((_, timers)) = self.sent.take() else {
            return;
        };
        if applied {
            self.every
                .retain(|t| timers.iter().all(|done| done.id != t.id));
            self.cancelled.extend(timers);
        }
    }

    /// The timers HA confirmed cancelled so far.
    pub fn cancelled(&self) -> &[HaTimer] {
        &self.cancelled
    }

    /// How many timers the cancel was for.
    pub fn total(&self) -> usize {
        self.total
    }
}

/// The `HassCancelTimer` slots under which HA's own search (`intent/timers.py`
/// `_find_timer`: by name, then by the start units, then the asking device, each step
/// returning a lone match) finds exactly `target` among `every` timer in HA; `None` when it
/// cannot. HA reports a unit a timer was not given as 0 and compares only the units it
/// stored, so zero units are left out.
fn pick(target: &HaTimer, every: &[HaTimer], device: &HaDeviceId) -> Option<TimerSelector> {
    let mut left: Vec<&HaTimer> = every
        .iter()
        .filter(|t| {
            target
                .name
                .as_deref()
                .is_none_or(|name| same_name(t.name.as_deref(), name))
        })
        .collect();
    if left.len() > 1 {
        left.retain(|t| start_units(t) == start_units(target));
    }
    if left.len() > 1 {
        left.retain(|t| t.ha_device.as_ref() == Some(device));
    }
    if !matches!(left.as_slice(), [only] if only.id == target.id) {
        return None;
    }
    let unit = |n: u32| (n != 0).then_some(n);
    Some(TimerSelector {
        name: target.name.clone(),
        start_hours: unit(target.start_hours),
        start_minutes: unit(target.start_minutes),
        start_seconds: unit(target.start_seconds),
    })
}

fn start_units(timer: &HaTimer) -> (u32, u32, u32) {
    (timer.start_hours, timer.start_minutes, timer.start_seconds)
}

fn same_name(name: Option<&str>, wanted: &str) -> bool {
    name.is_some_and(|name| normalize(name) == normalize(wanted))
}

/// A timer's length in seconds, from its start units.
pub(crate) fn timer_seconds(timer: &HaTimer) -> u32 {
    timer.start_hours * 3600 + timer.start_minutes * 60 + timer.start_seconds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn here() -> HaDeviceId {
        HaDeviceId("kitchen".to_owned())
    }

    fn timer(id: &str, name: Option<&str>, minutes: u32, device: &str) -> HaTimer {
        HaTimer {
            id: id.to_owned(),
            name: name.map(str::to_owned),
            ha_device: Some(HaDeviceId(device.to_owned())),
            start_hours: 0,
            start_minutes: minutes,
            start_seconds: 0,
            is_active: true,
        }
    }

    fn minutes(n: u32) -> TimerSelector {
        TimerSelector {
            start_minutes: Some(n),
            ..TimerSelector::default()
        }
    }

    /// Runs a plan's requests, each answered `applied`; the intents sent and the run.
    fn run(plan: TimerCancelPlan, applied: bool) -> (Vec<DirectIntent>, TimerCancelRun) {
        let TimerCancelPlan::Cancel(mut run) = plan else {
            panic!("not a cancel: {plan:?}");
        };
        let mut sent = Vec::new();
        while let Some(intent) = run.next_intent() {
            sent.push(intent);
            run.answered(applied);
        }
        (sent, run)
    }

    #[test]
    fn cancel_the_timer_with_none_one_or_several() {
        let other = timer("o", None, 5, "office");
        let list = |timers: Vec<HaTimer>| SpeakerTimers::new(timers, here());
        // Another speaker's timer is not this speaker's.
        assert_eq!(
            list(vec![other.clone()]).plan(TimerCancel::Any),
            TimerCancelPlan::NoTimers
        );
        let ten = timer("a", None, 10, "kitchen");
        let (sent, done) = run(
            list(vec![other.clone(), ten.clone()]).plan(TimerCancel::Any),
            true,
        );
        assert_eq!(sent, vec![DirectIntent::CancelTimer(minutes(10))]);
        assert_eq!(done.cancelled(), std::slice::from_ref(&ten));
        assert_eq!(done.total(), 1);
        let five = timer("b", None, 5, "kitchen");
        assert_eq!(
            list(vec![ten.clone(), other, five.clone()]).plan(TimerCancel::Any),
            TimerCancelPlan::WhichOne(vec![ten, five])
        );
    }

    #[test]
    fn a_refused_cancel_cancels_nothing() {
        let ten = timer("a", None, 10, "kitchen");
        let (_, done) = run(
            SpeakerTimers::new(vec![ten], here()).plan(TimerCancel::Any),
            false,
        );
        assert!(done.cancelled().is_empty());
        assert_eq!(done.total(), 1);
    }

    #[test]
    fn the_slots_pick_the_timer_by_name_then_length_then_device() {
        let pasta = timer("a", Some("Pasta"), 10, "kitchen");
        let other_pasta = timer("b", Some("pasta"), 5, "office");
        let selector = pick(&pasta, &[pasta.clone(), other_pasta.clone()], &here());
        assert_eq!(
            selector,
            Some(TimerSelector {
                name: Some("Pasta".to_owned()),
                start_minutes: Some(10),
                ..TimerSelector::default()
            })
        );
        // Same name and length on another speaker: the device tells them apart.
        let twin_elsewhere = timer("c", Some("pasta"), 10, "office");
        assert!(pick(&pasta, &[pasta.clone(), twin_elsewhere], &here()).is_some());
        // Same name and length on this speaker: HA cannot pick one.
        let twin_here = timer("d", Some("pasta"), 10, "kitchen");
        assert_eq!(pick(&pasta, &[pasta.clone(), twin_here], &here()), None);
    }

    #[test]
    fn two_timers_alike_cannot_be_cancelled_one_by_one() {
        let a = timer("a", None, 10, "kitchen");
        let b = timer("b", None, 10, "kitchen");
        let list = SpeakerTimers::new(vec![a.clone(), b.clone()], here());
        assert_eq!(
            list.plan_chosen(&ChosenTimers::One(a.clone())),
            TimerCancelPlan::Indistinct { timer: a, count: 2 }
        );
        // All of them: every timer in HA is this speaker's, so one HassCancelAllTimers.
        let (sent, done) = run(list.plan(TimerCancel::All), true);
        assert_eq!(sent, vec![DirectIntent::CancelAllTimers]);
        assert_eq!(done.cancelled().len(), 2);
    }

    #[test]
    fn all_timers_beside_another_speakers_go_one_at_a_time() {
        let a = timer("a", None, 10, "kitchen");
        let b = timer("b", None, 10, "kitchen");
        let c = timer("c", Some("tea"), 3, "kitchen");
        let other = timer("o", None, 5, "office");
        let list = SpeakerTimers::new(vec![a, b, c.clone(), other], here());
        let (sent, done) = run(list.plan(TimerCancel::All), true);
        // The two alike are skipped; the tea timer goes.
        assert_eq!(
            sent,
            vec![DirectIntent::CancelTimer(TimerSelector {
                name: Some("tea".to_owned()),
                start_minutes: Some(3),
                ..TimerSelector::default()
            })]
        );
        assert_eq!(done.cancelled(), [c]);
        assert_eq!(done.total(), 3);
        assert_eq!(
            SpeakerTimers::new(Vec::new(), here()).plan(TimerCancel::All),
            TimerCancelPlan::NoTimers
        );
    }

    #[test]
    fn a_chosen_timer_that_finished_is_not_cancelled() {
        let a = timer("a", None, 10, "kitchen");
        let b = timer("b", None, 5, "kitchen");
        let now = SpeakerTimers::new(vec![b.clone()], here());
        assert_eq!(
            now.plan_chosen(&ChosenTimers::One(a.clone())),
            TimerCancelPlan::AlreadyFinished(a.clone())
        );
        // All of the offered: only those still running, as the speaker's whole list.
        let (sent, done) = run(
            now.plan_chosen(&ChosenTimers::All(vec![a, b.clone()])),
            true,
        );
        assert_eq!(sent, vec![DirectIntent::CancelAllTimers]);
        assert_eq!(done.cancelled(), [b]);
        assert_eq!(done.total(), 1);
    }
}
