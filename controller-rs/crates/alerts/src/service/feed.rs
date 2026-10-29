//! One speaker's calendar feed: finds or creates its Local Calendar, reads the backlog of
//! past occurrences and keeps one `calendar/event/subscribe` open for the window
//! [now − 30 min, now + 7 days] (architecture §10.4, §16.4, §16.7).
//!
//! It re-subscribes hourly to roll the window, after every reconnect, and when the
//! applier asks for a fresh read. Before each subscription it reads the backlog (REST,
//! [now − 90 days, now − 30 min]) so missed occurrences are found. Everything it learns
//! goes to the service as [`FeedEvent`]s; it decides nothing itself.

use std::{sync::Arc, time::Duration};

use echomuse_core::{
    services::{EffectOutcome, HaConnectionGeneration},
    types::DeviceId,
};
use echomuse_ha::{CalendarItem, CalendarPush, HaStatus};
use jiff::Timestamp;
use tokio::sync::{mpsc, watch};

use super::Msg;
use crate::ports::{AlertClock, AlertHa, CalendarFeed};

/// Requests to the feed.
pub(crate) enum FeedControl {
    /// Read the calendar afresh now.
    Resubscribe,
    /// The speaker's calendar title changed (used only while none is bound).
    Retitle(String),
}

/// What the feed learned.
pub(crate) enum FeedEvent {
    /// The speaker's calendar.
    Bound { entry_id: String, entity: String },
    /// Occurrences before the window.
    Backlog {
        generation: HaConnectionGeneration,
        items: Vec<CalendarItem>,
    },
    /// One push of the subscription.
    Push {
        generation: HaConnectionGeneration,
        window_start_ms: i64,
        window_end_ms: i64,
        push: CalendarPush,
    },
    /// Something failed; the feed retries.
    Problem(String),
}

/// Timing of the feed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FeedTiming {
    pub window_back_ms: i64,
    pub window_ahead_ms: i64,
    pub backlog_ms: i64,
    pub resubscribe_every: Duration,
    pub retry_initial: Duration,
    pub retry_max: Duration,
}

pub(crate) struct Feed<H, C> {
    pub device: DeviceId,
    pub ha: Arc<H>,
    pub clock: Arc<C>,
    pub title: String,
    pub binding: Option<(String, String)>,
    pub control: mpsc::UnboundedReceiver<FeedControl>,
    pub status: watch::Receiver<HaStatus>,
    pub actor: mpsc::UnboundedSender<Msg>,
    pub timing: FeedTiming,
}

fn rfc3339(ms: i64) -> String {
    Timestamp::from_millisecond(ms).map_or_else(|_| String::new(), |ts| ts.to_string())
}

enum Attempt {
    /// Subscribed and followed until it ended or needed renewing.
    Ended,
    /// Something failed; wait before trying again.
    Failed,
}

impl<H: AlertHa, C: AlertClock> Feed<H, C> {
    fn emit(&self, event: FeedEvent) {
        let _ = self.actor.send(Msg::Feed(self.device.clone(), event));
    }

    pub(crate) async fn run(mut self) {
        let mut backoff = self.timing.retry_initial;
        loop {
            let generation = self.connected().await;
            match self.attempt(generation).await {
                Attempt::Ended => backoff = self.timing.retry_initial,
                Attempt::Failed => {
                    // Wait out the backoff, or less if the connection changes.
                    let _ = tokio::time::timeout(backoff, self.status.changed()).await;
                    backoff = (backoff * 2).min(self.timing.retry_max);
                }
            }
        }
    }

    /// Waits for a connection; its generation.
    async fn connected(&mut self) -> HaConnectionGeneration {
        loop {
            if let Some(generation) = self.status.borrow_and_update().generation {
                return generation;
            }
            if self.status.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    async fn bind(&mut self) -> Option<(String, String)> {
        if let Some(binding) = &self.binding {
            return Some(binding.clone());
        }
        let entries = match self.ha.local_calendars().await {
            Ok(entries) => entries,
            Err(error) => {
                self.emit(FeedEvent::Problem(format!(
                    "listing Local Calendars: {error}"
                )));
                return None;
            }
        };
        let entry = match entries.into_iter().find(|e| e.title == self.title) {
            Some(entry) => entry,
            None => match self.ha.create_local_calendar(self.title.clone()).await {
                EffectOutcome::Applied(entry) => entry,
                EffectOutcome::Rejected { code, message } => {
                    self.emit(FeedEvent::Problem(format!(
                        "creating calendar {:?}: {code} {message}",
                        self.title
                    )));
                    return None;
                }
                // Unknown or not sent: the next attempt looks for it by title first.
                _ => return None,
            },
        };
        let entity = match self.ha.calendar_entities(entry.entry_id.clone()).await {
            Ok(entities) => entities.into_iter().next(),
            Err(error) => {
                self.emit(FeedEvent::Problem(format!(
                    "reading calendar entities: {error}"
                )));
                return None;
            }
        };
        // HA registers the entity just after the entry; a later attempt finds it.
        let entity = entity?;
        let binding = (entry.entry_id, entity);
        self.binding = Some(binding.clone());
        self.emit(FeedEvent::Bound {
            entry_id: binding.0.clone(),
            entity: binding.1.clone(),
        });
        Some(binding)
    }

    async fn attempt(&mut self, generation: HaConnectionGeneration) -> Attempt {
        let Some((_, entity)) = self.bind().await else {
            return Attempt::Failed;
        };
        let now = self.clock.now_utc_ms();
        let back = now - self.timing.window_back_ms;
        match self
            .ha
            .calendar_events(
                entity.clone(),
                rfc3339(now - self.timing.backlog_ms),
                rfc3339(back),
            )
            .await
        {
            Ok(items) => self.emit(FeedEvent::Backlog { generation, items }),
            Err(error) => self.emit(FeedEvent::Problem(format!("reading the backlog: {error}"))),
        }
        let ahead = now + self.timing.window_ahead_ms;
        let mut feed = match self
            .ha
            .subscribe_calendar(entity, rfc3339(back), rfc3339(ahead))
            .await
        {
            Ok(feed) => feed,
            Err(error) => {
                self.emit(FeedEvent::Problem(format!(
                    "subscribing to the calendar: {error}"
                )));
                return Attempt::Failed;
            }
        };
        let renew = tokio::time::Instant::now() + self.timing.resubscribe_every;
        loop {
            tokio::select! {
                push = feed.next() => match push {
                    Some(push) => self.emit(FeedEvent::Push {
                        generation,
                        window_start_ms: back,
                        window_end_ms: ahead,
                        push,
                    }),
                    None => return Attempt::Ended,
                },
                control = self.control.recv() => match control {
                    Some(FeedControl::Resubscribe) => break,
                    Some(FeedControl::Retitle(title)) => self.title = title,
                    None => std::future::pending::<()>().await,
                },
                () = tokio::time::sleep_until(renew) => break,
                changed = self.status.changed() => {
                    if changed.is_err() || self.status.borrow().generation != Some(generation) {
                        return Attempt::Ended;
                    }
                }
            }
        }
        // Coalesce requests that arrived meanwhile into this one fresh read.
        while let Ok(control) = self.control.try_recv() {
            if let FeedControl::Retitle(title) = control {
                self.title = title;
            }
        }
        self.ha.unsubscribe_calendar(feed).await;
        Attempt::Ended
    }
}
