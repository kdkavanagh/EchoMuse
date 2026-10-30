//! The bounded queue between a device runtime and its recording's writer thread.
//!
//! The runtime only ever pushes, under a lock held for a few instructions; it never waits for
//! the disk or for room. When the queue is full the new entry is dropped and counted, and the
//! recording is marked incomplete. Privacy and lifecycle markers bypass the bound, so a full
//! queue can never hide a mute or an end. Mute and withdrawal remove unwritten entries from
//! the queue, and the writer drops any audio it took before a mute but had not yet written.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use bytes::Bytes;
use echomuse_turn::MonoInstant;

use crate::grant::lock;
use crate::record::{EndReason, Event, IngressOrdinal, RelTime, Tally};
use crate::writer::RecorderState;

/// How much a recording's write queue may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueBounds {
    /// Entries.
    pub max_entries: usize,
    /// Bytes: a frame counts its own length, any other entry a nominal 1 KiB.
    pub max_bytes: usize,
}

impl Default for QueueBounds {
    /// 4,096 entries and 8 MiB: several seconds of every uplink stream at once, far more than
    /// a healthy writer ever has outstanding.
    fn default() -> Self {
        Self {
            max_entries: 4_096,
            max_bytes: 8 * 1024 * 1024,
        }
    }
}

/// What a queued entry holds.
#[derive(Debug)]
pub(crate) enum Body {
    Frame {
        ingress: IngressOrdinal,
        frame: Bytes,
    },
    Event(Box<Event>),
}

#[derive(Debug)]
pub(crate) struct Item {
    pub(crate) seq: u64,
    pub(crate) t: RelTime,
    pub(crate) body: Body,
    /// The privacy epoch the entry was taken in (frames only matter).
    pub(crate) privacy_epoch: u64,
    cost: usize,
}

impl Item {
    pub(crate) fn new(seq: u64, t: RelTime, body: Body, privacy_epoch: u64) -> Self {
        let cost = match &body {
            Body::Frame { frame, .. } => frame.len(),
            Body::Event(_) => Event::QUEUE_COST,
        };
        Self {
            seq,
            t,
            body,
            privacy_epoch,
            cost,
        }
    }

    fn is_frame(&self) -> bool {
        matches!(self.body, Body::Frame { .. })
    }
}

/// What to discard when closing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purge {
    Nothing,
    Everything,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Closing {
    pub(crate) reason: EndReason,
    pub(crate) t: RelTime,
}

/// The queue's state.
#[derive(Debug)]
struct State {
    items: VecDeque<Item>,
    bytes: usize,
    closing: Option<Closing>,
    /// The store deleted the recording: the writer removes its file and stops.
    aborted: bool,
    /// The writer failed; nothing more is taken.
    failed: Option<String>,
    /// The writer has finished.
    done: bool,
    tally: Tally,
    /// Lost and discarded counts not yet noted in the file.
    unnoted_lost: u64,
    unnoted_frames: u64,
    unnoted_events: u64,
    last_seq: Option<u64>,
    #[cfg(test)]
    held: bool,
}

/// Whether a push was taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pushed {
    Queued,
    Lost,
    Closed,
}

/// One batch for the writer.
#[derive(Debug)]
pub(crate) struct Batch {
    pub(crate) items: Vec<Item>,
    pub(crate) lost: u64,
    pub(crate) discarded_frames: u64,
    pub(crate) discarded_events: u64,
    /// Set once the queue is closed and this batch drains it.
    pub(crate) closing: Option<Closing>,
    pub(crate) aborted: bool,
}

#[derive(Debug)]
pub(crate) struct Queue {
    state: Mutex<State>,
    wake: Condvar,
    bounds: QueueBounds,
    /// Bumped on every mute: frames taken in an older epoch are not written.
    privacy_epoch: AtomicU64,
    pub(crate) origin: MonoInstant,
}

impl Queue {
    pub(crate) fn new(bounds: QueueBounds, origin: MonoInstant) -> Self {
        Self {
            state: Mutex::new(State {
                items: VecDeque::new(),
                bytes: 0,
                closing: None,
                aborted: false,
                failed: None,
                done: false,
                tally: Tally::default(),
                unnoted_lost: 0,
                unnoted_frames: 0,
                unnoted_events: 0,
                last_seq: None,
                #[cfg(test)]
                held: false,
            }),
            wake: Condvar::new(),
            bounds,
            privacy_epoch: AtomicU64::new(0),
            origin,
        }
    }

    pub(crate) fn privacy_epoch(&self) -> u64 {
        self.privacy_epoch.load(Ordering::Acquire)
    }

    /// Queues an entry. A `guaranteed` entry is queued even when the queue is full.
    pub(crate) fn push(&self, item: Item, guaranteed: bool) -> Pushed {
        let mut s = lock(&self.state);
        if s.closing.is_some() || s.failed.is_some() {
            return Pushed::Closed;
        }
        s.last_seq = Some(item.seq);
        let full =
            s.items.len() >= self.bounds.max_entries || s.bytes + item.cost > self.bounds.max_bytes;
        if full && !guaranteed {
            s.tally.lost_entries += 1;
            s.unnoted_lost += 1;
            return Pushed::Lost;
        }
        s.bytes += item.cost;
        s.items.push_back(item);
        drop(s);
        self.wake.notify_one();
        Pushed::Queued
    }

    /// Adds counts the runtime keeps (withheld frames, redactions) to the totals.
    pub(crate) fn count(&self, add: Tally) {
        lock(&self.state).tally.add(&add);
    }

    fn discard(s: &mut State, frames_only: bool) {
        let before = s.items.len();
        let mut frames = 0;
        let mut events = 0;
        s.items.retain(|i| {
            let drop = i.is_frame() || !frames_only;
            if drop {
                if i.is_frame() {
                    frames += 1;
                } else {
                    events += 1;
                }
            }
            !drop
        });
        if s.items.len() != before {
            s.bytes = s.items.iter().map(|i| i.cost).sum();
        }
        s.tally.discarded_frames += frames;
        s.tally.discarded_events += events;
        s.unnoted_frames += frames;
        s.unnoted_events += events;
    }

    /// The device muted (or capture stopped being permitted): unwritten audio is discarded,
    /// in the queue and in the writer's hands.
    pub(crate) fn mute(&self) {
        let mut s = lock(&self.state);
        self.privacy_epoch.fetch_add(1, Ordering::AcqRel);
        Self::discard(&mut s, true);
    }

    /// The writer dropped a frame taken before a mute.
    pub(crate) fn discarded_by_writer(&self) {
        let mut s = lock(&self.state);
        s.tally.discarded_frames += 1;
        s.unnoted_frames += 1;
    }

    /// Ends the recording. The first reason wins.
    pub(crate) fn close(&self, reason: EndReason, purge: Purge, now: MonoInstant) {
        let mut s = lock(&self.state);
        if s.closing.is_some() {
            return;
        }
        if purge == Purge::Everything {
            self.privacy_epoch.fetch_add(1, Ordering::AcqRel);
            Self::discard(&mut s, false);
        }
        s.closing = Some(Closing {
            reason,
            t: RelTime::between(self.origin, now),
        });
        drop(s);
        self.wake.notify_one();
    }

    /// The store deleted the recording.
    pub(crate) fn abort(&self) {
        let mut s = lock(&self.state);
        s.aborted = true;
        s.items.clear();
        s.bytes = 0;
        drop(s);
        self.wake.notify_one();
    }

    pub(crate) fn is_aborted(&self) -> bool {
        lock(&self.state).aborted
    }

    pub(crate) fn fail(&self, error: String) {
        let mut s = lock(&self.state);
        s.failed = Some(error);
        s.items.clear();
        s.bytes = 0;
    }

    pub(crate) fn finished(&self) {
        lock(&self.state).done = true;
    }

    /// Whether entries are still being taken.
    pub(crate) fn is_open(&self) -> bool {
        let s = lock(&self.state);
        s.closing.is_none() && s.failed.is_none() && !s.aborted
    }

    /// The recording's state, for the runtime and the dashboard.
    pub(crate) fn status(&self) -> RecorderState {
        let s = lock(&self.state);
        if let Some(e) = &s.failed {
            RecorderState::Failed(e.clone())
        } else if s.aborted {
            RecorderState::Deleted
        } else if let Some(c) = s.closing {
            RecorderState::Ended {
                reason: c.reason,
                written: s.done,
            }
        } else {
            RecorderState::Recording {
                incomplete: s.tally.lost_entries > 0 || s.tally.discarded_events > 0,
            }
        }
    }

    /// Waits for work and takes all of it.
    pub(crate) fn take(&self) -> Batch {
        let mut s = lock(&self.state);
        loop {
            #[cfg(test)]
            let held = s.held;
            #[cfg(not(test))]
            let held = false;
            let ready = s.aborted
                || (!held
                    && (!s.items.is_empty()
                        || s.closing.is_some()
                        || s.unnoted_lost + s.unnoted_frames + s.unnoted_events > 0));
            if ready {
                break;
            }
            s = self
                .wake
                .wait(s)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        let items: Vec<Item> = s.items.drain(..).collect();
        s.bytes = 0;
        Batch {
            items,
            lost: std::mem::take(&mut s.unnoted_lost),
            discarded_frames: std::mem::take(&mut s.unnoted_frames),
            discarded_events: std::mem::take(&mut s.unnoted_events),
            closing: s.closing,
            aborted: s.aborted,
        }
    }

    /// The totals and the last sequence number assigned.
    pub(crate) fn totals(&self) -> (Tally, Option<u64>) {
        let s = lock(&self.state);
        (s.tally, s.last_seq)
    }

    /// Test hook: while held, the writer takes nothing, so entries stay pending.
    #[cfg(test)]
    pub(crate) fn hold(&self, held: bool) {
        lock(&self.state).held = held;
        self.wake.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        lock(&self.state).items.len()
    }
}
