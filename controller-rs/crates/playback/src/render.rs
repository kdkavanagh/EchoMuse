//! The render client: one per device session, owned by the device runtime.
//!
//! It follows spec §3.1.7 and §5.2:
//!
//! 1. A network playback gets a random nonzero epoch and a `render.start`.
//! 2. No frame is queued until the device acknowledges that start.
//! 3. Audio goes out in 3,840-frame packets from frame 0, at most
//!    [`MAX_FRAMES_AHEAD`] frames past the device's reported progress.
//! 4. Once the source is exhausted, `render.end` names the frame after the last
//!    one sent.
//!
//! Each source class has its own generation counter, which only grows, so a
//! playback never starts at a generation the device would call stale within a
//! session. Starting a playback in a class with a live one cancels the old one
//! first. Cancelling cancels the playback's token, so the link drops its
//! queued packets unwritten; a packet already being written may still reach
//! the device.
//!
//! Dialog output may carry [`ResponseLimits`] (spec §4.8): its audio must start
//! by a deadline (10 s after the answer is known), may go at most 2 s without
//! progress, and may last at most 120 s. Exceeding one cancels the playback and
//! reports [`ResponseLimit`].
//!
//! The client never waits and holds no clock: every call that depends on time
//! takes `now`, and [`RenderClient::next_deadline`] says when to call
//! [`RenderClient::poll`] next. Outcomes queue up as [`RenderEvent`]s.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use echomuse_protocol::ema::{self, Flags};
use echomuse_protocol::{Capabilities, Capability, MessageId, Reported};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::link::{FrameRefused, LinkClosed, RenderFrame, RenderLink};
use crate::wire::{
    AckFailure, AckOutcome, CancelReason, Command, Earcon, FORMAT_PCM16, FinishReason, LocalSource,
    NetworkClass, PlaybackId, ProgressEvent, RenderCancel, RenderEnd, RenderEpoch, RenderProgress,
    RenderRejection, RenderReport, RenderStart, SourceClass, reported,
};

/// Frames per render packet, the most kind 3 allows.
pub const PACKET_FRAMES: usize = 3_840;

/// The most frames sent past the device's reported progress: its source FIFO
/// holds 128 × 2,048 frames, less one packet.
pub const MAX_FRAMES_AHEAD: u64 = 258_304;

/// How much decoded audio a playback buffers before it is sent: about one
/// second, enough to fill the window as soon as the start is acknowledged.
pub const PREFETCH_FRAMES: usize = 12 * PACKET_FRAMES;

/// How long to wait before offering a frame again after the link's queue
/// was full.
pub const FRAME_RETRY: Duration = Duration::from_millis(20);

/// Spec §4.8: an answer's audio must start within this of the answer being
/// known.
pub const ANSWER_START_LIMIT: Duration = Duration::from_secs(10);

/// Spec §4.8: the longest a started answer may go without playback progress.
pub const STALL_LIMIT: Duration = Duration::from_secs(2);

/// Spec §4.8: the longest one spoken answer may play.
pub const ANSWER_LENGTH_LIMIT: Duration = Duration::from_secs(120);

/// The response limits of one spoken answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponseLimits {
    /// The audio must have started by this instant.
    pub start_deadline: Instant,
}

impl ResponseLimits {
    /// Limits for an answer known at `answer_ready`: its audio must start
    /// within [`ANSWER_START_LIMIT`].
    pub fn from_answer_ready(answer_ready: Instant) -> Self {
        Self {
            start_deadline: answer_ready + ANSWER_START_LIMIT,
        }
    }
}

/// Which response limit was exceeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseLimit {
    /// The audio did not start by its deadline.
    Start,
    /// No progress for [`STALL_LIMIT`] after it started.
    Stall,
    /// It played for [`ANSWER_LENGTH_LIMIT`].
    Length,
}

/// A playback this client started.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PlaybackRef {
    id: PlaybackId,
    class: SourceClass,
    generation: u32,
}

impl PlaybackRef {
    /// Its `playback_id`.
    pub fn id(&self) -> &PlaybackId {
        &self.id
    }

    /// Its generation, which every envelope about it carries.
    pub fn generation(&self) -> u32 {
        self.generation
    }
}

/// A network playback to start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NetworkStart {
    /// `content` or `dialog_output`.
    pub class: NetworkClass,
    /// The user-selected gain in dB; device ducking is relative to it.
    pub gain_db: f64,
    /// Whether it is a Home Assistant announcement.
    pub announcement: bool,
    /// The response limits of a spoken answer, if it is one.
    pub limits: Option<ResponseLimits>,
}

/// Why a local playback was not started.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LocalStartError {
    /// The device did not announce the capability the asset needs.
    #[error("the device lacks {0}")]
    Unsupported(Capability),
    /// The session has ended.
    #[error(transparent)]
    LinkClosed(#[from] LinkClosed),
}

/// Why audio was not accepted for a playback.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AudioRefused {
    /// The client has no such live network playback, or it is cancelling.
    #[error("the playback is not accepting audio")]
    NotAccepting,
    /// The source was already marked complete.
    #[error("the source already ended")]
    SourceEnded,
}

/// The playback is not live in this client.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("no such live playback")]
pub struct NotLive;

/// How a playback ended. Every playback ends exactly once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaybackEnd {
    /// The device reported `render.finished`.
    Finished {
        /// Why.
        reason: Reported<FinishReason>,
        /// One past the last source frame whose output completed.
        last_completed_frame: u64,
    },
    /// The device rejected the `render.start`; `None` when it gave no code.
    Rejected(Option<Reported<RenderRejection>>),
    /// The `render.start` was never acknowledged. A `render.cancel` was sent
    /// in case the device started it anyway.
    Unconfirmed,
    /// The session ended before the device reported an end.
    LinkClosed,
}

/// Something that happened to a playback.
#[derive(Clone, Debug, PartialEq)]
pub enum RenderEvent {
    /// The device accepted the `render.start`.
    Accepted(PlaybackRef),
    /// The device reported the `start` event: the audio became audible.
    Started(PlaybackRef),
    /// Any `render.progress` for the playback, `start` included.
    Progress {
        /// The playback.
        playback: PlaybackRef,
        /// The report.
        progress: RenderProgress,
    },
    /// The client cancelled the playback.
    Cancelled {
        /// The playback.
        playback: PlaybackRef,
        /// Why.
        reason: CancelReason,
    },
    /// A response limit was exceeded; the playback is cancelled.
    LimitExceeded {
        /// The playback.
        playback: PlaybackRef,
        /// Which limit.
        limit: ResponseLimit,
    },
    /// The playback is over.
    Ended {
        /// The playback.
        playback: PlaybackRef,
        /// How.
        end: PlaybackEnd,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    AwaitingAck,
    Running,
    Cancelling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandKind {
    Start,
    End,
    Cancel,
}

struct Network {
    epoch: RenderEpoch,
    token: CancellationToken,
    /// Decoded audio not yet packetized.
    pending: VecDeque<i16>,
    /// A packet the link's queue had no room for.
    blocked: Option<RenderFrame>,
    retry_at: Option<Instant>,
    /// One past the last frame packetized.
    next_frame: u64,
    next_sequence: u64,
    /// The device's latest `completed_frames`.
    completed: u64,
    source_done: bool,
    end_sent: bool,
}

struct LimitClock {
    start_deadline: Instant,
    started_at: Option<Instant>,
    last_advance: Option<Instant>,
}

impl LimitClock {
    fn deadline(&self) -> (Instant, ResponseLimit) {
        match (self.started_at, self.last_advance) {
            (Some(started), Some(advanced)) => {
                let stall = advanced + STALL_LIMIT;
                let length = started + ANSWER_LENGTH_LIMIT;
                if stall <= length {
                    (stall, ResponseLimit::Stall)
                } else {
                    (length, ResponseLimit::Length)
                }
            }
            _ => (self.start_deadline, ResponseLimit::Start),
        }
    }
}

struct Entry {
    playback: PlaybackRef,
    phase: Phase,
    network: Option<Network>,
    limits: Option<LimitClock>,
}

/// The device's playbacks as the controller drives them. See the module
/// documentation.
pub struct RenderClient {
    no_input_chime: bool,
    announcement_chime: bool,
    generations: [u32; 4],
    entries: Vec<Entry>,
    commands: HashMap<MessageId, (PlaybackId, CommandKind)>,
    events: VecDeque<RenderEvent>,
}

impl RenderClient {
    /// A client for a session whose hello announced `capabilities`.
    pub fn new(capabilities: &Capabilities) -> Self {
        Self {
            no_input_chime: capabilities.contains(Capability::NoInputChime),
            announcement_chime: capabilities.contains(Capability::AnnouncementChime),
            generations: [0; 4],
            entries: Vec::new(),
            commands: HashMap::new(),
            events: VecDeque::new(),
        }
    }

    /// Whether the device can play `source`; `Err` names the capability it
    /// lacks.
    pub fn supports(&self, source: &LocalSource) -> Result<(), Capability> {
        match source {
            LocalSource::Earcon(Earcon::NoInputChime) if !self.no_input_chime => {
                Err(Capability::NoInputChime)
            }
            LocalSource::Earcon(Earcon::AnnouncementChime) if !self.announcement_chime => {
                Err(Capability::AnnouncementChime)
            }
            _ => Ok(()),
        }
    }

    /// Starts a network playback: sends `render.start` with a fresh epoch at
    /// the class's next generation, after cancelling the class's live
    /// playback. Audio offered with [`push_audio`](Self::push_audio) is held
    /// until the device accepts the start.
    pub fn start_network<L: RenderLink>(
        &mut self,
        link: &mut L,
        start: NetworkStart,
    ) -> Result<PlaybackRef, LinkClosed> {
        let class = SourceClass::from(start.class);
        self.supersede(link, class);
        let epoch = self.fresh_epoch();
        let playback = self.next_playback(class);
        let body = RenderStart {
            playback_id: playback.id.clone(),
            source_class: class,
            epoch: Some(epoch),
            gain_db: start.gain_db,
            format: FORMAT_PCM16,
            local_asset: None,
            announcement: start.announcement,
        };
        let id = link.request(&Command::RenderStart(body), playback.generation)?;
        self.commands
            .insert(id, (playback.id.clone(), CommandKind::Start));
        self.entries.push(Entry {
            playback: playback.clone(),
            phase: Phase::AwaitingAck,
            network: Some(Network {
                epoch,
                token: CancellationToken::new(),
                pending: VecDeque::new(),
                blocked: None,
                retry_at: None,
                next_frame: 0,
                next_sequence: 0,
                completed: 0,
                source_done: false,
                end_sent: false,
            }),
            limits: start.limits.map(|limits| LimitClock {
                start_deadline: limits.start_deadline,
                started_at: None,
                last_advance: None,
            }),
        });
        Ok(playback)
    }

    /// Starts a local playback of a built-in earcon or an alert sound, after
    /// cancelling the class's live playback. An earcon the device did not
    /// announce is refused without sending anything.
    pub fn start_local<L: RenderLink>(
        &mut self,
        link: &mut L,
        source: &LocalSource,
        gain_db: f64,
        announcement: bool,
    ) -> Result<PlaybackRef, LocalStartError> {
        self.supports(source)
            .map_err(LocalStartError::Unsupported)?;
        let class = source.class();
        self.supersede(link, class);
        let playback = self.next_playback(class);
        let body = RenderStart {
            playback_id: playback.id.clone(),
            source_class: class,
            epoch: None,
            gain_db,
            format: FORMAT_PCM16,
            local_asset: Some(source.asset()),
            announcement,
        };
        let id = link.request(&Command::RenderStart(body), playback.generation)?;
        self.commands
            .insert(id, (playback.id.clone(), CommandKind::Start));
        self.entries.push(Entry {
            playback: playback.clone(),
            phase: Phase::AwaitingAck,
            network: None,
            limits: None,
        });
        Ok(playback)
    }

    /// Whether the playback takes more decoded audio now. While it does not,
    /// the runtime leaves the decoder's output queued, which in turn holds
    /// back the decoder and the fetch.
    pub fn wants_audio(&self, playback: &PlaybackRef) -> bool {
        self.index(playback).is_some_and(|index| {
            let entry = &self.entries[index];
            entry.phase != Phase::Cancelling
                && entry
                    .network
                    .as_ref()
                    .is_some_and(|net| !net.source_done && net.pending.len() < PREFETCH_FRAMES)
        })
    }

    /// Appends decoded 48 kHz mono audio to a network playback and sends what
    /// the window allows. Audio is accepted even past
    /// [`wants_audio`](Self::wants_audio) turning false, so a decoder's chunk
    /// is never split; the runtime stops reading once it is false.
    pub fn push_audio<L: RenderLink>(
        &mut self,
        link: &mut L,
        now: Instant,
        playback: &PlaybackRef,
        pcm: &[i16],
    ) -> Result<(), AudioRefused> {
        let index = self.index(playback).ok_or(AudioRefused::NotAccepting)?;
        let entry = &mut self.entries[index];
        if entry.phase == Phase::Cancelling {
            return Err(AudioRefused::NotAccepting);
        }
        let net = entry.network.as_mut().ok_or(AudioRefused::NotAccepting)?;
        if net.source_done {
            return Err(AudioRefused::SourceEnded);
        }
        net.pending.extend(pcm.iter().copied());
        self.pump(link, now, index);
        Ok(())
    }

    /// Marks a network playback's source complete. Once every frame has been
    /// handed to the link, `render.end` names the frame after the last.
    pub fn finish_audio<L: RenderLink>(
        &mut self,
        link: &mut L,
        now: Instant,
        playback: &PlaybackRef,
    ) -> Result<(), AudioRefused> {
        let index = self.index(playback).ok_or(AudioRefused::NotAccepting)?;
        let entry = &mut self.entries[index];
        if entry.phase == Phase::Cancelling {
            return Err(AudioRefused::NotAccepting);
        }
        let net = entry.network.as_mut().ok_or(AudioRefused::NotAccepting)?;
        if net.source_done {
            return Err(AudioRefused::SourceEnded);
        }
        net.source_done = true;
        self.pump(link, now, index);
        Ok(())
    }

    /// Cancels a playback: purges its queued packets, sends `render.cancel`
    /// and reports [`RenderEvent::Cancelled`]. The playback ends when the
    /// device reports it finished.
    pub fn cancel<L: RenderLink>(
        &mut self,
        link: &mut L,
        playback: &PlaybackRef,
        reason: CancelReason,
    ) -> Result<(), NotLive> {
        let index = self.index(playback).ok_or(NotLive)?;
        if self.entries[index].phase == Phase::Cancelling {
            return Ok(());
        }
        self.cancel_at(link, index, reason);
        Ok(())
    }

    /// Hands the client a `command.ack` outcome. Returns whether the command
    /// was one of this client's.
    pub fn on_ack<L: RenderLink>(
        &mut self,
        link: &mut L,
        now: Instant,
        message_id: &MessageId,
        outcome: AckOutcome,
    ) -> bool {
        let Some((id, kind)) = self.commands.remove(message_id) else {
            return false;
        };
        // An acknowledgement for a playback that has already ended changes
        // nothing.
        let Some(index) = self.entries.iter().position(|e| e.playback.id == id) else {
            return true;
        };
        match (kind, outcome) {
            (CommandKind::Start, AckOutcome::Accepted) => {
                let entry = &mut self.entries[index];
                if entry.phase == Phase::AwaitingAck {
                    entry.phase = Phase::Running;
                    self.events
                        .push_back(RenderEvent::Accepted(entry.playback.clone()));
                    self.pump(link, now, index);
                }
            }
            (CommandKind::Start, AckOutcome::Rejected { code }) => {
                let code = code.map(|code| reported(&code, RenderRejection::from_wire));
                self.end_at(index, PlaybackEnd::Rejected(code));
            }
            (CommandKind::Start, AckOutcome::Failed(AckFailure::TimedOut)) => {
                if self.entries[index].phase != Phase::Cancelling {
                    self.cancel_at(link, index, CancelReason::Unconfirmed);
                }
                if let Some(index) = self.entries.iter().position(|e| e.playback.id == id) {
                    self.end_at(index, PlaybackEnd::Unconfirmed);
                }
            }
            (_, AckOutcome::Failed(AckFailure::SessionEnded)) => {
                self.end_at(index, PlaybackEnd::LinkClosed);
            }
            // render.end and render.cancel acknowledgements change nothing:
            // the device's render.finished decides how the playback ended.
            (CommandKind::End | CommandKind::Cancel, _) => {}
        }
        true
    }

    /// Hands the client a `render.progress` or `render.finished` whose
    /// envelope carried `generation`. Returns whether it was about one of this
    /// client's live playbacks; a report for another generation is stale and
    /// ignored.
    pub fn on_report<L: RenderLink>(
        &mut self,
        link: &mut L,
        now: Instant,
        generation: u32,
        report: RenderReport,
    ) -> bool {
        let Some(index) = self.entries.iter().position(|e| {
            &e.playback.id == report.playback_id() && e.playback.generation == generation
        }) else {
            return false;
        };
        match report {
            RenderReport::Progress(progress) => {
                let entry = &mut self.entries[index];
                let mut advanced = false;
                if let Some(net) = entry.network.as_mut()
                    && progress.completed_frames > net.completed
                {
                    net.completed = progress.completed_frames;
                    advanced = true;
                }
                let start = progress.event == Reported::Known(ProgressEvent::Start);
                if let Some(limits) = entry.limits.as_mut() {
                    if (start || advanced) && limits.started_at.is_none() {
                        limits.started_at = Some(now);
                        limits.last_advance = Some(now);
                    }
                    if advanced {
                        limits.last_advance = Some(now);
                    }
                }
                let playback = entry.playback.clone();
                if start {
                    self.events
                        .push_back(RenderEvent::Started(playback.clone()));
                }
                self.events
                    .push_back(RenderEvent::Progress { playback, progress });
                self.pump(link, now, index);
            }
            RenderReport::Finished(finished) => {
                self.end_at(
                    index,
                    PlaybackEnd::Finished {
                        reason: finished.reason,
                        last_completed_frame: finished.last_completed_frame,
                    },
                );
            }
        }
        true
    }

    /// Retries frames the link had no room for and enforces response limits.
    /// Call it at [`next_deadline`](Self::next_deadline).
    pub fn poll<L: RenderLink>(&mut self, link: &mut L, now: Instant) {
        let ids: Vec<PlaybackId> = self.entries.iter().map(|e| e.playback.id.clone()).collect();
        for id in ids {
            let Some(index) = self.entries.iter().position(|e| e.playback.id == id) else {
                continue;
            };
            let retry_due = self.entries[index]
                .network
                .as_ref()
                .and_then(|net| net.retry_at)
                .is_some_and(|at| at <= now);
            if retry_due {
                self.pump(link, now, index);
            }
            let Some(index) = self.entries.iter().position(|e| e.playback.id == id) else {
                continue;
            };
            let entry = &self.entries[index];
            if entry.phase == Phase::Cancelling {
                continue;
            }
            let Some((deadline, limit)) = entry.limits.as_ref().map(LimitClock::deadline) else {
                continue;
            };
            if deadline <= now {
                self.events.push_back(RenderEvent::LimitExceeded {
                    playback: entry.playback.clone(),
                    limit,
                });
                self.cancel_at(link, index, CancelReason::ResponseTimeout);
            }
        }
    }

    /// The next instant [`poll`](Self::poll) has work, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries
            .iter()
            .flat_map(|entry| {
                let retry = entry.network.as_ref().and_then(|net| net.retry_at);
                let limit = (entry.phase != Phase::Cancelling)
                    .then(|| entry.limits.as_ref().map(|l| l.deadline().0))
                    .flatten();
                [retry, limit]
            })
            .flatten()
            .min()
    }

    /// Ends every playback because the session has ended. Their queued frames
    /// are dropped with the session.
    pub fn session_ended(&mut self) {
        for entry in self.entries.drain(..) {
            if let Some(net) = &entry.network {
                net.token.cancel();
            }
            self.events.push_back(RenderEvent::Ended {
                playback: entry.playback,
                end: PlaybackEnd::LinkClosed,
            });
        }
        self.commands.clear();
    }

    /// The next queued event.
    pub fn next_event(&mut self) -> Option<RenderEvent> {
        self.events.pop_front()
    }

    fn index(&self, playback: &PlaybackRef) -> Option<usize> {
        self.entries.iter().position(|e| &e.playback == playback)
    }

    fn next_playback(&mut self, class: SourceClass) -> PlaybackRef {
        let generation = &mut self.generations[class.slot()];
        *generation += 1;
        PlaybackRef {
            id: PlaybackId::generate(),
            class,
            generation: *generation,
        }
    }

    fn fresh_epoch(&self) -> RenderEpoch {
        loop {
            let epoch = RenderEpoch::random();
            let taken = self
                .entries
                .iter()
                .any(|e| e.network.as_ref().is_some_and(|net| net.epoch == epoch));
            if !taken {
                return epoch;
            }
        }
    }

    /// Cancels the class's live playback before a new one starts, so its
    /// queued packets go and its end is reported even if the new start is
    /// refused.
    fn supersede<L: RenderLink>(&mut self, link: &mut L, class: SourceClass) {
        while let Some(index) = self
            .entries
            .iter()
            .position(|e| e.playback.class == class && e.phase != Phase::Cancelling)
        {
            self.cancel_at(link, index, CancelReason::Superseded);
        }
    }

    fn cancel_at<L: RenderLink>(&mut self, link: &mut L, index: usize, reason: CancelReason) {
        let entry = &mut self.entries[index];
        entry.phase = Phase::Cancelling;
        entry.limits = None;
        if let Some(net) = entry.network.as_mut() {
            net.token.cancel();
            net.pending.clear();
            net.blocked = None;
            net.retry_at = None;
        }
        let playback = entry.playback.clone();
        self.events.push_back(RenderEvent::Cancelled {
            playback: playback.clone(),
            reason,
        });
        let body = RenderCancel {
            playback_id: playback.id.clone(),
            reason,
        };
        match link.request(&Command::RenderCancel(body), playback.generation) {
            Ok(id) => {
                self.commands.insert(id, (playback.id, CommandKind::Cancel));
            }
            Err(LinkClosed) => self.end_at(index, PlaybackEnd::LinkClosed),
        }
    }

    fn end_at(&mut self, index: usize, end: PlaybackEnd) {
        let entry = self.entries.remove(index);
        if let Some(net) = &entry.network {
            net.token.cancel();
        }
        self.events.push_back(RenderEvent::Ended {
            playback: entry.playback,
            end,
        });
    }

    /// Sends what the window and the link allow, then `render.end` once the
    /// source is exhausted and everything is handed over.
    fn pump<L: RenderLink>(&mut self, link: &mut L, now: Instant, index: usize) {
        let entry = &mut self.entries[index];
        if entry.phase != Phase::Running {
            return;
        }
        let generation = entry.playback.generation;
        let Some(net) = entry.network.as_mut() else {
            return;
        };
        let mut closed = false;
        loop {
            let frame = match net.blocked.take() {
                Some(frame) => frame,
                None => {
                    let frames = if net.pending.len() >= PACKET_FRAMES {
                        PACKET_FRAMES
                    } else if net.source_done && !net.pending.is_empty() {
                        net.pending.len()
                    } else {
                        break;
                    };
                    if net.next_frame + frames as u64 > net.completed + MAX_FRAMES_AHEAD {
                        break;
                    }
                    let pcm: Vec<i16> = net.pending.drain(..frames).collect();
                    let bytes = ema::encode_render(
                        net.epoch.get(),
                        net.next_sequence,
                        net.next_frame,
                        generation,
                        Flags::NONE,
                        &pcm,
                    )
                    .expect("a packet of at most 3,840 frames always encodes");
                    let frame = RenderFrame {
                        bytes,
                        playback: net.token.clone(),
                        sequence: net.next_sequence,
                        first_frame: net.next_frame,
                        frames: frames as u32,
                    };
                    net.next_frame += frames as u64;
                    net.next_sequence += 1;
                    frame
                }
            };
            match link.queue_frame(frame) {
                Ok(()) => {
                    net.retry_at = None;
                }
                Err(FrameRefused::Full(frame)) => {
                    net.blocked = Some(frame);
                    net.retry_at = Some(now + FRAME_RETRY);
                    break;
                }
                Err(FrameRefused::Closed) => {
                    closed = true;
                    break;
                }
            }
        }
        if !closed
            && net.source_done
            && !net.end_sent
            && net.pending.is_empty()
            && net.blocked.is_none()
        {
            let body = RenderEnd {
                playback_id: entry.playback.id.clone(),
                end_frame: net.next_frame,
            };
            match link.request(&Command::RenderEnd(body), generation) {
                Ok(id) => {
                    net.end_sent = true;
                    self.commands
                        .insert(id, (entry.playback.id.clone(), CommandKind::End));
                }
                Err(LinkClosed) => closed = true,
            }
        }
        if closed {
            self.end_at(index, PlaybackEnd::LinkClosed);
        }
    }
}

#[cfg(test)]
mod tests;
