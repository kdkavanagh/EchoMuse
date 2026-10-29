//! The device's media player (spec §4.9) and its announcement queue (§4.5).
//!
//! [`MediaPlayer`] is an owned state machine the device runtime drives. Its
//! inputs are Home Assistant's media commands and announcements, the start and
//! end of dialogs, and what happened to the content playback it asked for;
//! its outputs are [`MediaEffect`]s for the runtime to carry out. It does no
//! I/O and runs no tasks.
//!
//! - **One stream.** Playing a URL replaces the current stream.
//! - **Pause and resume.** The device cannot hold content for the controller,
//!   so pausing cancels the content playback and keeps a bookmark: the frames
//!   the device reported played. Resuming decodes the stream again, skipping to
//!   the bookmark when the resource is seekable, or joining at the live edge
//!   when it is a live stream.
//! - **Dialog deferral.** While a dialog runs (a turn, or an announcement
//!   playing), play, pause, resume and stop change only the intended state,
//!   which is reported to Home Assistant at once; the content playback follows
//!   when the dialog ends. Content keeps playing, ducked by the device, through
//!   the dialog. Volume and mute apply at once: they act on the device's
//!   output level, which the dialog shares.
//! - **Announcements** queue behind the device's replies and behind each
//!   other, and each opens with the announcement chime when the device has it
//!   and the announcement asks for a preannounce sound.

use std::collections::VecDeque;

use echomuse_esphome::entities::{DeviceVolume, MediaState, Playback};
use echomuse_esphome::voice::{Announcement, AnnouncementId, MediaRequest};
use echomuse_protocol::{Capabilities, Capability};
use std::fmt::Debug;

use crate::decode::Seekability;

/// Identifies one media stream: one URL from one play request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StreamId(u64);

/// Identifies one queued announcement.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AnnouncementKey(u64);

/// What opens an announcement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Opener {
    /// The device's `builtin:announcement_chime` earcon.
    Chime,
    /// Home Assistant's preannounce sound, on a device without the chime.
    Sound(String),
    /// Nothing.
    None,
}

/// An announcement to play now, as the device's dialog output. `Id` names a
/// satellite announcement to Home Assistant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnnouncementPlan<Id = AnnouncementId> {
    /// This announcement, for [`MediaPlayer::announcement_ended`].
    pub key: AnnouncementKey,
    /// What to play.
    pub media_url: String,
    /// What plays first.
    pub opener: Opener,
    /// The text, if Home Assistant gave one.
    pub text: Option<String>,
    /// Whether a reply window opens when it ends.
    pub start_conversation: bool,
    /// The satellite announcement to finish when it ends; `None` for a media
    /// player announcement, which Home Assistant does not wait for.
    pub satellite: Option<Id>,
}

/// Something for the runtime to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MediaEffect<Id = AnnouncementId> {
    /// Decode `url` and play it as the device's content, discarding the first
    /// `skip_frames` decoded frames. The playback's progress and end come back
    /// through [`MediaPlayer::content_opened`],
    /// [`MediaPlayer::content_progress`] and [`MediaPlayer::content_ended`].
    StartContent {
        /// The stream.
        stream: StreamId,
        /// What to decode.
        url: String,
        /// Frames to skip: the bookmark.
        skip_frames: u64,
    },
    /// Cancel the stream's content playback and its decode.
    StopContent {
        /// The stream.
        stream: StreamId,
    },
    /// Send `volume_set`.
    SetDeviceVolume(DeviceVolume),
    /// Play an announcement as dialog output.
    StartAnnouncement(AnnouncementPlan<Id>),
    /// Tell Home Assistant a satellite announcement ended.
    FinishAnnouncement {
        /// The announcement.
        id: Id,
        /// Whether it played to its end.
        success: bool,
    },
    /// Report the media player's state to Home Assistant.
    ReportState(PlayerState),
}

/// The media player's state as Home Assistant sees it: the intended state,
/// which a dialog's deferral does not hide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayerState {
    /// Idle, playing, paused or announcing.
    pub playback: Playback,
    /// The device's volume; `None` while unknown or when the device has no
    /// volume control.
    pub volume: Option<DeviceVolume>,
    /// Whether the controller muted it.
    pub muted: bool,
}

impl PlayerState {
    /// The entity state, or `None` while the volume is unknown.
    pub fn media_state(&self) -> Option<MediaState> {
        self.volume.map(|volume| MediaState {
            playback: self.playback,
            volume,
            muted: self.muted,
        })
    }
}

/// A satellite announcement Home Assistant asked for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SatelliteAnnouncement<Id = AnnouncementId> {
    /// Names it in the reply Home Assistant waits for.
    pub id: Id,
    /// What to play.
    pub media_url: String,
    /// The text, if Home Assistant gave one.
    pub text: Option<String>,
    /// Home Assistant's preannounce sound; present when it asks for one.
    pub preannounce_url: Option<String>,
    /// Whether a reply window opens when it ends.
    pub start_conversation: bool,
}

impl From<Announcement> for SatelliteAnnouncement<AnnouncementId> {
    fn from(announcement: Announcement) -> Self {
        Self {
            id: announcement.id,
            media_url: announcement.media_url,
            text: announcement.text,
            preannounce_url: announcement.preannounce_url,
            start_conversation: announcement.start_conversation,
        }
    }
}

/// How a command was taken.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandOutcome {
    /// Carried out, or already the case.
    Applied,
    /// Recorded as intended and reported; carried out when the dialog ends.
    Deferred,
    /// Not possible.
    Unavailable(Unavailable),
}

/// Why a command cannot be carried out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unavailable {
    /// Pause with no stream.
    NothingPlaying,
    /// Play with no stream to resume.
    NothingToResume,
    /// The device reported no volume control.
    NoVolumeControl,
}

/// How a content playback ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentEndReason {
    /// The stream played to its end.
    Drained,
    /// The playback was cancelled on a [`MediaEffect::StopContent`].
    Stopped,
    /// Fetching, decoding or the device failed it.
    Failed,
    /// The device session ended under it.
    Interrupted,
}

/// A content playback's end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentEnd {
    /// Why it ended.
    pub reason: ContentEndReason,
    /// The frames the device reported played (`render.finished`
    /// `last_completed_frame`); `None` when unknown.
    pub completed_frames: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Intent {
    Idle,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Actual {
    Idle,
    Playing(StreamId),
    /// Stop sent to pause; waiting for the end that carries the bookmark.
    Pausing(StreamId),
    Paused(StreamId),
}

#[derive(Clone, Debug)]
struct Stream {
    id: StreamId,
    url: String,
    seekable: Option<Seekability>,
    /// The source frame the current or last playback started from.
    start_frame: u64,
    /// The frames the current or last playback reported played.
    completed: Option<u64>,
}

/// The media player. See the module documentation. `Id` names satellite
/// announcements; the runtime uses the ESPHome server's [`AnnouncementId`].
#[derive(Debug)]
pub struct MediaPlayer<Id = AnnouncementId> {
    announcement_chime: bool,
    next_stream: u64,
    next_announcement: u64,
    stream: Option<Stream>,
    intent: Intent,
    actual: Actual,
    dialog: bool,
    announcing: Option<AnnouncementPlan<Id>>,
    queue: VecDeque<AnnouncementPlan<Id>>,
    volume: Option<DeviceVolume>,
    muted: bool,
    reported: Option<PlayerState>,
    effects: VecDeque<MediaEffect<Id>>,
}

impl<Id: Clone + Debug + Eq> MediaPlayer<Id> {
    /// A player for a device that announced `capabilities`, with the volume
    /// its hello reported (`None` for a null level).
    pub fn new(capabilities: &Capabilities, volume: Option<DeviceVolume>) -> Self {
        Self {
            announcement_chime: capabilities.contains(Capability::AnnouncementChime),
            next_stream: 0,
            next_announcement: 0,
            stream: None,
            intent: Intent::Idle,
            actual: Actual::Idle,
            dialog: false,
            announcing: None,
            queue: VecDeque::new(),
            volume,
            muted: false,
            reported: None,
            effects: VecDeque::new(),
        }
    }

    /// The state Home Assistant should see.
    pub fn state(&self) -> PlayerState {
        PlayerState {
            playback: if self.announcing.is_some() {
                Playback::Announcing
            } else {
                match self.intent {
                    Intent::Idle => Playback::Idle,
                    Intent::Playing => Playback::Playing,
                    Intent::Paused => Playback::Paused,
                }
            },
            volume: self.volume,
            muted: self.muted,
        }
    }

    /// The next effect to carry out.
    pub fn next_effect(&mut self) -> Option<MediaEffect<Id>> {
        self.effects.pop_front()
    }

    /// Takes a Home Assistant media player command.
    pub fn command(&mut self, request: MediaRequest) -> CommandOutcome {
        let outcome = match request {
            MediaRequest::PlayUrl {
                url,
                announcement: true,
            } => {
                let opener = if self.announcement_chime {
                    Opener::Chime
                } else {
                    Opener::None
                };
                self.enqueue(url, opener, None, false, None)
            }
            MediaRequest::PlayUrl {
                url,
                announcement: false,
            } => {
                self.next_stream += 1;
                self.stream = Some(Stream {
                    id: StreamId(self.next_stream),
                    url,
                    seekable: None,
                    start_frame: 0,
                    completed: None,
                });
                self.set_intent(Intent::Playing)
            }
            MediaRequest::Play => match self.stream {
                None => CommandOutcome::Unavailable(Unavailable::NothingToResume),
                Some(_) => self.set_intent(Intent::Playing),
            },
            MediaRequest::Pause => match self.stream {
                None => CommandOutcome::Unavailable(Unavailable::NothingPlaying),
                Some(_) => self.set_intent(Intent::Paused),
            },
            MediaRequest::Stop => {
                self.stream = None;
                self.set_intent(Intent::Idle)
            }
            MediaRequest::SetVolume(volume) => self.set_volume(volume),
            MediaRequest::Mute => self.mute(),
            MediaRequest::Unmute => self.unmute(),
        };
        self.report();
        outcome
    }

    /// Queues a satellite announcement. It plays once no dialog runs and
    /// every earlier announcement has ended. One that asks for a preannounce
    /// sound opens with the device's announcement chime in its place, or with
    /// Home Assistant's sound on a device without the chime.
    pub fn announce(&mut self, announcement: SatelliteAnnouncement<Id>) -> AnnouncementKey {
        let opener = match announcement.preannounce_url {
            Some(_) if self.announcement_chime => Opener::Chime,
            Some(url) => Opener::Sound(url),
            None => Opener::None,
        };
        self.enqueue(
            announcement.media_url,
            opener,
            announcement.text,
            announcement.start_conversation,
            Some(announcement.id),
        );
        let key = AnnouncementKey(self.next_announcement);
        self.report();
        key
    }

    /// A dialog began: a turn is listening or answering. Commands defer until
    /// [`dialog_ended`](Self::dialog_ended).
    pub fn dialog_started(&mut self) {
        self.dialog = true;
    }

    /// The dialog ended: queued announcements play, then deferred commands
    /// take effect.
    pub fn dialog_ended(&mut self) {
        self.dialog = false;
        self.advance();
        self.report();
    }

    /// The announcement ended; `success` when it played to its end.
    pub fn announcement_ended(&mut self, key: AnnouncementKey, success: bool) {
        if self.announcing.as_ref().is_none_or(|plan| plan.key != key) {
            return;
        }
        if let Some(plan) = self.announcing.take()
            && let Some(id) = plan.satellite
        {
            self.effects
                .push_back(MediaEffect::FinishAnnouncement { id, success });
        }
        self.advance();
        self.report();
    }

    /// The stream's decode opened and said whether it can be resumed.
    pub fn content_opened(&mut self, stream: StreamId, seekable: Seekability) {
        if let Some(current) = self.stream.as_mut().filter(|s| s.id == stream) {
            current.seekable = Some(seekable);
        }
    }

    /// The device reported the stream's playback progress.
    pub fn content_progress(&mut self, stream: StreamId, completed_frames: u64) {
        if let Some(current) = self.stream.as_mut().filter(|s| s.id == stream) {
            current.completed = Some(current.completed.unwrap_or(0).max(completed_frames));
        }
    }

    /// The stream's content playback ended.
    pub fn content_ended(&mut self, stream: StreamId, end: ContentEnd) {
        let ours = match self.actual {
            Actual::Playing(id) | Actual::Pausing(id) => id == stream,
            Actual::Idle | Actual::Paused(_) => false,
        };
        if !ours {
            // A replaced or stopped stream's late end.
            return;
        }
        let current = self.stream.as_ref().is_some_and(|s| s.id == stream);
        match end.reason {
            ContentEndReason::Drained | ContentEndReason::Failed => {
                self.actual = Actual::Idle;
                if current {
                    self.stream = None;
                    self.intent = Intent::Idle;
                }
            }
            ContentEndReason::Stopped | ContentEndReason::Interrupted => {
                if let Some(s) = self.stream.as_mut().filter(|s| s.id == stream) {
                    if let Some(completed) = end.completed_frames {
                        s.completed = Some(completed);
                    }
                    s.start_frame = match s.seekable {
                        Some(Seekability::Seekable) => s.start_frame + s.completed.unwrap_or(0),
                        // A live stream rejoins at its live edge.
                        Some(Seekability::Live) => 0,
                        // Never opened: nothing was played past the start.
                        None => s.start_frame,
                    };
                    s.completed = None;
                }
                self.actual = Actual::Paused(stream);
                if end.reason == ContentEndReason::Interrupted
                    && current
                    && self.intent == Intent::Playing
                {
                    // Losing the device pauses rather than replaying later.
                    self.intent = Intent::Paused;
                }
            }
        }
        self.advance();
        self.report();
    }

    /// The device reported its volume (`session.hello` or `volume_state`);
    /// `None` when it has no volume control. While muted, the device's level
    /// of 0 is the mute itself and the remembered level stays shown; any
    /// other level was set on the device and ends the mute.
    pub fn device_volume(&mut self, volume: Option<DeviceVolume>) {
        match volume {
            Some(level) if level.get() == 0 && self.muted => {}
            Some(_) | None => {
                self.volume = volume;
                self.muted = false;
            }
        }
        self.report();
    }

    /// The device session ended: queued and playing announcements end
    /// unsuccessfully, and no dialog is running.
    pub fn session_lost(&mut self) {
        self.dialog = false;
        for plan in self
            .announcing
            .take()
            .into_iter()
            .chain(self.queue.drain(..))
        {
            if let Some(id) = plan.satellite {
                self.effects
                    .push_back(MediaEffect::FinishAnnouncement { id, success: false });
            }
        }
        self.report();
    }

    fn deferring(&self) -> bool {
        self.dialog || self.announcing.is_some()
    }

    fn set_intent(&mut self, intent: Intent) -> CommandOutcome {
        self.intent = intent;
        if self.deferring() {
            CommandOutcome::Deferred
        } else {
            self.reconcile();
            CommandOutcome::Applied
        }
    }

    fn set_volume(&mut self, volume: DeviceVolume) -> CommandOutcome {
        if self.volume.is_none() {
            return CommandOutcome::Unavailable(Unavailable::NoVolumeControl);
        }
        self.volume = Some(volume);
        self.muted = false;
        self.effects.push_back(MediaEffect::SetDeviceVolume(volume));
        CommandOutcome::Applied
    }

    fn mute(&mut self) -> CommandOutcome {
        if self.volume.is_none() {
            return CommandOutcome::Unavailable(Unavailable::NoVolumeControl);
        }
        if !self.muted {
            // The level stays remembered in `volume` for the unmute.
            self.muted = true;
            let silent = DeviceVolume::new(0).expect("0 is a volume step");
            self.effects.push_back(MediaEffect::SetDeviceVolume(silent));
        }
        CommandOutcome::Applied
    }

    fn unmute(&mut self) -> CommandOutcome {
        let Some(volume) = self.volume else {
            return CommandOutcome::Unavailable(Unavailable::NoVolumeControl);
        };
        if self.muted {
            self.muted = false;
            self.effects.push_back(MediaEffect::SetDeviceVolume(volume));
        }
        CommandOutcome::Applied
    }

    fn enqueue(
        &mut self,
        media_url: String,
        opener: Opener,
        text: Option<String>,
        start_conversation: bool,
        satellite: Option<Id>,
    ) -> CommandOutcome {
        self.next_announcement += 1;
        self.queue.push_back(AnnouncementPlan {
            key: AnnouncementKey(self.next_announcement),
            media_url,
            opener,
            text,
            start_conversation,
            satellite,
        });
        let waits = self.deferring();
        self.advance();
        if waits {
            CommandOutcome::Deferred
        } else {
            CommandOutcome::Applied
        }
    }

    /// Starts the next announcement when nothing holds it back, then, with no
    /// dialog and no announcement, brings the content playback to the
    /// intended state.
    fn advance(&mut self) {
        if !self.deferring()
            && let Some(plan) = self.queue.pop_front()
        {
            self.announcing = Some(plan.clone());
            self.effects.push_back(MediaEffect::StartAnnouncement(plan));
        }
        if !self.deferring() {
            self.reconcile();
        }
    }

    fn reconcile(&mut self) {
        let current = self.stream.as_ref().map(|s| s.id);
        match (self.intent, current) {
            (Intent::Idle, _) | (_, None) => {
                if let Actual::Playing(id) = self.actual {
                    self.effects
                        .push_back(MediaEffect::StopContent { stream: id });
                }
                self.actual = Actual::Idle;
            }
            (Intent::Paused, Some(current)) => match self.actual {
                Actual::Playing(id) => {
                    self.effects
                        .push_back(MediaEffect::StopContent { stream: id });
                    self.actual = if id == current {
                        Actual::Pausing(id)
                    } else {
                        Actual::Idle
                    };
                }
                Actual::Pausing(id) | Actual::Paused(id) if id != current => {
                    self.actual = Actual::Idle;
                }
                Actual::Pausing(_) | Actual::Paused(_) | Actual::Idle => {}
            },
            (Intent::Playing, Some(current)) => match self.actual {
                Actual::Playing(id) if id == current => {}
                // The bookmark is still on its way.
                Actual::Pausing(id) if id == current => {}
                Actual::Playing(id) => {
                    self.effects
                        .push_back(MediaEffect::StopContent { stream: id });
                    self.start(current);
                }
                Actual::Pausing(_) | Actual::Paused(_) | Actual::Idle => self.start(current),
            },
        }
    }

    fn start(&mut self, id: StreamId) {
        let Some(stream) = self.stream.as_mut().filter(|s| s.id == id) else {
            return;
        };
        stream.completed = None;
        self.effects.push_back(MediaEffect::StartContent {
            stream: id,
            url: stream.url.clone(),
            skip_frames: stream.start_frame,
        });
        self.actual = Actual::Playing(id);
    }

    fn report(&mut self) {
        let state = self.state();
        if self.reported != Some(state) {
            self.reported = Some(state);
            self.effects.push_back(MediaEffect::ReportState(state));
        }
    }
}

#[cfg(test)]
mod tests;
