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

use echomuse_esphome::voice::SatelliteRequest;
use echomuse_grammar::Line;
use echomuse_playback::{
    AckOutcome, AnnouncementKey, CancelReason, ContentEnd, ContentEndReason, DecodeEvent, Earcon,
    EqSettings, FocusKind, FocusLease, FocusOwner, LocalSource, MediaEffect, NetworkClass,
    NetworkStart, Opener, PlaybackEnd, PlaybackRef, RenderClient, RenderEvent, ResponseLimits,
    SatelliteAnnouncement, StreamId as MediaStreamId,
    wire::{FinishReason, RenderRejection},
};
use echomuse_presentation::ring::Cue;
use echomuse_protocol::{MessageId, Reported};
use echomuse_records::vocab::PlaybackReason;
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    io::{
        CallId, DecodeCommand, DecodeReport, Effect, HaRequest, Now, Outbound, SatelliteCommand,
        VolumeSet,
    },
    link::EffectLink,
    local::Expectation,
    machine::Voice,
    turn::{Call, Stage},
};

/// The last answer this device spoke, for "repeat" (§4.4). Kept in memory only.
#[derive(Clone, Debug)]
pub(crate) struct LastAnswer {
    pub(crate) url: String,
    pub(crate) ended_at: Instant,
    pub(crate) follow_up: Option<Expectation>,
}

/// The alert sound preview playing, and the request that started it.
#[derive(Debug)]
pub(crate) struct Preview {
    pub(crate) id: crate::io::PreviewId,
    pub(crate) playback: PlaybackRef,
    /// Its start was answered (accepted, refused or unknown).
    pub(crate) answered: bool,
}

/// What a playback is for.
#[derive(Clone, Debug)]
pub(crate) enum Purpose {
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
pub(crate) struct Playout {
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

#[derive(Debug, Default)]
pub(crate) struct Playouts {
    list: Vec<Playout>,
    /// Released leases waiting for their acknowledgement.
    pub(crate) releasing: Vec<FocusLease>,
}

impl Playouts {
    pub(crate) fn answer_playing(&self) -> bool {
        self.list
            .iter()
            .any(|p| matches!(p.purpose, Purpose::Answer { .. }) && p.cancelled.is_none())
    }

    /// Dialog output is audible (an answer or an announcement).
    pub(crate) fn speaking(&self) -> bool {
        self.list.iter().any(|p| {
            p.audible
                && p.cancelled.is_none()
                && matches!(
                    p.purpose,
                    Purpose::Answer { .. } | Purpose::Announcement { .. }
                )
        })
    }

    pub(crate) fn focus_deadlines(&self) -> Vec<Option<Instant>> {
        self.list
            .iter()
            .filter_map(|p| p.focus.as_ref())
            .chain(self.releasing.iter())
            .map(FocusLease::next_deadline)
            .collect()
    }

    pub(crate) fn focus_ack(&mut self, id: &MessageId, outcome: &AckOutcome) -> bool {
        for lease in self
            .list
            .iter_mut()
            .filter_map(|p| p.focus.as_mut())
            .chain(self.releasing.iter_mut())
        {
            if lease.on_ack(id, outcome.clone()).is_ok() {
                return true;
            }
        }
        false
    }

    pub(crate) fn poll_focus(&mut self, link: &mut EffectLink, now: Instant) {
        for lease in self
            .list
            .iter_mut()
            .filter_map(|p| p.focus.as_mut())
            .chain(self.releasing.iter_mut())
        {
            let _ = lease.poll(link, now);
        }
        self.releasing.retain(FocusLease::is_held);
    }

    /// The device's own answer stops when a new turn begins.
    pub(crate) fn interrupt_answers(
        &mut self,
        render: &mut RenderClient,
        link: &mut EffectLink,
        _now: Now,
    ) {
        for playout in &mut self.list {
            if matches!(playout.purpose, Purpose::Answer { .. }) {
                cancel(playout, render, link, CancelReason::Interrupted);
            }
        }
    }

    /// Stops what a turn still plays when it ends some other way.
    pub(crate) fn end_answers_of(
        &mut self,
        turn: Uuid,
        render: &mut RenderClient,
        link: &mut EffectLink,
        _now: Now,
    ) {
        for playout in &mut self.list {
            if matches!(playout.purpose, Purpose::Answer { turn: t } if t == turn) {
                cancel(playout, render, link, CancelReason::Interrupted);
            }
        }
    }

    pub(crate) fn session_lost(&mut self, link: &mut EffectLink, now: Now) {
        for mut playout in self.list.drain(..) {
            if let Some(call) = playout.call {
                link.effects
                    .push(Effect::Decode(DecodeCommand::Cancel(call)));
            }
            if let Some(mut lease) = playout.focus.take() {
                let _ = lease.release(link, now.at);
            }
        }
        self.releasing.clear();
    }
}

fn cancel(
    playout: &mut Playout,
    render: &mut RenderClient,
    link: &mut EffectLink,
    reason: CancelReason,
) {
    if playout.cancelled.is_some() {
        return;
    }
    playout.cancelled = Some(reason);
    if let Some(call) = playout.call {
        link.effects
            .push(Effect::Decode(DecodeCommand::Cancel(call)));
        playout.pulling = false;
    }
    if let Some(playback) = &playout.playback {
        let _ = render.cancel(link, playback, reason);
    }
}

fn eq_settings(settings: &echomuse_settings::DeviceSettings) -> Option<EqSettings> {
    let bands = settings.playback.eq_bands.db().map(|db| db.round() as i8);
    EqSettings::new(bands, settings.playback.eq_loudness)
        .ok()
        .filter(|eq| !eq.is_flat())
}

impl Voice {
    /// Starts a network playback of `url` for `purpose`.
    fn play_url(&mut self, url: String, purpose: Purpose, now: Now) {
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
                eq_settings(&self.settings),
            ),
            Purpose::Earcon => return,
        };
        let Ok(playback) = self.render.start_network(
            &mut self.link,
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
                &mut self.link,
                now.at,
                FocusKind::DialogOutput,
                FocusOwner::new(owner),
                playback.generation(),
            )
            .ok()
        });
        let call = self.call_id();
        self.link.effects.push(Effect::Decode(DecodeCommand::Start {
            call,
            url: url.clone(),
            skip_frames: 0,
            equalizer,
        }));
        self.link
            .effects
            .push(Effect::Decode(DecodeCommand::Pull(call)));
        self.playouts.list.push(Playout {
            purpose,
            call: Some(call),
            url: Some(url),
            playback: Some(playback),
            pulling: true,
            waiting_room: false,
            pushed: false,
            source_done: false,
            focus,
            audible: false,
            limit_passed: false,
            cancelled: None,
        });
    }

    /// Plays the current turn's answer from `url` (HA's TTS, or a line's).
    pub(crate) fn speak_answer(&mut self, url: String, now: Now) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if turn.answering.playing {
            // HA gives one TTS URL per run; a second is ignored.
            return;
        }
        turn.answering.playing = true;
        let uuid = turn.uuid;
        self.play_url(url, Purpose::Answer { turn: uuid }, now);
    }

    /// Says one of the controller's own lines for the current turn (HA synthesises it).
    pub(crate) fn say(&mut self, line: Line, now: Now) {
        let text = self.line_text(&line, now);
        let target = self.target();
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if turn.stage != Stage::Speaking {
            turn.stage = Stage::Thinking;
        }
        turn.facts.response_text = Some(text.clone());
        turn.facts.intent_ended.get_or_insert(now.at);
        if let Some(body) = self.view.final_answer(&text) {
            self.link.send(Outbound::TurnView(body), 0);
        }
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        let (uuid, scope) = (turn.uuid, turn.scope.clone());
        match target {
            Some(target) => {
                let call = self.call_id();
                self.calls.insert(call, Call::Tts { turn: uuid });
                self.link.effects.push(Effect::Ha {
                    call,
                    scope,
                    request: HaRequest::Tts { target, text },
                });
            }
            None => {
                // No pipeline to speak with (HA unreachable): the error cue answers.
                self.cue(Cue::Error, now);
                self.answer_done(None, now);
            }
        }
    }

    /// Asks a question: the line, then a reply window expecting `expectation`.
    pub(crate) fn ask(&mut self, line: Line, expectation: Expectation, now: Now) {
        if let Some(turn) = self.turn.as_mut() {
            turn.answering.follow_up = Some(expectation);
        }
        self.say(line, now);
    }

    /// Plays an alert sound's preview (`alert_preview`, a local source the device holds):
    /// answered once the device accepts or refuses the `render.start`. A preview already
    /// playing is superseded by the render client (one live playback per class).
    pub(crate) fn preview(
        &mut self,
        id: crate::io::PreviewId,
        sound: echomuse_playback::AlertPreviewSound,
        _now: Now,
    ) {
        let source = LocalSource::AlertPreview(sound);
        if self.render.supports(&source).is_err() {
            self.preview_answer(id, crate::io::PreviewOutcome::Unsupported);
            return;
        }
        match self.render.start_local(&mut self.link, &source, 0.0, false) {
            Ok(playback) => {
                if let Some(old) = self.preview.take()
                    && !old.answered
                {
                    // Superseded before the device answered: what became of it is not
                    // known.
                    self.preview_answer(old.id, crate::io::PreviewOutcome::Unknown);
                }
                self.preview = Some(Preview {
                    id,
                    playback,
                    answered: false,
                });
            }
            Err(_) => self.preview_answer(id, crate::io::PreviewOutcome::SessionEnded),
        }
    }

    /// Stops the preview playing, if any.
    pub(crate) fn stop_preview(&mut self, id: crate::io::PreviewId, _now: Now) {
        let was_playing = match self.preview.take() {
            Some(preview) => {
                if !preview.answered {
                    self.preview_answer(preview.id, crate::io::PreviewOutcome::Unknown);
                }
                let _ =
                    self.render
                        .cancel(&mut self.link, &preview.playback, CancelReason::Stopped);
                true
            }
            None => false,
        };
        self.preview_answer(id, crate::io::PreviewOutcome::Stopped { was_playing });
    }

    fn preview_answer(&mut self, id: crate::io::PreviewId, outcome: crate::io::PreviewOutcome) {
        self.link
            .effects
            .push(Effect::PreviewAnswer { id, outcome });
    }

    /// A render event of the preview: its start is answered once, and it is forgotten
    /// when it ends.
    fn preview_event(&mut self, event: &RenderEvent) {
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
            self.preview_answer(id, answer);
        }
    }

    pub(crate) fn earcon(&mut self, earcon: Earcon, _now: Now) {
        let source = LocalSource::Earcon(earcon);
        if self.render.supports(&source).is_err() {
            return;
        }
        if let Ok(playback) = self.render.start_local(&mut self.link, &source, 0.0, false) {
            self.playouts.list.push(Playout {
                purpose: Purpose::Earcon,
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
            });
        }
    }

    pub(crate) fn cue(&mut self, cue: Cue, now: Now) {
        if let Some(command) = self.ring.cue(cue, now.at) {
            self.link.send(Outbound::Led(command), 0);
        }
    }

    fn playout_index_by_call(&self, call: CallId) -> Option<usize> {
        self.playouts.list.iter().position(|p| p.call == Some(call))
    }

    fn pull(&mut self, index: usize) {
        let playout = &mut self.playouts.list[index];
        if playout.pulling || playout.source_done || playout.cancelled.is_some() {
            return;
        }
        if let Some(call) = playout.call {
            playout.pulling = true;
            playout.waiting_room = false;
            self.link
                .effects
                .push(Effect::Decode(DecodeCommand::Pull(call)));
        }
    }

    pub(crate) fn decode_event(&mut self, call: CallId, event: DecodeReport, now: Now) {
        let Some(index) = self.playout_index_by_call(call) else {
            self.counters.stale_results += 1;
            return;
        };
        self.playouts.list[index].pulling = false;
        let Some(playback) = self.playouts.list[index].playback.clone() else {
            return;
        };
        match event {
            DecodeReport::Event(DecodeEvent::Opened { seekable }) => {
                if let Purpose::Content { stream } = self.playouts.list[index].purpose {
                    self.media.content_opened(stream, seekable);
                }
                self.pull(index);
            }
            DecodeReport::Event(DecodeEvent::Audio(pcm)) => {
                self.playouts.list[index].pushed = true;
                let _ = self
                    .render
                    .push_audio(&mut self.link, now.at, &playback, &pcm);
                if self.render.wants_audio(&playback) {
                    self.pull(index);
                } else {
                    self.playouts.list[index].waiting_room = true;
                }
            }
            DecodeReport::Event(DecodeEvent::Finished(Ok(_))) => {
                self.playouts.list[index].source_done = true;
                let _ = self.render.finish_audio(&mut self.link, now.at, &playback);
            }
            DecodeReport::Event(DecodeEvent::Finished(Err(error))) => {
                tracing::warn!(device = %self.device, %error, "a playback's audio could not be decoded");
                self.source_failed(index, now);
            }
            DecodeReport::Closed => self.source_failed(index, now),
        }
    }

    fn source_failed(&mut self, index: usize, now: Now) {
        let playout = &mut self.playouts.list[index];
        playout.source_done = true;
        let Some(playback) = playout.playback.clone() else {
            return;
        };
        if playout.pushed {
            // Play what arrived.
            let _ = self.render.finish_audio(&mut self.link, now.at, &playback);
        } else {
            cancel(
                playout,
                &mut self.render,
                &mut self.link,
                CancelReason::SourceFailed,
            );
        }
    }

    /// A render client event about one of the playbacks.
    pub(crate) fn playout_event(&mut self, playback: &PlaybackRef, event: RenderEvent, now: Now) {
        if self
            .preview
            .as_ref()
            .is_some_and(|p| &p.playback == playback)
        {
            self.preview_event(&event);
            return;
        }
        let Some(index) = self
            .playouts
            .list
            .iter()
            .position(|p| p.playback.as_ref() == Some(playback))
        else {
            return;
        };
        match event {
            RenderEvent::Accepted(_) | RenderEvent::Cancelled { .. } => {}
            RenderEvent::Started(_) => {
                self.playouts.list[index].audible = true;
                if let Purpose::Answer { turn } = self.playouts.list[index].purpose
                    && let Some(current) = self.turn.as_mut().filter(|t| t.uuid == turn)
                {
                    current.stage = Stage::Speaking;
                    current.facts.audible_at.get_or_insert(now.at);
                    if self.playouts.list[index].cancelled.is_none() {
                        self.question_audible(turn, now);
                    }
                }
            }
            RenderEvent::Progress { progress, .. } => {
                // The answer's first frame played at the first progress reporting
                // completed frames, at its device time less those frames.
                if progress.completed_frames > 0
                    && let Ok(mono_ns) = u64::try_from(progress.mono_ns)
                    && mono_ns > 0
                    && let Purpose::Answer { turn } = self.playouts.list[index].purpose
                    && let Some(current) = self.turn.as_mut().filter(|t| t.uuid == turn)
                    && current.facts.first_frame_mono_ns.is_none()
                {
                    let played_ns = progress.completed_frames * 1_000_000_000
                        / u64::from(echomuse_playback::eq::RENDER_SAMPLE_RATE);
                    current.facts.first_frame_mono_ns = mono_ns.checked_sub(played_ns);
                }
                if !self.playouts.list[index].audible && progress.completed_frames > 0 {
                    self.playouts.list[index].audible = true;
                    if let Purpose::Answer { turn } = self.playouts.list[index].purpose
                        && let Some(current) = self.turn.as_mut().filter(|t| t.uuid == turn)
                    {
                        current.stage = Stage::Speaking;
                        current.facts.audible_at.get_or_insert(now.at);
                        if self.playouts.list[index].cancelled.is_none() {
                            self.question_audible(turn, now);
                        }
                    }
                }
                if let Purpose::Content { stream } = self.playouts.list[index].purpose {
                    self.media
                        .content_progress(stream, progress.completed_frames);
                }
                if self.playouts.list[index].waiting_room && self.render.wants_audio(playback) {
                    self.pull(index);
                }
            }
            RenderEvent::LimitExceeded { .. } => {
                self.playouts.list[index].limit_passed = true;
            }
            RenderEvent::Ended { end, .. } => {
                let mut playout = self.playouts.list.remove(index);
                if let Some(call) = playout.call.filter(|_| !playout.source_done) {
                    self.link
                        .effects
                        .push(Effect::Decode(DecodeCommand::Cancel(call)));
                }
                if let Some(lease) = playout.focus.take() {
                    self.release_focus(Some(lease), now);
                }
                let reason = playback_reason(&end, playout.limit_passed);
                self.playout_ended(playout, reason, now);
            }
        }
    }

    fn playout_ended(&mut self, playout: Playout, reason: PlaybackReason, now: Now) {
        match playout.purpose {
            Purpose::Answer { turn } => {
                if let Some(current) = self.turn.as_mut().filter(|t| t.uuid == turn) {
                    current.facts.playback_ended = Some(now.at);
                    if reason == PlaybackReason::Drained
                        && let Some(url) = playout.url
                    {
                        self.last_answer = Some(LastAnswer {
                            url,
                            ended_at: now.at,
                            follow_up: current.answering.follow_up.clone(),
                        });
                    }
                    if reason != PlaybackReason::Drained && reason != PlaybackReason::Cancelled {
                        current.facts.terminal.get_or_insert(match reason {
                            PlaybackReason::ResponseTimeout => {
                                echomuse_records::vocab::TerminalReason::ResponseTimeout
                            }
                            _ => echomuse_records::vocab::TerminalReason::Interrupted,
                        });
                    }
                    self.answer_done(Some(reason), now);
                }
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
                        let effects = self.media_effects();
                        self.apply_media_effects(effects, now);
                        if played && start_conversation {
                            self.open_ha_conversation(now);
                        }
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
                let effects = self.media_effects();
                self.apply_media_effects(effects, now);
            }
            Purpose::Earcon => {}
        }
    }

    // ---- media and announcements ----------------------------------------------------

    pub(crate) fn media_playing(&self) -> bool {
        self.media.state().playback == echomuse_esphome::entities::Playback::Playing
    }

    pub(crate) fn media_effects(&mut self) -> Vec<MediaEffect> {
        std::iter::from_fn(|| self.media.next_effect()).collect()
    }

    pub(crate) fn apply_media_effects(&mut self, effects: Vec<MediaEffect>, now: Now) {
        for effect in effects {
            match effect {
                MediaEffect::StartContent {
                    stream,
                    url,
                    skip_frames,
                } => {
                    self.play_url(url, Purpose::Content { stream }, now);
                    if skip_frames > 0
                        && let Some(Effect::Decode(DecodeCommand::Start {
                            skip_frames: skip, ..
                        })) = self
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
                    for playout in &mut self.playouts.list {
                        if matches!(playout.purpose, Purpose::Content { stream: s } if s == stream)
                        {
                            cancel(
                                playout,
                                &mut self.render,
                                &mut self.link,
                                CancelReason::Stopped,
                            );
                        }
                    }
                }
                MediaEffect::SetDeviceVolume(volume) => {
                    self.link.send(
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
                            let started = self
                                .render
                                .start_local(&mut self.link, &source, 0.0, true)
                                .ok();
                            match started {
                                Some(playback) => self.playouts.list.push(Playout {
                                    purpose: Purpose::Announcement {
                                        key,
                                        then: Some(plan.media_url),
                                        start_conversation: plan.start_conversation,
                                    },
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
                                }),
                                None => self.play_url(
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
                            url,
                            Purpose::Announcement {
                                key,
                                then: Some(plan.media_url),
                                start_conversation: plan.start_conversation,
                            },
                            now,
                        ),
                        Opener::None => self.play_url(
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
                    self.link.effects.push(Effect::Satellite(
                        SatelliteCommand::FinishAnnouncement { id, success },
                    ));
                }
                MediaEffect::ReportState(state) => {
                    self.link
                        .effects
                        .push(Effect::Satellite(SatelliteCommand::Media(
                            state.media_state(),
                        )));
                }
            }
        }
    }

    /// A request Home Assistant sent the satellite.
    pub(crate) fn satellite(&mut self, request: SatelliteRequest, now: Now) {
        match request {
            SatelliteRequest::Media(request) => {
                let _ = self.media.command(request);
            }
            SatelliteRequest::Announce(announcement) => {
                let _: AnnouncementKey = self
                    .media
                    .announce(SatelliteAnnouncement::from(announcement));
            }
            SatelliteRequest::Run { run, event } => self.satellite_run_event(run, event, now),
            // Timers and the stop button belong to the alert service; the runtime
            // forwards them there.
            SatelliteRequest::Timer(_) | SatelliteRequest::StopAlert => {}
        }
        let effects = self.media_effects();
        self.apply_media_effects(effects, now);
    }
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
