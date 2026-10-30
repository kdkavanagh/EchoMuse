//! What the device plays (spec §3.1.7, §4.5, §4.8, §4.9): spoken answers and lines,
//! earcons, Home Assistant's announcements and the media stream.
//!
//! A network playback is fetched and decoded by the runtime (ffmpeg) one chunk at a
//! time, as the render client has room: the machine pulls the next chunk only while the
//! client wants audio, so a long answer never sits in memory. Spoken answers and
//! announcements play as `dialog_output` under a focus lease (the device ducks content
//! and backgrounds alerts); answers carry the response limits; content plays through the
//! equalizer. Announcements queue behind the device's own replies (the media player
//! decides when each may start).
//!
//! [`Output`] owns the render client, the media player, the playbacks with their decodes
//! and focus leases, the alert sound preview, earcons and announcements. It knows no turn:
//! the conversation asks it to play or stop an answer by the turn's identity, and what a
//! render event means for the conversation goes back as [`Notices`] carrying identities,
//! which the machine hands on once this call has returned.

use echomuse_esphome::voice::{Announcement, MediaRequest};
use echomuse_playback::{
    AckOutcome, AnnouncementKey, CancelReason, ContentEnd, ContentEndReason, DecodeEvent, Earcon,
    EqSettings, FocusKind, FocusLease, FocusOwner, LocalSource, MediaEffect, MediaPlayer,
    NetworkClass, NetworkStart, Opener, PlaybackEnd, PlaybackRef, RenderClient, RenderEvent,
    RenderReport, ResponseLimits, SatelliteAnnouncement, StreamId as MediaStreamId,
    wire::{FinishReason, RenderRejection},
};
use echomuse_protocol::{Capabilities, MessageId, Reported};
use echomuse_records::vocab::PlaybackReason;
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    io::{CallId, DecodeCommand, DecodeReport, Effect, Now, Outbound, SatelliteCommand, VolumeSet},
    session::OutputCx,
};

/// The alert sound preview playing, and the request that started it.
#[derive(Debug)]
struct Preview {
    id: crate::io::PreviewId,
    playback: PlaybackRef,
    /// Its start was answered (accepted, refused or unknown).
    answered: bool,
}

/// What a playback is for.
#[derive(Clone, Debug)]
enum Purpose {
    /// A turn's spoken answer or line.
    Answer {
        turn: Uuid,
    },
    /// An announcement's opener or its message.
    Announcement {
        key: AnnouncementKey,
        /// The message still to play after this opener; `None` for the message itself.
        then: Option<String>,
        start_conversation: bool,
    },
    Content {
        stream: MediaStreamId,
    },
    Earcon,
}

#[derive(Debug)]
struct Playout {
    purpose: Purpose,
    /// The decode feeding a network playback.
    call: Option<CallId>,
    url: Option<String>,
    playback: Option<PlaybackRef>,
    /// A pull is outstanding.
    pulling: bool,
    /// Pulling stopped because the render client had no room.
    waiting_room: bool,
    pushed: bool,
    source_done: bool,
    focus: Option<FocusLease>,
    audible: bool,
    limit_passed: bool,
    cancelled: Option<CancelReason>,
}

impl Playout {
    /// A playback the device plays from its own assets: nothing to fetch or decode.
    fn local(purpose: Purpose, playback: PlaybackRef) -> Self {
        Self {
            purpose,
            call: None,
            url: None,
            playback: Some(playback),
            pulling: false,
            waiting_room: false,
            pushed: false,
            source_done: true,
            focus: None,
            audible: false,
            limit_passed: false,
            cancelled: None,
        }
    }

    fn cancel(&mut self, render: &mut RenderClient, cx: &mut OutputCx<'_>, reason: CancelReason) {
        if self.cancelled.is_some() {
            return;
        }
        self.cancelled = Some(reason);
        if let Some(call) = self.call {
            cx.link
                .effects
                .push(Effect::Decode(DecodeCommand::Cancel(call)));
            self.pulling = false;
        }
        if let Some(playback) = &self.playback {
            let _ = render.cancel(cx.link, playback, reason);
        }
    }
}

/// What output is doing, for a command's context and for arbitration.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputSummary {
    /// An answer of this device's plays (and was not cancelled).
    pub(crate) answer_playing: bool,
    /// The media player plays.
    pub(crate) media_playing: bool,
}

/// An answer became audible.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Audible {
    pub(crate) turn: Uuid,
    /// It was already cancelled when it became audible.
    pub(crate) cancelled: bool,
}

/// An answer ended.
#[derive(Clone, Debug)]
pub(crate) struct AnswerEnded {
    pub(crate) turn: Uuid,
    pub(crate) reason: PlaybackReason,
    /// The answer's URL when it played to its end.
    pub(crate) completed_url: Option<String>,
}

/// What one render event means for the conversation, in the order it applies. Each
/// names the turn it is about; the conversation checks it before acting.
#[derive(Debug, Default)]
pub(crate) struct Notices {
    /// An answer's first frame played at this device time (`None` when the report's time
    /// cannot place it).
    pub(crate) first_frame: Option<(Uuid, Option<u64>)>,
    pub(crate) audible: Option<Audible>,
    pub(crate) ended: Option<AnswerEnded>,
    /// A completed announcement asks for a conversation HA started.
    pub(crate) ha_conversation: bool,
}

/// Work a render event leaves for after its notices were handled: pulling the next chunk
/// of a playback the device made room for.
#[derive(Debug)]
pub(crate) struct Refill(Option<PlaybackRef>);

/// The device's output. See the module documentation.
pub(crate) struct Output {
    render: RenderClient,
    media: MediaPlayer,
    playouts: Vec<Playout>,
    preview: Option<Preview>,
}

fn eq_settings(settings: &echomuse_settings::DeviceSettings) -> Option<EqSettings> {
    let bands = settings.playback.eq_bands.db().map(|db| db.round() as i8);
    EqSettings::new(bands, settings.playback.eq_loudness)
        .ok()
        .filter(|eq| !eq.is_flat())
}

impl Output {
    pub(crate) fn new(
        capabilities: &Capabilities,
        volume: Option<echomuse_esphome::entities::DeviceVolume>,
    ) -> Self {
        Self {
            render: RenderClient::new(capabilities),
            media: MediaPlayer::new(capabilities, volume),
            playouts: Vec::new(),
            preview: None,
        }
    }

    // ---- summaries ------------------------------------------------------------------

    pub(crate) fn summary(&self) -> OutputSummary {
        OutputSummary {
            answer_playing: self
                .playouts
                .iter()
                .any(|p| matches!(p.purpose, Purpose::Answer { .. }) && p.cancelled.is_none()),
            media_playing: self.media.state().playback
                == echomuse_esphome::entities::Playback::Playing,
        }
    }

    /// Dialog output is audible (an answer or an announcement).
    pub(crate) fn speaking(&self) -> bool {
        self.playouts.iter().any(|p| {
            p.audible
                && p.cancelled.is_none()
                && matches!(
                    p.purpose,
                    Purpose::Answer { .. } | Purpose::Announcement { .. }
                )
        })
    }

    /// When the render client or a playback's focus lease next needs polling.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.playouts
            .iter()
            .filter_map(|p| p.focus.as_ref())
            .filter_map(FocusLease::next_deadline)
            .chain(self.render.next_deadline())
            .min()
    }

    // ---- what the conversation asks -------------------------------------------------

    /// Plays turn `turn`'s answer from `url` (HA's TTS, or a line's).
    pub(crate) fn play_answer(&mut self, cx: &mut OutputCx<'_>, turn: Uuid, url: String, now: Now) {
        self.play_url(cx, url, Purpose::Answer { turn }, now);
    }

    /// The device's own answers stop when a new turn begins.
    pub(crate) fn interrupt_answers(&mut self, cx: &mut OutputCx<'_>) {
        for playout in &mut self.playouts {
            if matches!(playout.purpose, Purpose::Answer { .. }) {
                playout.cancel(&mut self.render, cx, CancelReason::Interrupted);
            }
        }
    }

    /// Stops what turn `turn` still plays when it ends some other way.
    pub(crate) fn end_answers_of(&mut self, cx: &mut OutputCx<'_>, turn: Uuid) {
        for playout in &mut self.playouts {
            if matches!(playout.purpose, Purpose::Answer { turn: t } if t == turn) {
                playout.cancel(&mut self.render, cx, CancelReason::Interrupted);
            }
        }
    }

    pub(crate) fn earcon(&mut self, cx: &mut OutputCx<'_>, earcon: Earcon) {
        let source = LocalSource::Earcon(earcon);
        if self.render.supports(&source).is_err() {
            return;
        }
        if let Ok(playback) = self.render.start_local(cx.link, &source, 0.0, false) {
            self.playouts
                .push(Playout::local(Purpose::Earcon, playback));
        }
    }

    /// A dialog began: the media player ducks or pauses as it decides.
    pub(crate) fn dialog_started(&mut self) {
        self.media.dialog_started();
    }

    /// The dialog ended: the media player applies what it held back.
    pub(crate) fn dialog_ended(&mut self, cx: &mut OutputCx<'_>, now: Now) {
        self.media.dialog_ended();
        self.apply_media_effects(cx, now);
    }

    /// A media command spoken to this device (during the dialog the player records the
    /// intent and applies it when the dialog ends, §4.9).
    pub(crate) fn media_command(&mut self, cx: &mut OutputCx<'_>, request: MediaRequest, now: Now) {
        let _ = self.media.command(request);
        self.apply_media_effects(cx, now);
    }

    // ---- what the machine hands on --------------------------------------------------

    /// A media request Home Assistant sent the satellite; its effects follow with
    /// [`Self::apply_media_effects`].
    pub(crate) fn satellite_media(&mut self, request: MediaRequest) {
        let _ = self.media.command(request);
    }

    /// An announcement Home Assistant sent the satellite; it starts with
    /// [`Self::apply_media_effects`] when the player allows.
    pub(crate) fn announce(&mut self, announcement: Announcement) {
        let _: AnnouncementKey = self
            .media
            .announce(SatelliteAnnouncement::from(announcement));
    }

    pub(crate) fn device_volume(
        &mut self,
        volume: Option<echomuse_esphome::entities::DeviceVolume>,
    ) {
        self.media.device_volume(volume);
    }

    pub(crate) fn render_report(
        &mut self,
        cx: &mut OutputCx<'_>,
        generation: u32,
        report: RenderReport,
        now: Now,
    ) {
        self.render.on_report(cx.link, now.at, generation, report);
    }

    /// A `command.ack` for the render client; `false` when it is not one of its commands.
    pub(crate) fn render_ack(
        &mut self,
        cx: &mut OutputCx<'_>,
        id: &MessageId,
        outcome: AckOutcome,
        now: Now,
    ) -> bool {
        self.render.on_ack(cx.link, now.at, id, outcome)
    }

    /// A `command.ack` for a live playback's focus lease.
    pub(crate) fn focus_ack(&mut self, id: &MessageId, outcome: &AckOutcome) -> bool {
        for lease in self.playouts.iter_mut().filter_map(|p| p.focus.as_mut()) {
            if lease.on_ack(id, outcome.clone()).is_ok() {
                return true;
            }
        }
        false
    }

    pub(crate) fn poll_render(&mut self, cx: &mut OutputCx<'_>, now: Now) {
        self.render.poll(cx.link, now.at);
    }

    pub(crate) fn poll_focus(&mut self, cx: &mut OutputCx<'_>, now: Now) {
        for lease in self.playouts.iter_mut().filter_map(|p| p.focus.as_mut()) {
            let _ = lease.poll(cx.link, now.at);
        }
    }

    /// The session ended: every decode stops, every lease is released, the player stops.
    pub(crate) fn session_lost(&mut self, cx: &mut OutputCx<'_>, now: Now) {
        for mut playout in self.playouts.drain(..) {
            if let Some(call) = playout.call {
                cx.link
                    .effects
                    .push(Effect::Decode(DecodeCommand::Cancel(call)));
            }
            if let Some(mut lease) = playout.focus.take() {
                let _ = lease.release(cx.link, now.at);
            }
        }
        self.render.session_ended();
        self.media.session_lost();
        self.apply_media_effects(cx, now);
    }

    // ---- playing --------------------------------------------------------------------

    /// Starts a network playback of `url` for `purpose`.
    fn play_url(&mut self, cx: &mut OutputCx<'_>, url: String, purpose: Purpose, now: Now) {
        let (class, limits, focus_owner, announcement, equalizer) = match &purpose {
            Purpose::Answer { turn } => (
                NetworkClass::DialogOutput,
                Some(ResponseLimits::from_answer_ready(now.at)),
                Some(turn.to_string()),
                false,
                None,
            ),
            Purpose::Announcement { key, .. } => (
                NetworkClass::DialogOutput,
                None,
                Some(format!("announcement-{key:?}")),
                true,
                None,
            ),
            Purpose::Content { .. } => (
                NetworkClass::Content,
                None,
                None,
                false,
                eq_settings(cx.settings),
            ),
            Purpose::Earcon => return,
        };
        let Ok(playback) = self.render.start_network(
            cx.link,
            NetworkStart {
                class,
                gain_db: 0.0,
                announcement,
                limits,
            },
        ) else {
            return;
        };
        let focus = focus_owner.and_then(|owner| {
            FocusLease::acquire(
                cx.link,
                now.at,
                FocusKind::DialogOutput,
                FocusOwner::new(owner),
                playback.generation(),
            )
            .ok()
        });
        let call = cx.ids.call();
        cx.link.effects.push(Effect::Decode(DecodeCommand::Start {
            call,
            url: url.clone(),
            skip_frames: 0,
            equalizer,
        }));
        cx.link
            .effects
            .push(Effect::Decode(DecodeCommand::Pull(call)));
        self.playouts.push(Playout {
            call: Some(call),
            url: Some(url),
            pulling: true,
            source_done: false,
            focus,
            ..Playout::local(purpose, playback)
        });
    }

    /// Plays an alert sound's preview (`alert_preview`, a local source the device holds):
    /// answered once the device accepts or refuses the `render.start`. A preview already
    /// playing is superseded by the render client (one live playback per class).
    pub(crate) fn preview(
        &mut self,
        cx: &mut OutputCx<'_>,
        id: crate::io::PreviewId,
        sound: echomuse_protocol::alerts::AlertSound,
    ) {
        let source = LocalSource::AlertPreview(sound);
        if self.render.supports(&source).is_err() {
            preview_answer(cx, id, crate::io::PreviewOutcome::Unsupported);
            return;
        }
        match self.render.start_local(cx.link, &source, 0.0, false) {
            Ok(playback) => {
                if let Some(old) = self.preview.take()
                    && !old.answered
                {
                    // Superseded before the device answered: what became of it is not
                    // known.
                    preview_answer(cx, old.id, crate::io::PreviewOutcome::Unknown);
                }
                self.preview = Some(Preview {
                    id,
                    playback,
                    answered: false,
                });
            }
            Err(_) => preview_answer(cx, id, crate::io::PreviewOutcome::SessionEnded),
        }
    }

    /// Stops the preview playing, if any.
    pub(crate) fn stop_preview(&mut self, cx: &mut OutputCx<'_>, id: crate::io::PreviewId) {
        let was_playing = match self.preview.take() {
            Some(preview) => {
                if !preview.answered {
                    preview_answer(cx, preview.id, crate::io::PreviewOutcome::Unknown);
                }
                let _ = self
                    .render
                    .cancel(cx.link, &preview.playback, CancelReason::Stopped);
                true
            }
            None => false,
        };
        preview_answer(cx, id, crate::io::PreviewOutcome::Stopped { was_playing });
    }

    /// A render event of the preview: its start is answered once, and it is forgotten
    /// when it ends.
    fn preview_event(&mut self, cx: &mut OutputCx<'_>, event: &RenderEvent) {
        let Some(preview) = self.preview.as_mut() else {
            return;
        };
        let answer = match event {
            RenderEvent::Accepted(_) | RenderEvent::Started(_) => {
                Some(crate::io::PreviewOutcome::Started)
            }
            RenderEvent::Ended { end, .. } => Some(match end {
                PlaybackEnd::Rejected(code) => {
                    crate::io::PreviewOutcome::Rejected(code.as_ref().map(|code| match code {
                        Reported::Known(code) => code.as_str().to_owned(),
                        Reported::Unrecognized(text) => text.clone(),
                    }))
                }
                PlaybackEnd::Unconfirmed => crate::io::PreviewOutcome::Unknown,
                PlaybackEnd::LinkClosed => crate::io::PreviewOutcome::SessionEnded,
                PlaybackEnd::Finished { .. } => crate::io::PreviewOutcome::Started,
            }),
            _ => None,
        };
        let id = preview.id;
        let first = !preview.answered;
        if answer.is_some() {
            preview.answered = true;
        }
        if matches!(event, RenderEvent::Ended { .. }) {
            self.preview = None;
        }
        if let Some(answer) = answer.filter(|_| first) {
            preview_answer(cx, id, answer);
        }
    }

    fn pull(&mut self, cx: &mut OutputCx<'_>, index: usize) {
        let playout = &mut self.playouts[index];
        if playout.pulling || playout.source_done || playout.cancelled.is_some() {
            return;
        }
        if let Some(call) = playout.call {
            playout.pulling = true;
            playout.waiting_room = false;
            cx.link
                .effects
                .push(Effect::Decode(DecodeCommand::Pull(call)));
        }
    }

    pub(crate) fn decode_event(
        &mut self,
        cx: &mut OutputCx<'_>,
        call: CallId,
        event: DecodeReport,
        now: Now,
    ) {
        let Some(index) = self.playouts.iter().position(|p| p.call == Some(call)) else {
            *cx.stale += 1;
            return;
        };
        self.playouts[index].pulling = false;
        let Some(playback) = self.playouts[index].playback.clone() else {
            return;
        };
        match event {
            DecodeReport::Event(DecodeEvent::Opened { seekable }) => {
                if let Purpose::Content { stream } = self.playouts[index].purpose {
                    self.media.content_opened(stream, seekable);
                }
                self.pull(cx, index);
            }
            DecodeReport::Event(DecodeEvent::Audio(pcm)) => {
                self.playouts[index].pushed = true;
                let _ = self.render.push_audio(cx.link, now.at, &playback, &pcm);
                if self.render.wants_audio(&playback) {
                    self.pull(cx, index);
                } else {
                    self.playouts[index].waiting_room = true;
                }
            }
            DecodeReport::Event(DecodeEvent::Finished(Ok(_))) => {
                self.playouts[index].source_done = true;
                let _ = self.render.finish_audio(cx.link, now.at, &playback);
            }
            DecodeReport::Event(DecodeEvent::Finished(Err(error))) => {
                tracing::warn!(device = %cx.device, %error, "a playback's audio could not be decoded");
                self.source_failed(cx, index, now);
            }
            DecodeReport::Closed => self.source_failed(cx, index, now),
        }
    }

    fn source_failed(&mut self, cx: &mut OutputCx<'_>, index: usize, now: Now) {
        let playout = &mut self.playouts[index];
        playout.source_done = true;
        let Some(playback) = playout.playback.clone() else {
            return;
        };
        if playout.pushed {
            // Play what arrived.
            let _ = self.render.finish_audio(cx.link, now.at, &playback);
        } else {
            playout.cancel(&mut self.render, cx, CancelReason::SourceFailed);
        }
    }

    /// The render client's next event, if any.
    pub(crate) fn next_render_event(&mut self) -> Option<RenderEvent> {
        self.render.next_event()
    }

    /// A render client event about one of the playbacks. What it means for the
    /// conversation is returned; [`Self::refill`] finishes it once that was handled.
    pub(crate) fn render_event(
        &mut self,
        cx: &mut OutputCx<'_>,
        event: RenderEvent,
        now: Now,
    ) -> (Notices, Refill) {
        let mut notices = Notices::default();
        let playback = match &event {
            RenderEvent::Accepted(p) | RenderEvent::Started(p) => p,
            RenderEvent::Progress { playback, .. }
            | RenderEvent::Cancelled { playback, .. }
            | RenderEvent::LimitExceeded { playback, .. }
            | RenderEvent::Ended { playback, .. } => playback,
        };
        if self
            .preview
            .as_ref()
            .is_some_and(|p| &p.playback == playback)
        {
            self.preview_event(cx, &event);
            return (notices, Refill(None));
        }
        let Some(index) = self
            .playouts
            .iter()
            .position(|p| p.playback.as_ref() == Some(playback))
        else {
            return (notices, Refill(None));
        };
        let mut refill = None;
        match event {
            RenderEvent::Accepted(_) | RenderEvent::Cancelled { .. } => {}
            RenderEvent::Started(_) => notices.audible = self.audible(index),
            RenderEvent::Progress { playback, progress } => {
                // The answer's first frame played at the first progress reporting
                // completed frames, at its device time less those frames.
                if progress.completed_frames > 0
                    && let Ok(mono_ns) = u64::try_from(progress.mono_ns)
                    && mono_ns > 0
                    && let Purpose::Answer { turn } = self.playouts[index].purpose
                {
                    let played_ns = progress.completed_frames * 1_000_000_000
                        / u64::from(echomuse_playback::eq::RENDER_SAMPLE_RATE);
                    notices.first_frame = Some((turn, mono_ns.checked_sub(played_ns)));
                }
                if !self.playouts[index].audible && progress.completed_frames > 0 {
                    notices.audible = self.audible(index);
                }
                if let Purpose::Content { stream } = self.playouts[index].purpose {
                    self.media
                        .content_progress(stream, progress.completed_frames);
                }
                refill = Some(playback);
            }
            RenderEvent::LimitExceeded { .. } => {
                self.playouts[index].limit_passed = true;
            }
            RenderEvent::Ended { end, .. } => {
                let mut playout = self.playouts.remove(index);
                if let Some(call) = playout.call.filter(|_| !playout.source_done) {
                    cx.link
                        .effects
                        .push(Effect::Decode(DecodeCommand::Cancel(call)));
                }
                cx.released.release(cx.link, playout.focus.take(), now);
                let reason = playback_reason(&end, playout.limit_passed);
                self.ended(cx, playout, reason, &mut notices, now);
            }
        }
        (notices, Refill(refill))
    }

    /// Pulls the next chunk of a playback whose progress made room for it.
    pub(crate) fn refill(&mut self, cx: &mut OutputCx<'_>, refill: Refill) {
        let Refill(Some(playback)) = refill else {
            return;
        };
        let Some(index) = self
            .playouts
            .iter()
            .position(|p| p.playback.as_ref() == Some(&playback))
        else {
            return;
        };
        if self.playouts[index].waiting_room && self.render.wants_audio(&playback) {
            self.pull(cx, index);
        }
    }

    /// A playback became audible: for an answer, the conversation hears of it.
    fn audible(&mut self, index: usize) -> Option<Audible> {
        let playout = &mut self.playouts[index];
        playout.audible = true;
        match playout.purpose {
            Purpose::Answer { turn } => Some(Audible {
                turn,
                cancelled: playout.cancelled.is_some(),
            }),
            _ => None,
        }
    }

    fn ended(
        &mut self,
        cx: &mut OutputCx<'_>,
        playout: Playout,
        reason: PlaybackReason,
        notices: &mut Notices,
        now: Now,
    ) {
        match playout.purpose {
            Purpose::Answer { turn } => {
                notices.ended = Some(AnswerEnded {
                    turn,
                    reason,
                    completed_url: playout.url.filter(|_| reason == PlaybackReason::Drained),
                });
            }
            Purpose::Announcement {
                key,
                then,
                start_conversation,
            } => {
                let played = reason == PlaybackReason::Drained;
                match then {
                    Some(message) if played || reason == PlaybackReason::Failed => {
                        // The opener ended (a failed opener does not stop the message).
                        self.play_url(
                            cx,
                            message,
                            Purpose::Announcement {
                                key,
                                then: None,
                                start_conversation,
                            },
                            now,
                        );
                    }
                    _ => {
                        self.media.announcement_ended(key, played);
                        self.apply_media_effects(cx, now);
                        notices.ha_conversation = played && start_conversation;
                    }
                }
            }
            Purpose::Content { stream } => {
                let reason = match reason {
                    PlaybackReason::Drained => ContentEndReason::Drained,
                    PlaybackReason::Cancelled => ContentEndReason::Stopped,
                    _ => ContentEndReason::Failed,
                };
                self.media.content_ended(
                    stream,
                    ContentEnd {
                        reason,
                        completed_frames: None,
                    },
                );
                self.apply_media_effects(cx, now);
            }
            Purpose::Earcon => {}
        }
    }

    // ---- media and announcements ----------------------------------------------------

    /// Carries out what the media player queued.
    pub(crate) fn apply_media_effects(&mut self, cx: &mut OutputCx<'_>, now: Now) {
        let effects: Vec<MediaEffect> = std::iter::from_fn(|| self.media.next_effect()).collect();
        for effect in effects {
            match effect {
                MediaEffect::StartContent {
                    stream,
                    url,
                    skip_frames,
                } => {
                    self.play_url(cx, url, Purpose::Content { stream }, now);
                    if skip_frames > 0
                        && let Some(Effect::Decode(DecodeCommand::Start {
                            skip_frames: skip, ..
                        })) = cx
                            .link
                            .effects
                            .iter_mut()
                            .rev()
                            .find(|e| matches!(e, Effect::Decode(DecodeCommand::Start { .. })))
                    {
                        *skip = skip_frames;
                    }
                }
                MediaEffect::StopContent { stream } => {
                    for playout in &mut self.playouts {
                        if matches!(playout.purpose, Purpose::Content { stream: s } if s == stream)
                        {
                            playout.cancel(&mut self.render, cx, CancelReason::Stopped);
                        }
                    }
                }
                MediaEffect::SetDeviceVolume(volume) => {
                    cx.link.send(
                        Outbound::VolumeSet(VolumeSet {
                            level: volume.get(),
                        }),
                        0,
                    );
                }
                MediaEffect::StartAnnouncement(plan) => {
                    let key = plan.key;
                    match plan.opener {
                        Opener::Chime => {
                            let source = LocalSource::Earcon(Earcon::AnnouncementChime);
                            let started = self.render.start_local(cx.link, &source, 0.0, true).ok();
                            match started {
                                Some(playback) => self.playouts.push(Playout::local(
                                    Purpose::Announcement {
                                        key,
                                        then: Some(plan.media_url),
                                        start_conversation: plan.start_conversation,
                                    },
                                    playback,
                                )),
                                None => self.play_url(
                                    cx,
                                    plan.media_url,
                                    Purpose::Announcement {
                                        key,
                                        then: None,
                                        start_conversation: plan.start_conversation,
                                    },
                                    now,
                                ),
                            }
                        }
                        Opener::Sound(url) => self.play_url(
                            cx,
                            url,
                            Purpose::Announcement {
                                key,
                                then: Some(plan.media_url),
                                start_conversation: plan.start_conversation,
                            },
                            now,
                        ),
                        Opener::None => self.play_url(
                            cx,
                            plan.media_url,
                            Purpose::Announcement {
                                key,
                                then: None,
                                start_conversation: plan.start_conversation,
                            },
                            now,
                        ),
                    }
                }
                MediaEffect::FinishAnnouncement { id, success } => {
                    cx.link
                        .effects
                        .push(Effect::Satellite(SatelliteCommand::FinishAnnouncement {
                            id,
                            success,
                        }));
                }
                MediaEffect::ReportState(state) => {
                    cx.link
                        .effects
                        .push(Effect::Satellite(SatelliteCommand::Media(
                            state.media_state(),
                        )));
                }
            }
        }
    }
}

fn preview_answer(
    cx: &mut OutputCx<'_>,
    id: crate::io::PreviewId,
    outcome: crate::io::PreviewOutcome,
) {
    cx.link.effects.push(Effect::PreviewAnswer { id, outcome });
}

fn playback_reason(end: &PlaybackEnd, limit_passed: bool) -> PlaybackReason {
    if limit_passed {
        return PlaybackReason::ResponseTimeout;
    }
    match end {
        PlaybackEnd::Finished { reason, .. } => match reason {
            Reported::Known(FinishReason::Drained) => PlaybackReason::Drained,
            Reported::Known(FinishReason::Cancelled) => PlaybackReason::Cancelled,
            Reported::Known(FinishReason::Underrun) => PlaybackReason::Underrun,
            Reported::Known(FinishReason::Failed) | Reported::Unrecognized(_) => {
                PlaybackReason::Failed
            }
        },
        PlaybackEnd::Rejected(Some(Reported::Known(RenderRejection::StaleGeneration))) => {
            PlaybackReason::Fenced
        }
        PlaybackEnd::Rejected(_) | PlaybackEnd::Unconfirmed => PlaybackReason::Failed,
        PlaybackEnd::LinkClosed => PlaybackReason::Cancelled,
    }
}
