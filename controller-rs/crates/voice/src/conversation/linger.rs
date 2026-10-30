//! A committed turn's response subscription after the turn ends (afe-metadata.md §2.1,
//! post-afe-audio-architecture.md §4.4): it stays open one second more, or until 30 s
//! after the commit if that comes first, so the chart and the turn recording run on
//! through the end of the answer. The turn's row waits with it, and is charted and
//! handed to the records service when the subscription closes: at that deadline, when
//! the device ends it, or at once on a mute or the session's end, which erase the
//! response's part of the recording before its audio goes. A newer turn may run
//! meanwhile; each lingering row keeps its own turn, subscription and continuation.

use std::time::Duration;

use echomuse_records::{TurnAudio, TurnRecord, vocab::ContinuationEnd};
use echomuse_uplink::{CloseReason, Delivery, SubscriptionId};
use tokio::time::Instant;
use uuid::Uuid;

use super::{
    Conversation,
    record::chart_turn,
    turn::{Turn, write_turn},
};
use crate::{io::Now, session::ConversationCx};

/// How long the response subscription stays open after its turn ends.
const LINGER: Duration = Duration::from_secs(1);

/// An ended turn whose response subscription is still open, with its row.
pub(super) struct Lingering {
    turn: Box<Turn>,
    subscription: SubscriptionId,
    until: Instant,
    record: Box<TurnRecord>,
    audio: TurnAudio,
}

impl Conversation {
    /// Keeps an ended turn's row until its response subscription closes.
    pub(super) fn linger(
        &mut self,
        turn: Turn,
        subscription: SubscriptionId,
        cap: Instant,
        record: TurnRecord,
        audio: TurnAudio,
        now: Now,
    ) {
        self.lingering.push(Lingering {
            turn: Box::new(turn),
            subscription,
            until: cap.min(now.at + LINGER),
            record: Box::new(record),
            audio,
        });
    }

    /// When the next lingering response subscription closes.
    pub(super) fn linger_deadline(&self) -> Option<Instant> {
        self.lingering.iter().map(|l| l.until).min()
    }

    /// Whether `subscription` is a lingering response subscription.
    pub(crate) fn is_lingering(&self, subscription: &SubscriptionId) -> bool {
        self.lingering
            .iter()
            .any(|l| l.subscription == *subscription)
    }

    /// A lingering response subscription's audio: its AFE records join its turn's, its
    /// mic stays in the timeline; its end finishes the row.
    pub(crate) fn lingering_delivery(
        &mut self,
        cx: &mut ConversationCx<'_>,
        delivery: Delivery,
        now: Now,
    ) {
        let subscription = delivery.subscription().clone();
        let Some(index) = self
            .lingering
            .iter()
            .position(|l| l.subscription == subscription)
        else {
            return;
        };
        match delivery {
            Delivery::Chunk(chunk) => {
                if let echomuse_uplink::ChunkBody::Data {
                    data: echomuse_uplink::ChunkData::Afe(records),
                    ..
                } = chunk.body
                {
                    let turn = &mut self.lingering[index].turn;
                    crate::analysis::store_afe(&mut turn.afe, chunk.span.start, records);
                }
            }
            Delivery::Ended(_) => {
                let lingering = self.lingering.swap_remove(index);
                finish_lingering(cx, lingering, false, false, now);
            }
        }
    }

    /// Closes the response subscriptions whose second has passed and writes their rows.
    pub(super) fn linger_timers(&mut self, cx: &mut ConversationCx<'_>, now: Now) {
        let mut index = 0;
        while index < self.lingering.len() {
            if self.lingering[index].until <= now.at {
                let lingering = self.lingering.swap_remove(index);
                finish_lingering(cx, lingering, true, false, now);
            } else {
                index += 1;
            }
        }
    }

    /// A mute or the session's end: every lingering row is charted and written now,
    /// before the audio goes, without the response's part of its recording.
    pub(crate) fn erase_lingering(&mut self, cx: &mut ConversationCx<'_>, now: Now) {
        for lingering in std::mem::take(&mut self.lingering) {
            finish_lingering(cx, lingering, false, true, now);
        }
    }

    /// A question's continuation for a row still lingering goes into it; false when no
    /// lingering row is the question's.
    pub(super) fn resolve_lingering(&mut self, question: Uuid, end: ContinuationEnd) -> bool {
        use echomuse_records::vocab::Continuation;
        let Some(lingering) = self.lingering.iter_mut().find(|l| l.turn.uuid == question) else {
            return false;
        };
        if lingering.record.continuation == Some(Continuation::Pending) {
            lingering.record.continuation = Some(Continuation::Resolved(end));
        }
        true
    }

    /// The live turn's chart before a mute or the session's end clears its audio; its
    /// response subscription's part is erased (afe-metadata.md §2.1). The evidence the
    /// turn was authorized to keep is charted first.
    pub(crate) fn chart_before_erasure(&mut self, cx: &ConversationCx<'_>) {
        if let Some(turn) = self.turn.as_mut() {
            // The response subscription goes with the session's audio.
            turn.response = None;
            if turn.facts.afe_evidence.is_none() {
                chart_turn(cx.audio, turn, true);
            }
        }
    }
}

fn finish_lingering(
    cx: &mut ConversationCx<'_>,
    lingering: Lingering,
    close: bool,
    response_erased: bool,
    now: Now,
) {
    let Lingering {
        mut turn,
        subscription,
        mut record,
        mut audio,
        ..
    } = lingering;
    if close {
        cx.audio
            .close(&subscription, CloseReason::Closed, cx.facts.mono(now.at));
    }
    chart_turn(cx.audio, &mut turn, response_erased);
    record.afe_series = turn.facts.afe_series.take();
    if let Some(evidence) = turn.facts.afe_evidence.take() {
        record.set_afe_evidence(evidence);
    }
    audio.turn_recording = turn.facts.turn_recording.take();
    write_turn(cx, *record, audio, turn.facts.fix_pending);
}
