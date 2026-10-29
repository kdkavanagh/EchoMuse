//! The voice satellite node (spec §3.3.1): one per device, listening on the
//! device's fixed port while the device runtime keeps it started.
//!
//! The runtime drives it through [`VoiceSatellite`]: entity states, button
//! events and controller-started voice runs. What Home Assistant asks of the
//! device arrives as [`SatelliteRequest`]s on a bounded channel. The node
//! never waits for the runtime: if that channel is full, the connection that
//! produced the request is closed (Home Assistant reconnects) rather than the
//! request being dropped silently.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::SocketAddr;

use bytes::{BufMut, Bytes, BytesMut};
use echomuse_core::types::DeviceId;
use echomuse_protocol::Platform;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::api::{
    ApiMessage, ButtonCommandRequest, MediaPlayerCommand, MediaPlayerCommandRequest,
    SubscribeVoiceAssistantRequest, VoiceAssistantAnnounceFinished, VoiceAssistantAnnounceRequest,
    VoiceAssistantAudio, VoiceAssistantAudioSettings, VoiceAssistantConfigurationRequest,
    VoiceAssistantConfigurationResponse, VoiceAssistantEvent, VoiceAssistantEventResponse,
    VoiceAssistantRequest, VoiceAssistantResponse, VoiceAssistantSetConfiguration,
    VoiceAssistantTimerEvent, VoiceAssistantTimerEventResponse,
};
use crate::entities::{
    self, ButtonEvent, DeviceVolume, MEDIA_PLAYER_KEY, MediaState, STOP_ALERT_KEY, VoiceEntities,
    VoiceState, VoiceStates,
};
use crate::frame::{self, Frame, FrameTooLarge};
use crate::identity::NodeIdentity;
use crate::server::{
    CloseReason, ConnId, Core, CoreEvent, FixedReplies, ServerStatus, ServerStopped, StartError,
    bind,
};

/// Bytes of 16 kHz 16-bit mono audio per chunk sent to Home Assistant: 20 ms.
pub const RUN_AUDIO_CHUNK_BYTES: usize = 640;

/// Run audio the node holds while Home Assistant has not yet accepted the
/// run: 15 s at 16 kHz, 16-bit mono.
const MAX_PENDING_RUN_AUDIO: usize = 15 * 16_000 * 2;

/// Commands queued from the handle to the node.
const COMMAND_CAPACITY: usize = 64;

/// How the runtime configures a satellite.
#[derive(Clone, Debug)]
pub struct VoiceSatelliteConfig {
    /// The device it presents.
    pub device: DeviceId,
    /// The device's label, which names the device and its entities.
    pub label: String,
    /// The image the device runs, which sets the model.
    pub platform: Platform,
    /// `ESPHOME_PROJECT_VERSION`.
    pub project_version: String,
    /// The address to listen on: the device's fixed port.
    pub address: SocketAddr,
    /// The optional entities the device's capabilities call for.
    pub entities: VoiceEntities,
    /// Capacity of the request channel to the runtime (at least 1).
    pub request_capacity: usize,
}

/// Identifies one controller-started voice run.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunId(u64);

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "run {}", self.0)
    }
}

/// Identifies one announcement Home Assistant asked for.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AnnouncementId(u64);

impl fmt::Display for AnnouncementId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "announcement {}", self.0)
    }
}

/// A media player command from Home Assistant.
#[derive(Clone, Debug, PartialEq)]
pub enum MediaRequest {
    /// Play `url`; `announcement` asks for it to play over current media.
    PlayUrl {
        /// What to play, usually a Home Assistant media proxy URL.
        url: String,
        /// Whether this is an announcement.
        announcement: bool,
    },
    /// Resume playback.
    Play,
    /// Pause playback.
    Pause,
    /// Stop playback.
    Stop,
    /// Mute output.
    Mute,
    /// Unmute output.
    Unmute,
    /// Set the volume.
    SetVolume(DeviceVolume),
}

/// An announcement Home Assistant asked the satellite to play. The runtime
/// reports the end with [`VoiceSatellite::finish_announcement`]; Home
/// Assistant waits for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Announcement {
    /// Names this announcement in the reply.
    pub id: AnnouncementId,
    /// What to play.
    pub media_url: String,
    /// The announcement's text, if Home Assistant gave one.
    pub text: Option<String>,
    /// A chime to play first, if any.
    pub preannounce_url: Option<String>,
    /// Whether to start a conversation once the announcement ends, as a
    /// controller-started run.
    pub start_conversation: bool,
}

/// What happened to one of the satellite's Home Assistant timers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TimerEventKind {
    /// The timer was set.
    Started,
    /// It was changed or paused.
    Updated,
    /// It was cancelled.
    Cancelled,
    /// It ran out.
    Finished,
}

/// A timer event from Home Assistant, which owns the satellite's timers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimerEvent {
    /// What happened.
    pub kind: TimerEventKind,
    /// Home Assistant's timer ID.
    pub timer_id: String,
    /// The timer's name, if it has one.
    pub name: Option<String>,
    /// The timer's full length in seconds.
    pub total_seconds: u32,
    /// Seconds left.
    pub seconds_left: u32,
    /// Whether it is counting down (not paused).
    pub is_active: bool,
}

/// How a controller-started run ended. Each run gets exactly one of these
/// unless the runtime ended it itself, by aborting it or starting another.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RunClosed {
    /// Home Assistant finished the pipeline.
    Finished,
    /// Home Assistant refused to start it.
    Refused,
    /// The connection or the voice-assistant subscription it ran on went away.
    Lost,
    /// Home Assistant did not accept the run before 15 s of audio had queued.
    AudioBacklog,
}

/// An event of a controller-started run, mapped from Home Assistant's
/// pipeline events.
#[derive(Clone, Debug, PartialEq)]
pub enum RunEvent {
    /// Home Assistant accepted the run; queued audio is on its way.
    Accepted,
    /// The pipeline started. `tts_url` is where the reply will stream from
    /// when Home Assistant knows it up front.
    Started {
        /// The streaming TTS URL, if given.
        tts_url: Option<String>,
    },
    /// Wake-word detection started (not used by these runs, which skip it).
    WakeWordStarted,
    /// Wake-word detection ended.
    WakeWordEnded,
    /// Speech recognition started.
    SttStarted,
    /// Voice activity detection heard speech start.
    VadStarted,
    /// Voice activity detection heard speech end.
    VadEnded,
    /// Speech recognition finished.
    SttEnded {
        /// The transcript.
        text: String,
    },
    /// Intent handling started.
    IntentStarted,
    /// Intent handling progressed; `tts_start_streaming` means the reply can
    /// start playing before intent handling ends.
    IntentProgress {
        /// Whether the TTS stream can start now.
        tts_start_streaming: bool,
    },
    /// Intent handling finished.
    IntentEnded {
        /// The conversation to continue, if any.
        conversation_id: Option<String>,
        /// Whether Home Assistant expects a follow-up reply.
        continue_conversation: bool,
        /// The spoken response text.
        speech: String,
    },
    /// Speech synthesis started.
    TtsStarted {
        /// The text being spoken.
        text: String,
    },
    /// Speech synthesis finished; `url` is the audio to play.
    TtsEnded {
        /// The reply audio, if there is any.
        url: Option<String>,
    },
    /// Streaming of TTS audio over the API started (only for speakers).
    TtsStreamStarted,
    /// Streaming of TTS audio over the API ended.
    TtsStreamEnded,
    /// The pipeline failed.
    Error {
        /// Home Assistant's error code.
        code: String,
        /// Its description.
        message: String,
    },
    /// An event type this schema does not name.
    Unknown(i32),
    /// The run ended; nothing more arrives for it.
    Closed(RunClosed),
}

/// Something Home Assistant asked of the device.
#[derive(Clone, Debug, PartialEq)]
pub enum SatelliteRequest {
    /// A media player command.
    Media(MediaRequest),
    /// The `stop_alert` button was pressed.
    StopAlert,
    /// Play an announcement.
    Announce(Announcement),
    /// A timer changed.
    Timer(TimerEvent),
    /// An event of a controller-started run.
    Run {
        /// The run it belongs to.
        run: RunId,
        /// What happened.
        event: RunEvent,
    },
}

/// The satellite has no such entity: the device lacks the capability.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("this satellite has no {0} entity")]
pub struct EntityAbsent(pub &'static str);

/// A button event could not be fired.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ButtonEventError {
    /// The device has no `action_button` entity.
    #[error(transparent)]
    EntityAbsent(#[from] EntityAbsent),
    /// The node has stopped.
    #[error(transparent)]
    Stopped(#[from] ServerStopped),
}

/// A run could not be started.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum StartRunError {
    /// No Home Assistant connection is subscribed to the voice assistant.
    #[error("Home Assistant is not subscribed to the voice assistant")]
    NotSubscribed,
    /// The conversation ID does not fit in a frame.
    #[error(transparent)]
    TooLarge(#[from] FrameTooLarge),
    /// The node has stopped.
    #[error(transparent)]
    Stopped(#[from] ServerStopped),
}

/// Home Assistant is no longer waiting for this playback to finish.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum FinishError {
    /// Nothing is waiting: unknown, already finished, or its connection is
    /// gone.
    #[error("Home Assistant is not waiting for this playback")]
    NotPending,
    /// The node has stopped.
    #[error(transparent)]
    Stopped(#[from] ServerStopped),
}

#[derive(Debug)]
enum Command {
    FireButton(ButtonEvent),
    StartRun {
        conversation_id: Option<String>,
        reply: oneshot::Sender<Result<RunId, StartRunError>>,
    },
    RunAudio {
        run: RunId,
        pcm: Bytes,
    },
    EndRunAudio(RunId),
    AbortRun(RunId),
    FinishRunResponse {
        run: RunId,
        success: bool,
        reply: oneshot::Sender<Result<(), FinishError>>,
    },
    FinishAnnouncement {
        id: AnnouncementId,
        success: bool,
        reply: oneshot::Sender<Result<(), FinishError>>,
    },
}

/// The runtime's handle on a started satellite. Dropping it stops the node
/// (without waiting); [`VoiceSatellite::shutdown`] stops it and waits.
#[derive(Debug)]
pub struct VoiceSatellite {
    commands: mpsc::Sender<Command>,
    states: watch::Sender<VoiceStates>,
    status: watch::Receiver<ServerStatus>,
    entities: VoiceEntities,
    local_addr: SocketAddr,
    shutdown: CancellationToken,
    task: Option<JoinHandle<()>>,
}

/// Starts a satellite listening on `config.address`. Must be called within
/// a Tokio runtime, on which the node then runs. Returns the handle and the
/// channel of Home Assistant's requests.
pub fn start_voice_satellite(
    config: VoiceSatelliteConfig,
) -> Result<(VoiceSatellite, mpsc::Receiver<SatelliteRequest>), StartError> {
    let identity = NodeIdentity::voice_satellite(
        &config.device,
        &config.label,
        config.platform,
        &config.project_version,
    );
    let replies = FixedReplies {
        hello: frame::encode(&identity.hello_response())?,
        device_info: frame::encode(&identity.device_info())?,
        entities: entities::voice_entity_frames(&config.label, config.entities)?,
    };
    let listener = bind(config.address)?;
    let local_addr = listener.local_addr().map_err(|source| StartError::Bind {
        address: config.address,
        source,
    })?;
    let shutdown = CancellationToken::new();
    let (status_tx, status_rx) = watch::channel(ServerStatus::default());
    let (states_tx, states_rx) = watch::channel(VoiceStates::default());
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (request_tx, request_rx) = mpsc::channel(config.request_capacity.max(1));
    let actor = Actor {
        core: Core::new(listener, replies, shutdown.child_token(), status_tx),
        entities: config.entities,
        states: states_rx,
        sent_states: VoiceStates::default(),
        commands: command_rx,
        requests: request_tx,
        runtime_gone: false,
        subscriber: None,
        run: None,
        response_owner: None,
        next_run: 0,
        announcements: HashMap::new(),
        next_announcement: 0,
        undelivered_closures: VecDeque::new(),
    };
    let task = tokio::spawn(actor.run(shutdown.clone()));
    Ok((
        VoiceSatellite {
            commands: command_tx,
            states: states_tx,
            status: status_rx,
            entities: config.entities,
            local_addr,
            shutdown,
            task: Some(task),
        },
        request_rx,
    ))
}

impl VoiceSatellite {
    /// The address the node listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The node's connection status, updated as it changes.
    pub fn status(&self) -> watch::Receiver<ServerStatus> {
        self.status.clone()
    }

    /// The optional entities this satellite has.
    pub fn entities(&self) -> VoiceEntities {
        self.entities
    }

    /// Sets the media player's state; `None` is unknown.
    pub fn set_media(&self, media: Option<MediaState>) {
        self.states
            .send_if_modified(|states| replace(&mut states.media, media));
    }

    /// Sets the ambient light in lux; `None` (or a value that is not a
    /// number) is no reading.
    pub fn set_ambient_light(&self, lux: Option<f32>) -> Result<(), EntityAbsent> {
        if !self.entities.ambient_light {
            return Err(EntityAbsent("ambient_light"));
        }
        let lux = lux.filter(|lux| lux.is_finite());
        self.states
            .send_if_modified(|states| replace(&mut states.ambient_light_lux, lux));
        Ok(())
    }

    /// Sets whether an alert is ringing; `None` is unknown.
    pub fn set_alert_ringing(&self, ringing: Option<bool>) {
        self.states
            .send_if_modified(|states| replace(&mut states.alert_ringing, ringing));
    }

    /// Sets the voice state; `None` is unknown.
    pub fn set_voice_state(&self, state: Option<VoiceState>) {
        self.states
            .send_if_modified(|states| replace(&mut states.voice_state, state));
    }

    /// Fires an `action_button` event to every client subscribed to states.
    /// Events are momentary: with no client subscribed it reaches no one.
    pub async fn fire_button_event(&self, event: ButtonEvent) -> Result<(), ButtonEventError> {
        if !self.entities.action_button {
            return Err(EntityAbsent("action_button").into());
        }
        self.command(Command::FireButton(event)).await?;
        Ok(())
    }

    /// Starts a controller-started run: `VoiceAssistantRequest(start,
    /// flags=0)` to the subscribed client, so Home Assistant runs its
    /// pipeline from speech recognition on. Any run still open is aborted
    /// first and gets no further events.
    pub async fn start_run(&self, conversation_id: Option<String>) -> Result<RunId, StartRunError> {
        let (reply, response) = oneshot::channel();
        self.command(Command::StartRun {
            conversation_id,
            reply,
        })
        .await?;
        response.await.map_err(|_| ServerStopped)?
    }

    /// Queues 16 kHz mono audio for `run`. It goes out in 640-byte chunks
    /// once Home Assistant has accepted the run. Audio for a run that is no
    /// longer current is discarded.
    pub async fn send_run_audio(&self, run: RunId, samples: &[i16]) -> Result<(), ServerStopped> {
        let mut pcm = BytesMut::with_capacity(samples.len() * 2);
        for sample in samples {
            pcm.put_i16_le(*sample);
        }
        self.command(Command::RunAudio {
            run,
            pcm: pcm.freeze(),
        })
        .await
    }

    /// Ends `run`'s audio: the last partial chunk, then an empty chunk
    /// marking the end.
    pub async fn end_run_audio(&self, run: RunId) -> Result<(), ServerStopped> {
        self.command(Command::EndRunAudio(run)).await
    }

    /// Aborts `run`: Home Assistant abandons the pipeline and sends nothing
    /// more for it. Its effects so far are not undone.
    pub async fn abort_run(&self, run: RunId) -> Result<(), ServerStopped> {
        self.command(Command::AbortRun(run)).await
    }

    /// Tells Home Assistant that `run`'s spoken reply has finished playing,
    /// which returns the satellite entity to idle.
    pub async fn finish_run_response(&self, run: RunId, success: bool) -> Result<(), FinishError> {
        let (reply, response) = oneshot::channel();
        self.command(Command::FinishRunResponse {
            run,
            success,
            reply,
        })
        .await?;
        response.await.map_err(|_| ServerStopped)?
    }

    /// Tells Home Assistant that announcement `id` has finished playing.
    pub async fn finish_announcement(
        &self,
        id: AnnouncementId,
        success: bool,
    ) -> Result<(), FinishError> {
        let (reply, response) = oneshot::channel();
        self.command(Command::FinishAnnouncement { id, success, reply })
            .await?;
        response.await.map_err(|_| ServerStopped)?
    }

    /// Stops listening, asks connected clients to disconnect and waits for
    /// the node to finish (a few seconds at most).
    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    async fn command(&self, command: Command) -> Result<(), ServerStopped> {
        self.commands.send(command).await.map_err(|_| ServerStopped)
    }
}

impl Drop for VoiceSatellite {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Sets `slot` to `value`, reporting whether it changed.
fn replace<T: PartialEq>(slot: &mut T, value: T) -> bool {
    if *slot == value {
        return false;
    }
    *slot = value;
    true
}

/// The controller-started run in progress.
#[derive(Debug)]
struct ActiveRun {
    id: RunId,
    conn: ConnId,
    /// Home Assistant answered the start without error.
    accepted: bool,
    /// The end-of-audio chunk has been produced.
    audio_ended: bool,
    /// Chunks waiting for Home Assistant to accept the run.
    pending: VecDeque<Bytes>,
    pending_bytes: usize,
    /// Audio short of a whole chunk.
    remainder: BytesMut,
}

struct Actor {
    core: Core,
    entities: VoiceEntities,
    states: watch::Receiver<VoiceStates>,
    /// The states clients were last sent.
    sent_states: VoiceStates,
    commands: mpsc::Receiver<Command>,
    requests: mpsc::Sender<SatelliteRequest>,
    /// The runtime dropped its request receiver.
    runtime_gone: bool,
    /// The connection subscribed to the voice assistant. The subscription's
    /// flags are not read: the schema numbers audio over the API 1, Home
    /// Assistant's client sends 4, and ESPHome ignores them. The satellite
    /// only offers audio over the API, so every subscriber uses it.
    subscriber: Option<ConnId>,
    run: Option<ActiveRun>,
    /// The run whose spoken reply Home Assistant may still be waiting on,
    /// and the connection it ran on.
    response_owner: Option<(RunId, ConnId)>,
    next_run: u64,
    announcements: HashMap<AnnouncementId, ConnId>,
    next_announcement: u64,
    /// Run closures the request queue had no room for, delivered as room
    /// frees so that every run's end reaches the runtime.
    undelivered_closures: VecDeque<SatelliteRequest>,
}

/// What a pipeline event's data says about `name`.
fn event_value<'a>(event: &'a VoiceAssistantEventResponse, name: &str) -> Option<&'a str> {
    event
        .data
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.value.as_str())
}

fn non_empty(text: Option<&str>) -> Option<String> {
    text.filter(|text| !text.is_empty()).map(str::to_owned)
}

/// Maps one Home Assistant pipeline event to a run event.
fn run_event(event: &VoiceAssistantEventResponse) -> RunEvent {
    use VoiceAssistantEvent as E;
    let Ok(kind) = E::try_from(event.event_type) else {
        return RunEvent::Unknown(event.event_type);
    };
    let text = |name| event_value(event, name).unwrap_or_default().to_owned();
    match kind {
        E::VoiceAssistantError => RunEvent::Error {
            code: text("code"),
            message: text("message"),
        },
        E::VoiceAssistantRunStart => RunEvent::Started {
            tts_url: non_empty(event_value(event, "url")),
        },
        E::VoiceAssistantRunEnd => RunEvent::Closed(RunClosed::Finished),
        E::VoiceAssistantSttStart => RunEvent::SttStarted,
        E::VoiceAssistantSttEnd => RunEvent::SttEnded { text: text("text") },
        E::VoiceAssistantIntentStart => RunEvent::IntentStarted,
        E::VoiceAssistantIntentProgress => RunEvent::IntentProgress {
            tts_start_streaming: event_value(event, "tts_start_streaming") == Some("1"),
        },
        E::VoiceAssistantIntentEnd => RunEvent::IntentEnded {
            conversation_id: non_empty(event_value(event, "conversation_id")),
            continue_conversation: event_value(event, "continue_conversation") == Some("1"),
            speech: text("speech"),
        },
        E::VoiceAssistantTtsStart => RunEvent::TtsStarted { text: text("text") },
        E::VoiceAssistantTtsEnd => RunEvent::TtsEnded {
            url: non_empty(event_value(event, "url")),
        },
        E::VoiceAssistantWakeWordStart => RunEvent::WakeWordStarted,
        E::VoiceAssistantWakeWordEnd => RunEvent::WakeWordEnded,
        E::VoiceAssistantSttVadStart => RunEvent::VadStarted,
        E::VoiceAssistantSttVadEnd => RunEvent::VadEnded,
        E::VoiceAssistantTtsStreamStart => RunEvent::TtsStreamStarted,
        E::VoiceAssistantTtsStreamEnd => RunEvent::TtsStreamEnded,
    }
}

fn audio_frame(data: Bytes, end: bool) -> Bytes {
    frame::encode(&VoiceAssistantAudio {
        data,
        end,
        data2: Bytes::new(),
    })
    .expect("a run audio chunk fits a frame")
}

fn stop_frame() -> Bytes {
    frame::encode(&VoiceAssistantRequest {
        start: false,
        ..Default::default()
    })
    .expect("an empty request fits a frame")
}

impl Actor {
    async fn run(mut self, shutdown: CancellationToken) {
        // A second sender, so waiting for room does not hold `self`.
        let requests = self.requests.clone();
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                command = self.commands.recv() => match command {
                    Some(command) => self.on_command(command),
                    None => break,
                },
                changed = self.states.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    self.push_state_changes();
                }
                permit = requests.reserve(), if !self.undelivered_closures.is_empty() => {
                    match permit {
                        Ok(permit) => {
                            let closure = self.undelivered_closures.pop_front().expect("guarded");
                            permit.send(closure);
                        }
                        Err(_) => self.runtime_gone = true,
                    }
                }
                event = self.core.next() => self.on_core(event),
            }
            if self.runtime_gone {
                break;
            }
        }
        self.core.shutdown().await;
    }

    /// Hands a request to the runtime. A full queue closes `from`, the
    /// connection the request came from.
    fn deliver(&mut self, request: SatelliteRequest, from: Option<ConnId>) {
        match self.requests.try_send(request) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                if let Some(conn) = from {
                    self.core.close(conn, CloseReason::RequestsBacklogged);
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => self.runtime_gone = true,
        }
    }

    fn deliver_run(&mut self, run: RunId, event: RunEvent, from: Option<ConnId>) {
        let closing = matches!(event, RunEvent::Closed(_));
        let request = SatelliteRequest::Run { run, event };
        if !closing {
            self.deliver(request, from);
            return;
        }
        // A run's end is never dropped: it waits for room instead, and
        // closures stay in order behind one another.
        if !self.undelivered_closures.is_empty() {
            self.undelivered_closures.push_back(request);
            return;
        }
        match self.requests.try_send(request) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(request)) => {
                self.undelivered_closures.push_back(request);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => self.runtime_gone = true,
        }
    }

    fn push_state_changes(&mut self) {
        let after = *self.states.borrow_and_update();
        let frames = entities::changed_voice_state_frames(self.entities, &self.sent_states, &after);
        self.sent_states = after;
        for frame in &frames {
            self.core.send_to_state_subscribers(frame);
        }
    }

    fn on_core(&mut self, event: CoreEvent) {
        match event {
            CoreEvent::SubscribedStates(conn) => {
                // Bring the existing subscribers up to date first, so the
                // snapshot the new one gets is the one they have.
                if self.states.has_changed().unwrap_or(false) {
                    self.push_state_changes();
                }
                for frame in entities::all_voice_state_frames(self.entities, &self.sent_states) {
                    self.core.send(conn, frame);
                }
            }
            CoreEvent::Message(conn, frame) => {
                if let Err(reason) = self.on_message(conn, &frame) {
                    self.core.close(conn, reason);
                }
            }
            CoreEvent::Closed(conn) => self.forget_connection(conn),
        }
    }

    fn forget_connection(&mut self, conn: ConnId) {
        if self.subscriber == Some(conn) {
            self.subscriber = None;
            self.core.set_subscribed(false);
        }
        self.lose_run_on(conn);
        if self.response_owner.is_some_and(|(_, owner)| owner == conn) {
            self.response_owner = None;
        }
        self.announcements.retain(|_, owner| *owner != conn);
    }

    /// Ends the run on `conn`, if any, as lost.
    fn lose_run_on(&mut self, conn: ConnId) {
        if let Some(run) = self.run.take_if(|run| run.conn == conn) {
            self.deliver_run(run.id, RunEvent::Closed(RunClosed::Lost), None);
        }
    }

    fn on_message(&mut self, conn: ConnId, frame: &Frame) -> Result<(), CloseReason> {
        fn decode<M: ApiMessage>(frame: &Frame) -> Result<M, CloseReason> {
            frame.decode::<M>().map_err(|_| CloseReason::Malformed)
        }
        match frame.message_type {
            MediaPlayerCommandRequest::ID => {
                let command = decode::<MediaPlayerCommandRequest>(frame)?;
                if command.key == MEDIA_PLAYER_KEY {
                    self.on_media_command(conn, &command);
                }
            }
            ButtonCommandRequest::ID => {
                if decode::<ButtonCommandRequest>(frame)?.key == STOP_ALERT_KEY {
                    self.deliver(SatelliteRequest::StopAlert, Some(conn));
                }
            }
            SubscribeVoiceAssistantRequest::ID => {
                let request = decode::<SubscribeVoiceAssistantRequest>(frame)?;
                self.on_voice_subscription(conn, &request);
            }
            VoiceAssistantResponse::ID => {
                let response = decode::<VoiceAssistantResponse>(frame)?;
                self.on_run_response(conn, &response);
            }
            VoiceAssistantEventResponse::ID => {
                let event = decode::<VoiceAssistantEventResponse>(frame)?;
                self.on_run_event(conn, &event);
            }
            VoiceAssistantTimerEventResponse::ID => {
                let event = decode::<VoiceAssistantTimerEventResponse>(frame)?;
                // An event type this schema does not name has nothing to map to.
                if let Ok(kind) = VoiceAssistantTimerEvent::try_from(event.event_type) {
                    let kind = match kind {
                        VoiceAssistantTimerEvent::VoiceAssistantTimerStarted => {
                            TimerEventKind::Started
                        }
                        VoiceAssistantTimerEvent::VoiceAssistantTimerUpdated => {
                            TimerEventKind::Updated
                        }
                        VoiceAssistantTimerEvent::VoiceAssistantTimerCancelled => {
                            TimerEventKind::Cancelled
                        }
                        VoiceAssistantTimerEvent::VoiceAssistantTimerFinished => {
                            TimerEventKind::Finished
                        }
                    };
                    let timer = TimerEvent {
                        kind,
                        timer_id: event.timer_id,
                        name: non_empty(Some(&event.name)),
                        total_seconds: event.total_seconds,
                        seconds_left: event.seconds_left,
                        is_active: event.is_active,
                    };
                    self.deliver(SatelliteRequest::Timer(timer), Some(conn));
                }
            }
            VoiceAssistantAnnounceRequest::ID => {
                let request = decode::<VoiceAssistantAnnounceRequest>(frame)?;
                let id = AnnouncementId(self.next_announcement);
                self.next_announcement += 1;
                self.announcements.insert(id, conn);
                let announcement = Announcement {
                    id,
                    media_url: request.media_id,
                    text: non_empty(Some(&request.text)),
                    preannounce_url: non_empty(Some(&request.preannounce_media_id)),
                    start_conversation: request.start_conversation,
                };
                self.deliver(SatelliteRequest::Announce(announcement), Some(conn));
            }
            VoiceAssistantConfigurationRequest::ID => {
                decode::<VoiceAssistantConfigurationRequest>(frame)?;
                // The wake word is the device's own model, chosen by the
                // controller, so Home Assistant has none to offer or enable.
                let reply = VoiceAssistantConfigurationResponse {
                    available_wake_words: Vec::new(),
                    active_wake_words: Vec::new(),
                    max_active_wake_words: 0,
                };
                self.core.send(
                    conn,
                    frame::encode(&reply).expect("an empty configuration fits a frame"),
                );
            }
            // With no wake words to choose from, there is nothing to set.
            VoiceAssistantSetConfiguration::ID => {
                decode::<VoiceAssistantSetConfiguration>(frame)?;
            }
            // Anything else is a message this node has no entity or feature
            // for; ESPHome ignores those too.
            _ => {}
        }
        Ok(())
    }

    fn on_media_command(&mut self, conn: ConnId, command: &MediaPlayerCommandRequest) {
        // ESPHome applies the parts of a combined command in this order.
        if command.has_command {
            let request = match MediaPlayerCommand::try_from(command.command) {
                Ok(MediaPlayerCommand::Play) => Some(MediaRequest::Play),
                Ok(MediaPlayerCommand::Pause) => Some(MediaRequest::Pause),
                Ok(MediaPlayerCommand::Stop) => Some(MediaRequest::Stop),
                Ok(MediaPlayerCommand::Mute) => Some(MediaRequest::Mute),
                Ok(MediaPlayerCommand::Unmute) => Some(MediaRequest::Unmute),
                // The media player does not advertise these features, so
                // Home Assistant does not send them.
                _ => None,
            };
            if let Some(request) = request {
                self.deliver(SatelliteRequest::Media(request), Some(conn));
            }
        }
        if command.has_volume
            && let Some(volume) = DeviceVolume::from_home_assistant(command.volume)
        {
            self.deliver(
                SatelliteRequest::Media(MediaRequest::SetVolume(volume)),
                Some(conn),
            );
        }
        if command.has_media_url {
            let request = MediaRequest::PlayUrl {
                url: command.media_url.clone(),
                announcement: command.has_announcement && command.announcement,
            };
            self.deliver(SatelliteRequest::Media(request), Some(conn));
        }
    }

    fn on_voice_subscription(&mut self, conn: ConnId, request: &SubscribeVoiceAssistantRequest) {
        if request.subscribe {
            // The newest subscriber wins, as ESPHome's Bluetooth proxy
            // decides: an older one is almost always a dead connection that
            // has not timed out yet.
            if let Some(previous) = self.subscriber
                && previous != conn
            {
                self.lose_run_on(previous);
            }
            self.subscriber = Some(conn);
            self.core.set_subscribed(true);
        } else if self.subscriber == Some(conn) {
            self.subscriber = None;
            self.core.set_subscribed(false);
            self.lose_run_on(conn);
        }
    }

    fn on_run_response(&mut self, conn: ConnId, response: &VoiceAssistantResponse) {
        let Some(run) = self
            .run
            .as_mut()
            .filter(|run| run.conn == conn && !run.accepted)
        else {
            // No run is waiting for an answer: a late reply to one already
            // aborted.
            return;
        };
        let id = run.id;
        if response.error {
            self.run = None;
            if self.response_owner.is_some_and(|(owner, _)| owner == id) {
                self.response_owner = None;
            }
            self.deliver_run(id, RunEvent::Closed(RunClosed::Refused), Some(conn));
            return;
        }
        run.accepted = true;
        let pending: Vec<Bytes> = run.pending.drain(..).collect();
        run.pending_bytes = 0;
        self.deliver_run(id, RunEvent::Accepted, Some(conn));
        for chunk in pending {
            if !self.core.send(conn, chunk) {
                break;
            }
        }
    }

    fn on_run_event(&mut self, conn: ConnId, event: &VoiceAssistantEventResponse) {
        // Events that arrive before Home Assistant has accepted the current
        // run belong to the one it replaced: Home Assistant answers a start
        // only after it has abandoned the previous pipeline.
        let Some(run) = self
            .run
            .as_ref()
            .filter(|run| run.conn == conn && run.accepted)
        else {
            return;
        };
        let id = run.id;
        let event = run_event(event);
        if event == RunEvent::Closed(RunClosed::Finished) {
            self.run = None;
        }
        self.deliver_run(id, event, Some(conn));
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::FireButton(event) => {
                self.core
                    .send_to_state_subscribers(&entities::button_event_frame(event));
            }
            Command::StartRun {
                conversation_id,
                reply,
            } => {
                let _ = reply.send(self.start_run(conversation_id));
            }
            Command::RunAudio { run, pcm } => self.run_audio(run, &pcm),
            Command::EndRunAudio(run) => self.end_run_audio(run),
            Command::AbortRun(run) => {
                if let Some(active) = self.run.take_if(|active| active.id == run) {
                    self.core.send(active.conn, stop_frame());
                }
                if self.response_owner.is_some_and(|(owner, _)| owner == run) {
                    self.response_owner = None;
                }
            }
            Command::FinishRunResponse {
                run,
                success,
                reply,
            } => {
                let result = match self.response_owner {
                    Some((owner, conn)) if owner == run => {
                        self.response_owner = None;
                        self.send_finished(conn, success)
                    }
                    _ => Err(FinishError::NotPending),
                };
                let _ = reply.send(result);
            }
            Command::FinishAnnouncement { id, success, reply } => {
                let result = match self.announcements.remove(&id) {
                    Some(conn) => self.send_finished(conn, success),
                    None => Err(FinishError::NotPending),
                };
                let _ = reply.send(result);
            }
        }
    }

    fn send_finished(&mut self, conn: ConnId, success: bool) -> Result<(), FinishError> {
        let frame = frame::encode(&VoiceAssistantAnnounceFinished { success })
            .expect("a fixed-size message fits a frame");
        if self.core.send(conn, frame) {
            Ok(())
        } else {
            Err(FinishError::NotPending)
        }
    }

    fn start_run(&mut self, conversation_id: Option<String>) -> Result<RunId, StartRunError> {
        let subscriber = self
            .subscriber
            .filter(|conn| self.core.is_open(*conn))
            .ok_or(StartRunError::NotSubscribed)?;
        let request = frame::encode(&VoiceAssistantRequest {
            start: true,
            conversation_id: conversation_id.unwrap_or_default(),
            flags: 0,
            // ESPHome's defaults: no noise suppression, no gain, unity volume.
            audio_settings: Some(VoiceAssistantAudioSettings {
                noise_suppression_level: 0,
                auto_gain: 0,
                volume_multiplier: 1.0,
            }),
            wake_word_phrase: String::new(),
        })?;
        if let Some(previous) = self.run.take()
            && self.core.is_open(previous.conn)
        {
            self.core.send(previous.conn, stop_frame());
        }
        if !self.core.send(subscriber, request) {
            return Err(StartRunError::NotSubscribed);
        }
        let id = RunId(self.next_run);
        self.next_run += 1;
        self.run = Some(ActiveRun {
            id,
            conn: subscriber,
            accepted: false,
            audio_ended: false,
            pending: VecDeque::new(),
            pending_bytes: 0,
            remainder: BytesMut::new(),
        });
        self.response_owner = Some((id, subscriber));
        Ok(id)
    }

    fn run_audio(&mut self, run: RunId, pcm: &[u8]) {
        let Some(active) = self.run.as_mut().filter(|a| a.id == run && !a.audio_ended) else {
            return;
        };
        active.remainder.extend_from_slice(pcm);
        let mut chunks = Vec::new();
        while active.remainder.len() >= RUN_AUDIO_CHUNK_BYTES {
            let chunk = active.remainder.split_to(RUN_AUDIO_CHUNK_BYTES).freeze();
            chunks.push(audio_frame(chunk, false));
        }
        self.emit_run_audio(chunks);
    }

    fn end_run_audio(&mut self, run: RunId) {
        let Some(active) = self.run.as_mut().filter(|a| a.id == run && !a.audio_ended) else {
            return;
        };
        active.audio_ended = true;
        let mut chunks = Vec::new();
        if !active.remainder.is_empty() {
            chunks.push(audio_frame(active.remainder.split().freeze(), false));
        }
        chunks.push(audio_frame(Bytes::new(), true));
        self.emit_run_audio(chunks);
    }

    /// Sends audio frames of the current run, or holds them until Home
    /// Assistant accepts it.
    fn emit_run_audio(&mut self, frames: Vec<Bytes>) {
        let Some(active) = self.run.as_mut() else {
            return;
        };
        let (id, conn) = (active.id, active.conn);
        if active.accepted {
            for frame in frames {
                if !self.core.send(conn, frame) {
                    return;
                }
            }
            return;
        }
        for frame in frames {
            active.pending_bytes += frame.len();
            active.pending.push_back(frame);
        }
        if active.pending_bytes > MAX_PENDING_RUN_AUDIO {
            self.run = None;
            self.core.send(conn, stop_frame());
            if self.response_owner.is_some_and(|(owner, _)| owner == id) {
                self.response_owner = None;
            }
            self.deliver_run(id, RunEvent::Closed(RunClosed::AudioBacklog), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{VoiceAssistantEvent, VoiceAssistantEventData};

    fn event(kind: VoiceAssistantEvent, data: &[(&str, &str)]) -> VoiceAssistantEventResponse {
        VoiceAssistantEventResponse {
            event_type: kind as i32,
            data: data
                .iter()
                .map(|(name, value)| VoiceAssistantEventData {
                    name: (*name).to_owned(),
                    value: (*value).to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn pipeline_events_map_to_typed_results() {
        use VoiceAssistantEvent as E;
        assert_eq!(
            run_event(&event(
                E::VoiceAssistantSttEnd,
                &[("text", "turn on the lights")]
            )),
            RunEvent::SttEnded {
                text: "turn on the lights".to_owned()
            }
        );
        assert_eq!(
            run_event(&event(
                E::VoiceAssistantIntentEnd,
                &[
                    ("conversation_id", "01J"),
                    ("continue_conversation", "1"),
                    ("speech", "Which room?")
                ]
            )),
            RunEvent::IntentEnded {
                conversation_id: Some("01J".to_owned()),
                continue_conversation: true,
                speech: "Which room?".to_owned()
            }
        );
        assert_eq!(
            run_event(&event(
                E::VoiceAssistantIntentEnd,
                &[("conversation_id", ""), ("continue_conversation", "0")]
            )),
            RunEvent::IntentEnded {
                conversation_id: None,
                continue_conversation: false,
                speech: String::new()
            }
        );
        assert_eq!(
            run_event(&event(
                E::VoiceAssistantTtsEnd,
                &[("url", "http://ha/tts.flac")]
            )),
            RunEvent::TtsEnded {
                url: Some("http://ha/tts.flac".to_owned())
            }
        );
        assert_eq!(
            run_event(&event(E::VoiceAssistantTtsEnd, &[])),
            RunEvent::TtsEnded { url: None }
        );
        assert_eq!(
            run_event(&event(
                E::VoiceAssistantError,
                &[("code", "stt-no-text-recognized"), ("message", "No text")]
            )),
            RunEvent::Error {
                code: "stt-no-text-recognized".to_owned(),
                message: "No text".to_owned()
            }
        );
        assert_eq!(
            run_event(&event(
                E::VoiceAssistantIntentProgress,
                &[("tts_start_streaming", "1")]
            )),
            RunEvent::IntentProgress {
                tts_start_streaming: true
            }
        );
        assert_eq!(
            run_event(&event(E::VoiceAssistantRunEnd, &[])),
            RunEvent::Closed(RunClosed::Finished)
        );
        assert_eq!(
            run_event(&VoiceAssistantEventResponse {
                event_type: 57,
                data: Vec::new()
            }),
            RunEvent::Unknown(57)
        );
    }
}
