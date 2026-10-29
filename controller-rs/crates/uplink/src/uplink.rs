//! One session's audio subscriptions, driven by the device runtime.
//!
//! The runtime owns one [`Uplink`] per device session and calls it in ingress
//! order: `stream.open`, `stream.end` and every uplink packet after its
//! [`StreamRegistry`](echomuse_protocol::streams::StreamRegistry) accepted
//! them, `uplink.ended`, the privacy mute and session loss. Controller
//! decisions (accepting a candidate, converting it into a turn, opening and
//! closing subscriptions) return [`Grant`]s the runtime applies to its stream
//! registry at once, and queue [`Outgoing`] commands it sends next. Each
//! subscription's audio comes out as ordered [`Delivery`] values.
//!
//! # Ordering
//!
//! For every subscription, stream and epoch, chunks are released in sample
//! order from the subscription's start, without overlap and without holes:
//! a range that is not held yet stops the stream until it arrives or is known
//! never to arrive. A range is known never to arrive when:
//!
//! - the device flagged a discontinuity after skipping it;
//! - the subscription's own backfill run began after it (a clipped start);
//! - the device moved past it on the subscription's own run or on live audio
//!   without sending it;
//! - the live position is further past it than the device's ring holds;
//! - later audio waited behind it for [`UplinkConfig::hole_wait`].
//!
//! The device sends a subscription's backfill as one run in the order the
//! subscriptions became active (see [`crate::timeline`]), so an older
//! subscription's live audio may arrive before a newer one's backfill. A
//! subscription that starts wanting a stream nobody else wants takes the
//! first packet that follows as its run's start.
//!
//! # Privacy
//!
//! [`Uplink::mute`] ends every subscription at once, discards their queued
//! deliveries, empties every buffer, and refuses any later packet of the
//! muted capture epochs. While muted nothing can open.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::num::NonZeroU64;

use echomuse_protocol::StreamId;
use echomuse_protocol::ema::{Flags, Packet};
use echomuse_protocol::streams::{
    BackfillGrant, StreamEnd, StreamOpen, UplinkEnded, UplinkEndedReason,
};
use echomuse_turn::{MonoInstant, Sample};

use crate::buffer::{EpochBuffer, Lookup, Span};
use crate::chunk::{
    Accounting, Backlog, Chunk, ChunkBody, Delivery, EndCause, Ended, MissingCause,
};
use crate::config::{SessionTerms, UplinkConfig};
use crate::starts::{
    CandidateStarts, NEAR_MISS_AFE_LEAD, NEAR_MISS_GRACE, NEAR_MISS_TAIL, device_grid_start,
    device_history, record_samples,
};
use crate::timeline::{Attribution, Claim, Path, SubKey, Timeline};
use crate::wire::{
    CloseReason, Conversion, ConversionReason, Generation, OpenReason, Outgoing, Owner,
    RENEW_INTERVAL, SUBSCRIPTION_TTL, StreamStart, SubscriptionId, SubscriptionReason, UplinkClose,
    UplinkCommand, UplinkOpen, UplinkRenew,
};

/// A backfill grant the runtime applies to its stream registry with
/// `grant_backfill(stream, grant)` before sending anything else: one epoch
/// for a capture stream, every open epoch for the reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Grant {
    /// The stream.
    pub stream: StreamId,
    /// The epochs covered.
    pub grant: BackfillGrant,
}

/// A `wake.candidate` the controller accepts (`command.ack` `accepted`),
/// which releases the candidate subscription's audio.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateOpen {
    /// The candidate subscription (`lease_id`).
    pub subscription_id: SubscriptionId,
    /// `capture_epoch`.
    pub capture_epoch: NonZeroU64,
    /// `support_start`.
    pub support_start: Sample,
}

/// A `wake.near_miss` to record (`near_miss_v1` only), while the speaker's
/// `saveWakeClips` is on and capture is permitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NearMissOpen {
    /// `near_miss_id`, the subscription's owner.
    pub near_miss_id: Owner,
    /// `capture_epoch`.
    pub capture_epoch: NonZeroU64,
    /// `support_start`.
    pub support_start: Sample,
    /// `support_end`.
    pub support_end: Sample,
}

/// A subscription the controller opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Opened {
    /// Its ID.
    pub subscription_id: SubscriptionId,
    /// The grants to apply before sending its `uplink.open`.
    pub grants: Vec<Grant>,
}

/// What became of one packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ingested {
    /// A subscription wants it; this many of its stream samples were new.
    Stored {
        /// Stream samples not held before.
        new_samples: u64,
    },
    /// No subscription wants its stream epoch, so it was dropped.
    Unwanted,
    /// Its capture epoch was erased by a mute: refused.
    ErasedEpoch,
    /// Its stream epoch is not in the timeline (never announced to it).
    UnknownEpoch,
    /// A render frame, which is never uplink audio.
    NotUplink,
}

/// Why a subscription could not open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpenError {
    /// The privacy mute is on.
    Muted,
    /// The device has announced no capture epoch.
    NoCaptureEpoch,
    /// The request names another capture epoch than the current one.
    StaleEpoch {
        /// The current capture epoch.
        current: Option<NonZeroU64>,
        /// The epoch the request named.
        requested: NonZeroU64,
    },
    /// A subscription with this ID is already open.
    DuplicateId,
    /// No stream is wanted.
    NoStreams,
    /// The `afe` stream was asked for in a session without `afe_metadata`.
    AfeUnavailable,
    /// A near-miss subscription for a device without `near_miss_v1`.
    NearMissUnsupported,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Muted => f.write_str("the device is muted"),
            OpenError::NoCaptureEpoch => f.write_str("the device has no capture epoch"),
            OpenError::StaleEpoch { current, requested } => write!(
                f,
                "capture epoch {requested} is not the current one ({})",
                current.map_or_else(|| "none".to_owned(), |e| e.to_string())
            ),
            OpenError::DuplicateId => f.write_str("a subscription with this ID is open"),
            OpenError::NoStreams => f.write_str("no stream is wanted"),
            OpenError::AfeUnavailable => f.write_str("this session has no afe stream"),
            OpenError::NearMissUnsupported => {
                f.write_str("the device does not take near-miss subscriptions")
            }
        }
    }
}

impl std::error::Error for OpenError {}

/// Why a command about a subscription could not be issued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandError {
    /// No such open subscription.
    UnknownSubscription,
    /// Only a candidate subscription converts into a turn.
    NotACandidate,
    /// The device already ended it; it is only delivering what was in flight.
    Ending,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CommandError::UnknownSubscription => "no such open subscription",
            CommandError::NotACandidate => "only a candidate subscription converts into a turn",
            CommandError::Ending => "the device already ended the subscription",
        })
    }
}

impl std::error::Error for CommandError {}

/// A snapshot of one subscription.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionView {
    /// Its ID.
    pub id: SubscriptionId,
    /// What it serves.
    pub owner: Owner,
    /// Its reason.
    pub reason: SubscriptionReason,
    /// Its generation.
    pub generation: Generation,
    /// Its capture epoch.
    pub capture_epoch: NonZeroU64,
    /// The streams it wants.
    pub streams: Vec<StreamId>,
    /// The device ended it and it is delivering what was in flight.
    pub draining: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartSpec {
    /// A capture stream from this sample, on the device's grid.
    At(u64),
    /// From the live position.
    Live,
    /// The reference from a capture sample the device maps itself.
    Mapped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    /// Waiting for the backfill run the device owes it.
    AwaitClaim,
    /// Waiting for the first live packet to know where it starts.
    AwaitLive,
    /// Its own packets now arrive in device order.
    Ordered,
}

#[derive(Debug)]
struct Cursor {
    epoch: NonZeroU64,
    start: StartSpec,
    mode: Mode,
    /// The next position owed; `None` until known.
    pos: Option<u64>,
    /// Everything below this that is not held will never arrive.
    settled_to: u64,
    settle_cause: MissingCause,
    /// When later audio started waiting behind a hole at `pos`.
    stalled_since: Option<MonoInstant>,
    /// After `uplink.ended`: one past the last position the device sent.
    target: Option<u64>,
}

impl Cursor {
    fn new(epoch: NonZeroU64, start: StartSpec) -> Self {
        let (mode, pos) = match start {
            StartSpec::At(sample) => (Mode::AwaitClaim, Some(sample)),
            StartSpec::Mapped => (Mode::AwaitClaim, None),
            StartSpec::Live => (Mode::AwaitLive, None),
        };
        Self {
            epoch,
            start,
            mode,
            pos,
            settled_to: 0,
            settle_cause: MissingCause::NotSent,
            stalled_since: None,
            target: None,
        }
    }

    fn settle(&mut self, to: u64, cause: MissingCause) {
        if to > self.settled_to {
            self.settled_to = to;
            self.settle_cause = cause;
        }
    }

    fn attribute(&mut self, attribution: Attribution, stream: StreamId) {
        if self.mode != Mode::AwaitClaim {
            return;
        }
        match (attribution, self.start) {
            (Attribution::Run { start }, StartSpec::At(requested)) => {
                if start > requested {
                    self.settle(start, MissingCause::Clipped);
                }
                self.mode = Mode::Ordered;
            }
            (Attribution::Run { start }, _) => {
                self.pos = Some(start.next_multiple_of(anchor_grid(stream)));
                self.mode = Mode::Ordered;
            }
            (Attribution::NoRun, StartSpec::At(_)) => self.mode = Mode::Ordered,
            (Attribution::NoRun, _) => self.mode = Mode::AwaitLive,
        }
    }

    fn on_packet(&mut self, path: Path, first: u64, own: SubKey, stream: StreamId) {
        match self.mode {
            Mode::AwaitLive if path == Path::Advance => {
                self.pos = Some(first.next_multiple_of(anchor_grid(stream)));
                self.mode = Mode::Ordered;
            }
            Mode::Ordered if path == Path::Advance || path == Path::Run(Some(own)) => {
                if let Some(pos) = self.pos
                    && first > pos.max(self.settled_to)
                {
                    self.settle(first, MissingCause::NotSent);
                }
            }
            _ => {}
        }
    }

    fn stall_deadline(&self, wait: std::time::Duration) -> Option<MonoInstant> {
        self.stalled_since.map(|since| since.after(wait))
    }

    fn done(&self) -> bool {
        match self.target {
            None => true,
            Some(target) => self.pos.is_some_and(|pos| pos >= target),
        }
    }
}

/// Where a subscription's position may start on a stream: the grids the
/// device rounds every start to.
fn anchor_grid(stream: StreamId) -> u64 {
    match stream {
        StreamId::Mic | StreamId::Cells => 512,
        StreamId::Afe => 1_280,
        StreamId::Reference => 2_560,
    }
}

#[derive(Debug)]
struct Want {
    stream: StreamId,
    /// One per epoch: a capture stream has one; the reference gains one with
    /// every new reference epoch while the subscription lives.
    cursors: Vec<Cursor>,
}

#[derive(Debug)]
enum Phase {
    Active {
        next_renewal: MonoInstant,
    },
    Draining {
        report: UplinkEnded,
        deadline: MonoInstant,
    },
}

#[derive(Clone, Copy, Debug)]
struct NearMissPlan {
    /// One past the last mic sample it must hold: `support_end + 4800`.
    mic_through: u64,
    afe: bool,
}

#[derive(Debug)]
struct Sub {
    key: SubKey,
    id: SubscriptionId,
    owner: Owner,
    reason: SubscriptionReason,
    generation: Generation,
    capture_epoch: NonZeroU64,
    wants: Vec<Want>,
    phase: Phase,
    near_miss: Option<NearMissPlan>,
    overflowed: bool,
}

impl Sub {
    fn wants_on(&self, stream: StreamId, epoch: u64) -> bool {
        self.wants
            .iter()
            .any(|w| w.stream == stream && w.cursors.iter().any(|c| c.epoch.get() == epoch))
    }

    fn position(&self, stream: StreamId) -> Option<u64> {
        self.wants
            .iter()
            .find(|w| w.stream == stream)
            .and_then(|w| w.cursors.last())
            .and_then(|c| c.pos)
    }

    fn near_miss_done(&self) -> bool {
        let Some(plan) = self.near_miss else {
            return false;
        };
        let Some(mic) = self.position(StreamId::Mic) else {
            return false;
        };
        if mic < plan.mic_through {
            return false;
        }
        !plan.afe
            || self
                .position(StreamId::Afe)
                .is_some_and(|afe| afe >= plan.mic_through)
            || mic >= plan.mic_through + NEAR_MISS_GRACE.0
    }

    fn view(&self) -> SubscriptionView {
        SubscriptionView {
            id: self.id.clone(),
            owner: self.owner.clone(),
            reason: self.reason,
            generation: self.generation,
            capture_epoch: self.capture_epoch,
            streams: self.wants.iter().map(|w| w.stream).collect(),
            draining: matches!(self.phase, Phase::Draining { .. }),
        }
    }
}

fn find_cursor(wants: &mut [Want], stream: StreamId, epoch: u64) -> Option<&mut Cursor> {
    wants
        .iter_mut()
        .find(|w| w.stream == stream)
        .and_then(|w| w.cursors.iter_mut().find(|c| c.epoch.get() == epoch))
}

/// A subscription the controller ended while the device may still be sending
/// it: its unanswered backfill claims stay in line until the device's
/// `uplink.ended` arrives, so its run is not handed to anyone else.
#[derive(Debug)]
struct Zombie {
    key: SubKey,
    id: SubscriptionId,
    streams: Vec<(StreamId, u64)>,
    expires: MonoInstant,
}

#[derive(Debug, Default)]
struct Account {
    backlog: Backlog,
    delivered: Accounting,
}

#[derive(Debug)]
struct Outbox {
    queue: VecDeque<Delivery>,
    accounts: HashMap<SubscriptionId, Account>,
    max_bytes: u64,
    max_deliveries: usize,
}

impl Outbox {
    /// Queues a delivery; false when its subscription is now over its
    /// bounds. An [`Ended`] is always queued.
    fn push(&mut self, delivery: Delivery) -> bool {
        let account = self
            .accounts
            .entry(delivery.subscription().clone())
            .or_default();
        account.backlog.deliveries += 1;
        account.backlog.payload_bytes += delivery.payload_bytes();
        let within = account.backlog.payload_bytes <= self.max_bytes
            && account.backlog.deliveries <= self.max_deliveries;
        self.queue.push_back(delivery);
        within
    }

    fn drain(&mut self, out: &mut Vec<Delivery>) {
        for delivery in self.queue.drain(..) {
            let id = delivery.subscription();
            if let Some(account) = self.accounts.get_mut(id) {
                account.backlog.deliveries -= 1;
                account.backlog.payload_bytes -= delivery.payload_bytes();
                if let Delivery::Chunk(chunk) = &delivery {
                    account.delivered.chunks += 1;
                    account.delivered.payload_bytes += chunk.payload_bytes();
                }
                if matches!(delivery, Delivery::Ended(_)) {
                    self.accounts.remove(id);
                }
            }
            out.push(delivery);
        }
    }

    /// Drops every queued chunk, keeping only end notices.
    fn discard_chunks(&mut self) {
        self.queue.retain(|d| matches!(d, Delivery::Ended(_)));
        for account in self.accounts.values_mut() {
            account.backlog = Backlog::default();
        }
        for delivery in &self.queue {
            if let Some(account) = self.accounts.get_mut(delivery.subscription()) {
                account.backlog.deliveries += 1;
            }
        }
    }
}

/// What a release reads from.
struct Source<'a> {
    buffer: &'a EpochBuffer,
    live_end: Option<u64>,
    packet: Option<&'a Packet>,
    stream: StreamId,
    epoch: NonZeroU64,
}

impl Source<'_> {
    /// Below this the device's ring can no longer hold anything.
    fn overwritten_below(&self) -> Option<u64> {
        let unit = record_samples(self.stream);
        let gone = self.live_end?.checked_sub(device_history(self.stream))?;
        Some(gone - gone % unit)
    }
}

/// Releases what `cursor` can deliver now. False when the subscription went
/// over its delivery bounds.
fn release(
    cursor: &mut Cursor,
    source: &Source<'_>,
    id: &SubscriptionId,
    generation: Generation,
    outbox: &mut Outbox,
    config: &UplinkConfig,
    now: MonoInstant,
) -> bool {
    let Some(mut pos) = cursor.pos else {
        return true;
    };
    loop {
        let (end, body) = match source.buffer.lookup(pos) {
            Lookup::Data {
                end,
                meta,
                discontinuity,
            } => {
                let data = source
                    .buffer
                    .chunk_data(Span { start: pos, end }, &meta, source.packet);
                (
                    end,
                    ChunkBody::Data {
                        meta,
                        discontinuity,
                        data,
                    },
                )
            }
            Lookup::Missing { end } => (end, ChunkBody::Missing(MissingCause::Device)),
            Lookup::Evicted { until } => (until, ChunkBody::Missing(MissingCause::Evicted)),
            Lookup::Unknown { next_known } => {
                let known = next_known.unwrap_or(u64::MAX);
                if pos < cursor.settled_to {
                    (
                        cursor.settled_to.min(known),
                        ChunkBody::Missing(cursor.settle_cause),
                    )
                } else if let Some(gone) = source.overwritten_below().filter(|g| *g > pos) {
                    (
                        gone.min(known),
                        ChunkBody::Missing(MissingCause::Overwritten),
                    )
                } else {
                    let limit = next_known.or(cursor.target.filter(|t| *t > pos));
                    match (limit, cursor.stalled_since) {
                        (None, _) => {
                            cursor.stalled_since = None;
                            return true;
                        }
                        (Some(_), None) => {
                            cursor.stalled_since = Some(now);
                            return true;
                        }
                        (Some(limit), Some(since)) if now >= since.after(config.hole_wait) => {
                            (limit, ChunkBody::Missing(MissingCause::TimedOut))
                        }
                        _ => return true,
                    }
                }
            }
        };
        let chunk = Chunk {
            subscription: id.clone(),
            generation,
            stream: source.stream,
            epoch: source.epoch,
            span: Span { start: pos, end },
            body,
        };
        pos = end;
        cursor.pos = Some(pos);
        cursor.stalled_since = None;
        if !outbox.push(Delivery::Chunk(chunk)) {
            return false;
        }
    }
}

/// One device session's audio timeline and subscriptions. Owned and driven by
/// the device runtime; nothing here reads a clock or does I/O.
#[derive(Debug)]
pub struct Uplink {
    config: UplinkConfig,
    terms: SessionTerms,
    timeline: Timeline,
    muted: bool,
    subs: Vec<Sub>,
    zombies: Vec<Zombie>,
    next_key: u64,
    commands: Vec<Outgoing>,
    outbox: Outbox,
}

impl Uplink {
    /// A new session's uplink. `muted` is the hello's privacy state.
    pub fn new(config: UplinkConfig, terms: SessionTerms, muted: bool) -> Self {
        Self {
            config,
            terms,
            timeline: Timeline::new(config),
            muted,
            subs: Vec::new(),
            zombies: Vec::new(),
            next_key: 0,
            commands: Vec::new(),
            outbox: Outbox {
                queue: VecDeque::new(),
                accounts: HashMap::new(),
                max_bytes: config.max_backlog_bytes,
                max_deliveries: config.max_backlog_deliveries,
            },
        }
    }

    /// What the session allows.
    pub fn terms(&self) -> SessionTerms {
        self.terms
    }

    /// The retained audio, for reads by range.
    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// Whether the privacy mute is on.
    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// A snapshot of one live subscription.
    pub fn subscription(&self, id: &SubscriptionId) -> Option<SubscriptionView> {
        self.subs.iter().find(|s| &s.id == id).map(Sub::view)
    }

    /// Snapshots of every live subscription.
    pub fn subscriptions(&self) -> Vec<SubscriptionView> {
        self.subs.iter().map(Sub::view).collect()
    }

    /// Whether a diagnostic subscription is open: the controller refuses
    /// every wake while one is (§4.1).
    pub fn diagnostic_live(&self) -> bool {
        self.subs.iter().any(|s| {
            s.reason == SubscriptionReason::Diagnostic && matches!(s.phase, Phase::Active { .. })
        })
    }

    /// Takes the commands to send, in order.
    pub fn take_commands(&mut self) -> Vec<Outgoing> {
        std::mem::take(&mut self.commands)
    }

    /// Moves every queued delivery to `out`, in the order released.
    pub fn drain(&mut self, out: &mut Vec<Delivery>) {
        self.outbox.drain(out);
    }

    /// What a subscription has waiting; `None` once its end was drained.
    pub fn backlog(&self, id: &SubscriptionId) -> Option<Backlog> {
        self.outbox.accounts.get(id).map(|a| a.backlog)
    }

    /// What a subscription's consumer has taken so far; `None` once its end
    /// was drained.
    pub fn delivered(&self, id: &SubscriptionId) -> Option<Accounting> {
        self.outbox.accounts.get(id).map(|a| a.delivered)
    }

    // ── Device reports ──────────────────────────────────────────────────

    /// A `stream.open` the registry accepted. A new capture epoch ends every
    /// subscription, as the device does; a new reference epoch restarts the
    /// reference of every live subscription at its live position.
    pub fn stream_opened(&mut self, open: &StreamOpen) {
        let (stream, epoch) = (open.stream_id, open.epoch);
        if self.timeline.is_erased(stream, epoch.get()) {
            return;
        }
        match stream {
            StreamId::Reference => {
                self.timeline.set_reference(Some(epoch));
                self.timeline.ensure(stream, epoch);
                for sub in &mut self.subs {
                    if !matches!(sub.phase, Phase::Active { .. }) {
                        continue;
                    }
                    if let Some(want) = sub.wants.iter_mut().find(|w| w.stream == stream)
                        && !want.cursors.iter().any(|c| c.epoch == epoch)
                    {
                        want.cursors.push(Cursor::new(epoch, StartSpec::Live));
                    }
                }
            }
            _ => {
                if self.timeline.capture_epoch() != Some(epoch) {
                    self.end_all(EndCause::CaptureEpoch);
                    self.timeline.set_capture(Some(epoch));
                }
                self.timeline.ensure(stream, epoch);
            }
        }
    }

    /// A `stream.end` the registry accepted. The end of the capture epoch
    /// ends every subscription; the end of a reference epoch closes each
    /// subscription's reference on it, declaring what never came missing.
    pub fn stream_ended(&mut self, end: &StreamEnd, now: MonoInstant) {
        let (stream, epoch) = (end.stream_id, end.epoch);
        if stream != StreamId::Reference {
            if self.timeline.capture_epoch().map(NonZeroU64::get) == Some(epoch) {
                self.end_all(EndCause::CaptureEpoch);
                self.timeline.set_capture(None);
            }
            return;
        }
        if self.timeline.reference_epoch().map(NonZeroU64::get) == Some(epoch) {
            self.timeline.set_reference(None);
        }
        let Self {
            timeline,
            subs,
            outbox,
            config,
            ..
        } = self;
        let Some(held) = timeline.get(stream, epoch) else {
            return;
        };
        let source = Source {
            buffer: &held.buffer,
            live_end: held.track.live_end(),
            packet: None,
            stream,
            epoch: held.epoch,
        };
        for sub in subs.iter_mut() {
            let Some(want) = sub.wants.iter_mut().find(|w| w.stream == stream) else {
                continue;
            };
            let Some(at) = want.cursors.iter().position(|c| c.epoch.get() == epoch) else {
                continue;
            };
            let mut cursor = want.cursors.remove(at);
            cursor.target = None;
            if cursor.pos.is_some() {
                cursor.settle(end.final_sample, MissingCause::NotSent);
            }
            if !release(
                &mut cursor,
                &source,
                &sub.id,
                sub.generation,
                outbox,
                config,
                now,
            ) {
                sub.overflowed = true;
            }
        }
        self.settle_subs(now);
    }

    /// One uplink packet the registry accepted.
    pub fn ingest(&mut self, packet: &Packet, now: MonoInstant) -> Ingested {
        let header = packet.header();
        let Some(stream) = StreamId::of_kind(header.kind) else {
            return Ingested::NotUplink;
        };
        let epoch = header.epoch;
        if self.timeline.is_erased(stream, epoch) {
            return Ingested::ErasedEpoch;
        }
        let Self {
            timeline,
            subs,
            outbox,
            config,
            ..
        } = self;
        let Some(held) = timeline.get_mut(stream, epoch) else {
            return Ingested::UnknownEpoch;
        };
        let first = header.first_sample;
        let classified = held.track.classify(
            first,
            packet.end_sample(),
            header.flags.contains(Flags::DISCONTINUITY),
        );
        if let Some(missing) = classified.missing {
            held.buffer.mark_missing(missing);
        }
        let wanted = subs.iter().any(|s| s.wants_on(stream, epoch));
        let new_samples = if wanted { held.buffer.store(packet) } else { 0 };
        for (key, attribution) in &classified.attributed {
            if let Some(sub) = subs.iter_mut().find(|s| s.key == *key)
                && let Some(cursor) = find_cursor(&mut sub.wants, stream, epoch)
            {
                cursor.attribute(*attribution, stream);
            }
        }
        let source = Source {
            buffer: &held.buffer,
            live_end: held.track.live_end(),
            packet: Some(packet),
            stream,
            epoch: held.epoch,
        };
        for sub in subs.iter_mut() {
            let Some(cursor) = find_cursor(&mut sub.wants, stream, epoch) else {
                continue;
            };
            cursor.on_packet(classified.path, first, sub.key, stream);
            if !release(
                cursor,
                &source,
                &sub.id,
                sub.generation,
                outbox,
                config,
                now,
            ) {
                sub.overflowed = true;
            }
        }
        self.settle_subs(now);
        if wanted {
            Ingested::Stored { new_samples }
        } else {
            Ingested::Unwanted
        }
    }

    /// An `uplink.ended`. A subscription ended for `closed`, `ttl` or
    /// `overrun` first delivers what the report says was sent and may still
    /// be in flight on the audio socket; `mute`, `epoch` and `session` end it
    /// at once.
    pub fn device_ended(&mut self, report: &UplinkEnded, now: MonoInstant) {
        if let Some(zombie) = self
            .zombies
            .iter_mut()
            .find(|z| z.id.as_str() == report.subscription_id)
        {
            zombie.expires = zombie.expires.min(now.after(self.config.hole_wait));
            return;
        }
        let Some(at) = self
            .subs
            .iter()
            .position(|s| s.id.as_str() == report.subscription_id)
        else {
            return;
        };
        if matches!(self.subs[at].phase, Phase::Draining { .. }) {
            return;
        }
        match report.reason {
            UplinkEndedReason::Mute => {
                let id = self.subs[at].id.clone();
                self.outbox
                    .queue
                    .retain(|d| d.subscription() != &id || matches!(d, Delivery::Ended(_)));
                if let Some(account) = self.outbox.accounts.get_mut(&id) {
                    account.backlog = Backlog::default();
                }
                self.end_sub(at, EndCause::Device(report.clone()), Leaves::Gone, now);
            }
            UplinkEndedReason::Epoch | UplinkEndedReason::Session => {
                self.end_sub(at, EndCause::Device(report.clone()), Leaves::Gone, now);
            }
            UplinkEndedReason::Closed | UplinkEndedReason::Ttl | UplinkEndedReason::Overrun => {
                let sub = &mut self.subs[at];
                for want in &mut sub.wants {
                    let last = report.last_sample.get(&want.stream).copied().flatten();
                    let clipped = report.clipped_start.get(&want.stream).copied().flatten();
                    let newest = want.cursors.len().saturating_sub(1);
                    for (i, cursor) in want.cursors.iter_mut().enumerate() {
                        if i != newest {
                            continue;
                        }
                        cursor.target = last.map(|l| l.saturating_add(1));
                        if let Some(clipped) = clipped {
                            match cursor.pos {
                                Some(_) => cursor.settle(clipped, MissingCause::Clipped),
                                None => cursor.pos = Some(clipped),
                            }
                            cursor.mode = Mode::Ordered;
                        }
                    }
                }
                sub.phase = Phase::Draining {
                    report: report.clone(),
                    deadline: now.after(self.config.hole_wait),
                };
                self.release_all(now);
                self.settle_subs(now);
            }
        }
    }

    /// The privacy mute went on: every subscription ends now, queued
    /// deliveries and buffers are discarded, and the muted capture epochs'
    /// packets are refused from now on.
    pub fn mute(&mut self) {
        self.muted = true;
        self.outbox.discard_chunks();
        self.commands.clear();
        self.zombies.clear();
        for sub in std::mem::take(&mut self.subs) {
            self.push_ended(&sub, EndCause::Muted);
        }
        self.timeline.erase();
    }

    /// The privacy mute went off. Subscriptions may open again once the
    /// device announces the new capture epoch.
    pub fn unmute(&mut self) {
        self.muted = false;
    }

    /// The session ended: every subscription ends, and nothing queued or
    /// buffered survives.
    pub fn session_lost(&mut self) {
        self.outbox.discard_chunks();
        self.commands.clear();
        self.zombies.clear();
        for sub in std::mem::take(&mut self.subs) {
            self.push_ended(&sub, EndCause::SessionLost);
        }
        self.timeline.clear();
    }

    // ── Controller decisions ────────────────────────────────────────────

    /// Accepts a `wake.candidate`: the runtime acknowledges it `accepted`
    /// right after applying the returned grants. The candidate subscription
    /// is generation 1, wants mic, cells, reference and (in an
    /// `afe_metadata` session) AFE records from the candidate starts, and is
    /// renewed every second from now on.
    pub fn accept_candidate(
        &mut self,
        candidate: CandidateOpen,
        now: MonoInstant,
    ) -> Result<Vec<Grant>, OpenError> {
        let capture = self.check_open(Some(candidate.capture_epoch))?;
        if self.subs.iter().any(|s| s.id == candidate.subscription_id) {
            return Err(OpenError::DuplicateId);
        }
        let starts = CandidateStarts::new(candidate.support_start, self.terms.afe_metadata);
        let mut streams = BTreeMap::from([
            (StreamId::Mic, StreamStart::At(starts.mic)),
            (StreamId::Cells, StreamStart::At(starts.cells)),
            (StreamId::Reference, StreamStart::At(starts.reference)),
        ]);
        if let Some(afe) = starts.afe {
            streams.insert(StreamId::Afe, StreamStart::At(afe));
        }
        let owner = Owner::of_subscription(&candidate.subscription_id);
        let grants = self.activate(
            candidate.subscription_id,
            owner,
            SubscriptionReason::Candidate,
            capture,
            &streams,
            None,
            now,
        );
        Ok(grants)
    }

    /// Converts an accepted candidate into the turn's subscription: an
    /// `uplink.renew` at the next generation with `reason: "turn"` and the
    /// turn as owner. Its backfilled audio becomes the turn's pre-roll.
    pub fn convert_to_turn(
        &mut self,
        id: &SubscriptionId,
        turn: Owner,
        now: MonoInstant,
    ) -> Result<(), CommandError> {
        let sub = self
            .subs
            .iter_mut()
            .find(|s| &s.id == id)
            .ok_or(CommandError::UnknownSubscription)?;
        let Phase::Active { next_renewal } = &mut sub.phase else {
            return Err(CommandError::Ending);
        };
        if sub.reason != SubscriptionReason::Candidate {
            return Err(CommandError::NotACandidate);
        }
        sub.generation = sub.generation.next();
        sub.reason = SubscriptionReason::Turn;
        sub.owner = turn.clone();
        *next_renewal = now.after(RENEW_INTERVAL);
        self.commands.push(Outgoing {
            generation: sub.generation,
            command: UplinkCommand::Renew(UplinkRenew {
                subscription_id: sub.id.clone(),
                ttl: SUBSCRIPTION_TTL,
                conversion: Some(Conversion {
                    reason: ConversionReason::Turn,
                    owner: turn,
                }),
            }),
        });
        Ok(())
    }

    /// Opens a button turn's subscription: the candidate formulas with the
    /// press sample in place of the support start (300 ms of pre-roll).
    pub fn open_button_turn(
        &mut self,
        turn: Owner,
        press: Sample,
        now: MonoInstant,
    ) -> Result<Opened, OpenError> {
        let starts = CandidateStarts::new(press, self.terms.afe_metadata);
        let mut streams = BTreeMap::from([
            (StreamId::Mic, StreamStart::At(starts.mic)),
            (StreamId::Cells, StreamStart::At(starts.cells)),
            (StreamId::Reference, StreamStart::At(starts.reference)),
        ]);
        if let Some(afe) = starts.afe {
            streams.insert(StreamId::Afe, StreamStart::At(afe));
        }
        self.open(OpenReason::Turn, turn, streams, now)
    }

    /// Opens a reply window's subscription: mic, reference and (with
    /// `afe_metadata`) AFE records live, cells from `cells_from`, 10 s before
    /// the window opens.
    pub fn open_reply(
        &mut self,
        expectation: Owner,
        cells_from: Sample,
        now: MonoInstant,
    ) -> Result<Opened, OpenError> {
        let mut streams = BTreeMap::from([
            (StreamId::Mic, StreamStart::Live),
            (StreamId::Reference, StreamStart::Live),
            (StreamId::Cells, StreamStart::At(cells_from)),
        ]);
        if self.terms.afe_metadata {
            streams.insert(StreamId::Afe, StreamStart::Live);
        }
        self.open(OpenReason::Reply, expectation, streams, now)
    }

    /// Opens a dashboard recording mode's subscription: mic live, and AFE
    /// records live with `afe_metadata` for the recording's charts.
    pub fn open_diagnostic(&mut self, mode: Owner, now: MonoInstant) -> Result<Opened, OpenError> {
        let mut streams = BTreeMap::from([(StreamId::Mic, StreamStart::Live)]);
        if self.terms.afe_metadata {
            streams.insert(StreamId::Afe, StreamStart::Live);
        }
        self.open(OpenReason::Diagnostic, mode, streams, now)
    }

    /// Opens the response recording when a turn commits in an
    /// `afe_metadata` session, before the committed subscription closes:
    /// AFE records from where the committed subscription's reached and, when
    /// the speaker saves utterances, mic from its last contiguous sample.
    pub fn open_response(
        &mut self,
        turn: Owner,
        afe_from: Sample,
        mic_from: Option<Sample>,
        now: MonoInstant,
    ) -> Result<Opened, OpenError> {
        let mut streams = BTreeMap::from([(StreamId::Afe, StreamStart::At(afe_from))]);
        if let Some(mic) = mic_from {
            streams.insert(StreamId::Mic, StreamStart::At(mic));
        }
        self.open(OpenReason::Turn, turn, streams, now)
    }

    /// Opens a near miss's recording on a `near_miss_v1` device: mic from
    /// `round_down(support_start − 4800, 512)`, and with `afe_metadata` AFE
    /// records from `support_start − 16000`. The controller closes it
    /// (`closed`) itself once it holds mic through `support_end + 4800` and,
    /// when it took AFE records, the records through it too, or the mic has
    /// run 16,000 samples past that without them.
    pub fn open_near_miss(
        &mut self,
        near_miss: NearMissOpen,
        now: MonoInstant,
    ) -> Result<Opened, OpenError> {
        if !self.terms.near_miss {
            return Err(OpenError::NearMissUnsupported);
        }
        self.check_open(Some(near_miss.capture_epoch))?;
        let mic = CandidateStarts::new(near_miss.support_start, false).mic;
        let mut streams = BTreeMap::from([(StreamId::Mic, StreamStart::At(mic))]);
        if self.terms.afe_metadata {
            streams.insert(
                StreamId::Afe,
                StreamStart::At(near_miss.support_start - NEAR_MISS_AFE_LEAD),
            );
        }
        let plan = NearMissPlan {
            mic_through: (near_miss.support_end + NEAR_MISS_TAIL).0 + 1,
            afe: self.terms.afe_metadata,
        };
        self.open_with(
            OpenReason::NearMiss,
            near_miss.near_miss_id,
            streams,
            Some(plan),
            now,
        )
    }

    /// Opens a subscription with `uplink.open` at generation 1.
    pub fn open(
        &mut self,
        reason: OpenReason,
        owner: Owner,
        streams: BTreeMap<StreamId, StreamStart>,
        now: MonoInstant,
    ) -> Result<Opened, OpenError> {
        if reason == OpenReason::NearMiss && !self.terms.near_miss {
            return Err(OpenError::NearMissUnsupported);
        }
        self.open_with(reason, owner, streams, None, now)
    }

    fn open_with(
        &mut self,
        reason: OpenReason,
        owner: Owner,
        streams: BTreeMap<StreamId, StreamStart>,
        near_miss: Option<NearMissPlan>,
        now: MonoInstant,
    ) -> Result<Opened, OpenError> {
        let capture = self.check_open(None)?;
        if streams.is_empty() {
            return Err(OpenError::NoStreams);
        }
        if streams.contains_key(&StreamId::Afe) && !self.terms.afe_metadata {
            return Err(OpenError::AfeUnavailable);
        }
        let id = SubscriptionId::generate();
        let grants = self.activate(
            id.clone(),
            owner.clone(),
            reason.into(),
            capture,
            &streams,
            near_miss,
            now,
        );
        self.commands.push(Outgoing {
            generation: Generation::FIRST,
            command: UplinkCommand::Open(UplinkOpen {
                subscription_id: id.clone(),
                owner,
                reason,
                streams,
                ttl: SUBSCRIPTION_TTL,
            }),
        });
        Ok(Opened {
            subscription_id: id,
            grants,
        })
    }

    /// Closes a subscription with `uplink.close` at its generation. Its
    /// consumer gets [`EndCause::Closed`] at once; audio still in flight is
    /// dropped.
    pub fn close(
        &mut self,
        id: &SubscriptionId,
        reason: CloseReason,
        now: MonoInstant,
    ) -> Result<(), CommandError> {
        let at = self
            .subs
            .iter()
            .position(|s| &s.id == id)
            .ok_or(CommandError::UnknownSubscription)?;
        self.close_at(at, reason, EndCause::Closed(reason), now);
        Ok(())
    }

    fn close_at(&mut self, at: usize, reason: CloseReason, cause: EndCause, now: MonoInstant) {
        let sub = &self.subs[at];
        if matches!(sub.phase, Phase::Active { .. }) {
            self.commands.push(Outgoing {
                generation: sub.generation,
                command: UplinkCommand::Close(UplinkClose {
                    subscription_id: sub.id.clone(),
                    reason,
                }),
            });
            self.end_sub(at, cause, Leaves::Zombie, now);
        } else {
            self.end_sub(at, cause, Leaves::Gone, now);
        }
    }

    /// The device rejected a command about `id` sent at `generation`. A
    /// rejection of an older generation (a renewal overtaken by a
    /// conversion) changes nothing; otherwise the subscription is gone on
    /// the device and ends here too.
    pub fn command_rejected(
        &mut self,
        id: &SubscriptionId,
        generation: Generation,
        code: Option<String>,
        now: MonoInstant,
    ) {
        let Some(at) = self.subs.iter().position(|s| &s.id == id) else {
            return;
        };
        let sub = &self.subs[at];
        if generation < sub.generation || !matches!(sub.phase, Phase::Active { .. }) {
            return;
        }
        self.end_sub(at, EndCause::Rejected { code }, Leaves::Gone, now);
    }

    // ── Time ────────────────────────────────────────────────────────────

    /// Runs what is due at `now`: renewals, holes that waited too long, drains
    /// that ran out, and forgotten closed subscriptions.
    pub fn poll(&mut self, now: MonoInstant) {
        for sub in &mut self.subs {
            if let Phase::Active { next_renewal } = &mut sub.phase
                && *next_renewal <= now
            {
                *next_renewal = now.after(RENEW_INTERVAL);
                self.commands.push(Outgoing {
                    generation: sub.generation,
                    command: UplinkCommand::Renew(UplinkRenew {
                        subscription_id: sub.id.clone(),
                        ttl: SUBSCRIPTION_TTL,
                        conversion: None,
                    }),
                });
            }
        }
        self.release_all(now);
        let timeline = &mut self.timeline;
        self.zombies.retain(|z| {
            let keep = z.expires > now;
            if !keep {
                timeline.drop_claims(z.key);
            }
            keep
        });
        self.settle_subs(now);
    }

    /// When [`Uplink::poll`] next has something to do.
    pub fn next_deadline(&self) -> Option<MonoInstant> {
        let wait = self.config.hole_wait;
        let subs = self.subs.iter().flat_map(|s| {
            let phase = match &s.phase {
                Phase::Active { next_renewal } => *next_renewal,
                Phase::Draining { deadline, .. } => *deadline,
            };
            let stalls = s
                .wants
                .iter()
                .flat_map(|w| w.cursors.iter())
                .filter_map(move |c| c.stall_deadline(wait));
            std::iter::once(phase).chain(stalls)
        });
        subs.chain(self.zombies.iter().map(|z| z.expires)).min()
    }

    // ── Internals ───────────────────────────────────────────────────────

    fn check_open(&self, requested: Option<NonZeroU64>) -> Result<NonZeroU64, OpenError> {
        if self.muted {
            return Err(OpenError::Muted);
        }
        let current = self.timeline.capture_epoch();
        match (current, requested) {
            (None, None) => Err(OpenError::NoCaptureEpoch),
            (Some(current), None) => Ok(current),
            (current, Some(requested)) if current != Some(requested) => {
                Err(OpenError::StaleEpoch { current, requested })
            }
            (_, Some(requested)) => Ok(requested),
        }
    }

    fn others_want(&self, stream: StreamId, epoch: u64) -> bool {
        self.subs.iter().any(|s| s.wants_on(stream, epoch))
            || self
                .zombies
                .iter()
                .any(|z| z.streams.contains(&(stream, epoch)))
    }

    #[allow(clippy::too_many_arguments)]
    fn activate(
        &mut self,
        id: SubscriptionId,
        owner: Owner,
        reason: SubscriptionReason,
        capture: NonZeroU64,
        streams: &BTreeMap<StreamId, StreamStart>,
        near_miss: Option<NearMissPlan>,
        now: MonoInstant,
    ) -> Vec<Grant> {
        let key = SubKey(self.next_key);
        self.next_key += 1;
        let mut grants = Vec::new();
        let mut wants = Vec::new();
        for (&stream, &start) in streams {
            let mut want = Want {
                stream,
                cursors: Vec::new(),
            };
            let spec = match (start, device_grid_start(stream, sample_of(start))) {
                (StreamStart::Live, _) => StartSpec::Live,
                (StreamStart::At(_), Some(at)) => StartSpec::At(at.0),
                (StreamStart::At(_), None) => StartSpec::Mapped,
            };
            let epoch = match stream {
                StreamId::Reference => self.timeline.reference_epoch(),
                _ => Some(capture),
            };
            if spec != StartSpec::Live {
                grants.push(Grant {
                    stream,
                    grant: match stream {
                        StreamId::Reference => BackfillGrant::EveryOpenEpoch,
                        _ => BackfillGrant::Epoch(capture.get()),
                    },
                });
            }
            if let Some(epoch) = epoch {
                if spec != StartSpec::Live {
                    let fresh = !self.others_want(stream, epoch.get());
                    let floor = match spec {
                        StartSpec::At(at) => Some(at),
                        _ => None,
                    };
                    self.timeline
                        .ensure(stream, epoch)
                        .track
                        .claim(Claim { key, fresh, floor });
                }
                want.cursors.push(Cursor::new(epoch, spec));
            }
            wants.push(want);
        }
        self.subs.push(Sub {
            key,
            id,
            owner,
            reason,
            generation: Generation::FIRST,
            capture_epoch: capture,
            wants,
            phase: Phase::Active {
                next_renewal: now.after(RENEW_INTERVAL),
            },
            near_miss,
            overflowed: false,
        });
        grants
    }

    /// Releases every cursor with nothing new arriving: for holes whose wait
    /// ran out and drains.
    fn release_all(&mut self, now: MonoInstant) {
        let Self {
            timeline,
            subs,
            outbox,
            config,
            ..
        } = self;
        for sub in subs.iter_mut() {
            for want in &mut sub.wants {
                for cursor in &mut want.cursors {
                    let Some(held) = timeline.get(want.stream, cursor.epoch.get()) else {
                        continue;
                    };
                    let source = Source {
                        buffer: &held.buffer,
                        live_end: held.track.live_end(),
                        packet: None,
                        stream: want.stream,
                        epoch: held.epoch,
                    };
                    if !release(
                        cursor,
                        &source,
                        &sub.id,
                        sub.generation,
                        outbox,
                        config,
                        now,
                    ) {
                        sub.overflowed = true;
                    }
                }
            }
        }
    }

    /// Ends what is finished: overflowed consumers, completed near misses and
    /// drains that delivered everything or ran out.
    fn settle_subs(&mut self, now: MonoInstant) {
        let mut at = 0;
        while at < self.subs.len() {
            let sub = &self.subs[at];
            if sub.overflowed {
                self.close_at(at, CloseReason::Closed, EndCause::Overflow, now);
                continue;
            }
            if let Phase::Draining { report, deadline } = &sub.phase {
                let drained = sub.wants.iter().all(|w| w.cursors.iter().all(Cursor::done));
                if drained || *deadline <= now {
                    let cause = EndCause::Device(report.clone());
                    if !drained {
                        self.give_up_drain(at);
                    }
                    self.end_sub(at, cause, Leaves::Gone, now);
                    continue;
                }
            } else if sub.near_miss_done() {
                let cause = EndCause::Closed(CloseReason::Closed);
                self.close_at(at, CloseReason::Closed, cause, now);
                continue;
            }
            at += 1;
        }
    }

    /// A drain ran out: what the device said it sent and never arrived is
    /// declared missing.
    fn give_up_drain(&mut self, at: usize) {
        let sub = &mut self.subs[at];
        for want in &mut sub.wants {
            for cursor in &mut want.cursors {
                let (Some(pos), Some(target)) = (cursor.pos, cursor.target) else {
                    continue;
                };
                if pos >= target {
                    continue;
                }
                let chunk = Chunk {
                    subscription: sub.id.clone(),
                    generation: sub.generation,
                    stream: want.stream,
                    epoch: cursor.epoch,
                    span: Span {
                        start: pos,
                        end: target,
                    },
                    body: ChunkBody::Missing(MissingCause::TimedOut),
                };
                cursor.pos = Some(target);
                self.outbox.push(Delivery::Chunk(chunk));
            }
        }
    }

    fn end_all(&mut self, cause: EndCause) {
        for sub in std::mem::take(&mut self.subs) {
            self.timeline.drop_claims(sub.key);
            self.commands
                .retain(|c| c.command.subscription_id() != &sub.id);
            self.push_ended(&sub, cause.clone());
        }
        for zombie in std::mem::take(&mut self.zombies) {
            self.timeline.drop_claims(zombie.key);
        }
    }

    fn end_sub(&mut self, at: usize, cause: EndCause, leaves: Leaves, now: MonoInstant) {
        let sub = self.subs.remove(at);
        match leaves {
            Leaves::Zombie => {
                let streams = sub
                    .wants
                    .iter()
                    .flat_map(|w| w.cursors.iter().map(move |c| (w.stream, c.epoch.get())))
                    .collect();
                self.zombies.push(Zombie {
                    key: sub.key,
                    id: sub.id.clone(),
                    streams,
                    expires: now.after(SUBSCRIPTION_TTL + self.config.hole_wait),
                });
            }
            Leaves::Gone => {
                self.timeline.drop_claims(sub.key);
                self.commands
                    .retain(|c| c.command.subscription_id() != &sub.id);
            }
        }
        self.push_ended(&sub, cause);
    }

    fn push_ended(&mut self, sub: &Sub, cause: EndCause) {
        self.outbox.push(Delivery::Ended(Ended {
            subscription: sub.id.clone(),
            owner: sub.owner.clone(),
            reason: sub.reason,
            generation: sub.generation,
            cause,
        }));
    }
}

/// What an ended subscription leaves behind on the device side.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Leaves {
    /// The device may still be sending it until its `uplink.ended`.
    Zombie,
    /// The device has ended it too, or never had it.
    Gone,
}

fn sample_of(start: StreamStart) -> Sample {
    match start {
        StreamStart::At(sample) => sample,
        StreamStart::Live => Sample(0),
    }
}

#[cfg(test)]
mod tests;
