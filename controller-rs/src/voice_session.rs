//! A device link session's voice path in the device runtime: the [`Voice`] machine and
//! the work it asks for (spec §5.1, §5.2, §5.4).
//!
//! The runtime hands the machine every relevant input in order and calls
//! `VoiceSession::execute` after each one. Effects are carried out without the runtime
//! ever waiting: messages go to the link at once; acknowledged requests, speech-worker
//! jobs, Home Assistant requests, decodes, alert operations and records writes run as
//! tasks on the runtime's task set, and post their answers back as `VoiceMessage`s the
//! runtime feeds to the machine. Every answer quotes the identity of what asked for it
//! (a call id, a message id, a stream key, a turn), and the machine drops stale ones.
//! Records writes are not tied to the session: a turn that ends with its session is
//! still written.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use echomuse_alerts::{
    AlertHandle, RingsSnapshot, TimerViews, schedule::HaTimeZone, wire::TimerView,
};
use echomuse_core::{
    services::{
        DirectIntentRequest, HomeAssistant, IntentRequest, RecognizeRequest, RunEvent, SttRequest,
        TtsRequest,
    },
    types::{DeviceId, RuntimeError, SessionId, WorkScope},
};
use echomuse_esphome::entities::DeviceVolume;
use echomuse_grammar::Grammar;
use echomuse_ha::HaClient;
use echomuse_link::{CommandFailure, PendingAck, SendError, SessionLink};
use echomuse_playback::{
    AckFailure, AckOutcome, Command as PlaybackCommand, DecodeEvent, DecodeRequest, DecodeStream,
    Decoder, Equalizer, RenderFrame, eq::RENDER_SAMPLE_RATE, resolve_media_url,
};
use echomuse_presentation::ring::wire::LedCommand;
use echomuse_protocol::{Capabilities, MessageId};
use echomuse_records::RecordsService;
use echomuse_settings::service::{DeviceSettingsWatch, SettingsHandle};
use echomuse_speech::{BcresnetModelFiles, PinnedFile, Sha256Digest};
use echomuse_uplink::UplinkCommand;
use echomuse_voice::{
    AlertReply, AlertRequest, CallId, DecodeCommand, DecodeReport, Effect, HaReply, HaRequest,
    Input, Now, Outbound, PersistReply, PersistRequest, PreviewId, PreviewOutcome,
    SatelliteCommand, Voice, VoiceConfig, VoiceStatus, WakeModelInfo,
    arbiter::WakeArbiter,
    speech::{
        MicJob, ReferenceJob, mic::MicAnalysis, models::ModelSet, reference::ReferenceScoring,
        span::SpanDecoding, verify::WakeVerification,
    },
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
};

use crate::{
    esphome_node::EsphomeNodes,
    speech_models::SpeechStatus,
    speech_workers::{SpeechWorkerHandle, WorkerSlot},
};

/// How long the device has to acknowledge a command before its outcome is unknown.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// Render frames queued for the audio socket; the render client never keeps more than
/// about 70 ahead of the device, so a full queue means the socket is stuck.
const RENDER_QUEUE: usize = 256;
/// Answers waiting for the runtime.
pub(crate) const MESSAGE_CAPACITY: usize = 4_096;
/// How many alarms a voice request reads.
const ALARM_LIMIT: usize = 64;
/// How long a failed lookup of HA's time zone or the satellite's device waits to retry.
const LOOKUP_RETRY: Duration = Duration::from_secs(10);

/// The shared services every device's voice path uses. Each device runtime clones it
/// with its own speech status (see `device::SpeechFeed`).
#[derive(Clone)]
pub struct VoiceServices {
    pub ha: HaClient,
    /// Home Assistant's base URL, for media URLs it gives relative to itself.
    pub ha_base: reqwest::Url,
    pub alerts: AlertHandle,
    pub records: RecordsService,
    pub settings: SettingsHandle,
    /// Compiled once; clones share it.
    pub grammar: Grammar,
    pub arbiter: Arc<WakeArbiter>,
    pub speech: watch::Receiver<SpeechStatus>,
    pub decoder: Decoder,
    /// Transcript correction's published tables and preference compiler.
    pub correction: crate::correction::CorrectionTables,
}

/// An answer for a session's machine, posted by a task.
#[expect(
    clippy::large_enum_variant,
    reason = "inputs are nearly every message; boxing them would allocate per answer"
)]
pub(crate) enum VoiceMessage {
    Input {
        session: SessionId,
        input: Input,
    },
    Decoded {
        session: SessionId,
        call: CallId,
        stream: DecodeStream,
        event: Option<DecodeEvent>,
    },
}

/// What changed among the shared state a session's machine follows.
pub(crate) enum WatchChange {
    Rings(Arc<RingsSnapshot>),
    Settings(Arc<echomuse_settings::DeviceSettings>),
    Speech,
    /// This device's running timers.
    Timers(TimerView),
    /// New transcript correction tables were published (or withdrawn).
    Tables,
}

/// The shared state a session's machine follows. Each watch is followed until its
/// sender is gone: the speech status's sender is dropped once loading settles, and the
/// rings and settings must still reach the machine after that.
pub(crate) struct VoiceWatch {
    rings: watch::Receiver<Arc<RingsSnapshot>>,
    settings: DeviceSettingsWatch,
    speech: watch::Receiver<SpeechStatus>,
    /// Every speaker's timers; only this device's are passed on, when they change.
    timers: watch::Receiver<Arc<TimerViews>>,
    device: DeviceId,
    timers_seen: Option<TimerView>,
    rings_open: bool,
    settings_open: bool,
    speech_open: bool,
    timers_open: bool,
    tables: watch::Receiver<Option<Arc<echomuse_correction::Tables>>>,
    tables_open: bool,
}

impl VoiceWatch {
    /// This device's timers in the latest view (empty when it has none).
    fn own_timers(&mut self) -> Option<TimerView> {
        let view = self
            .timers
            .borrow_and_update()
            .get(&self.device.0)
            .cloned()
            .unwrap_or(TimerView { timers: Vec::new() });
        if self.timers_seen.as_ref() == Some(&view) {
            return None;
        }
        self.timers_seen = Some(view.clone());
        Some(view)
    }

    pub(crate) async fn changed(&mut self) -> WatchChange {
        loop {
            if !(self.rings_open
                || self.settings_open
                || self.speech_open
                || self.timers_open
                || self.tables_open)
            {
                return std::future::pending().await;
            }
            tokio::select! {
                changed = self.rings.changed(), if self.rings_open => match changed {
                    Ok(()) => {
                        return WatchChange::Rings(Arc::clone(&self.rings.borrow_and_update()));
                    }
                    Err(_) => self.rings_open = false,
                },
                changed = self.settings.changed(), if self.settings_open => match changed {
                    Ok(settings) => return WatchChange::Settings(settings),
                    Err(_) => self.settings_open = false,
                },
                changed = self.speech.changed(), if self.speech_open => match changed {
                    Ok(()) => return WatchChange::Speech,
                    Err(_) => self.speech_open = false,
                },
                changed = self.timers.changed(), if self.timers_open => match changed {
                    Ok(()) => {
                        if let Some(view) = self.own_timers() {
                            return WatchChange::Timers(view);
                        }
                    }
                    Err(_) => self.timers_open = false,
                },
                changed = self.tables.changed(), if self.tables_open => match changed {
                    Ok(()) => return WatchChange::Tables,
                    Err(_) => self.tables_open = false,
                },
            }
        }
    }
}

enum Decode {
    Idle(DecodeStream),
    Pulling,
    Cancelled,
}

/// Everything a session's machine needs to start.
pub(crate) struct VoiceStart {
    pub(crate) services: Arc<VoiceServices>,
    pub(crate) device: DeviceId,
    pub(crate) session: SessionId,
    pub(crate) link: SessionLink,
    pub(crate) capabilities: Capabilities,
    pub(crate) afe_metadata: bool,
    pub(crate) muted: bool,
    pub(crate) volume: Option<DeviceVolume>,
    pub(crate) session_scope: WorkScope,
    pub(crate) capture: Option<WorkScope>,
    /// Whether `session.ready` granted capture.
    pub(crate) capture_permitted: bool,
    /// The device row's Kiosk Satellite token; `None` while none is stored.
    pub(crate) kiosk_token: Option<String>,
    pub(crate) workers: [SpeechWorkerHandle; 4],
    pub(crate) messages: mpsc::Sender<VoiceMessage>,
}

pub(crate) struct VoiceSession {
    session: SessionId,
    device: DeviceId,
    voice: Voice,
    link: SessionLink,
    services: Arc<VoiceServices>,
    workers: [SpeechWorkerHandle; 4],
    session_scope: WorkScope,
    messages: mpsc::Sender<VoiceMessage>,
    decodes: HashMap<CallId, Decode>,
    render: mpsc::Sender<RenderFrame>,
    models: Option<Arc<dyn ModelSet>>,
    capture_permitted: bool,
    pub(crate) watch: VoiceWatch,
    /// Each worker's answers in the order its jobs were submitted, which is the order
    /// the worker runs them. A task per job could deliver a later job's answer first
    /// (one already done when first polled overtakes one woken later), and the turn's
    /// evidence takes ASR results only in stream order.
    answers: [mpsc::UnboundedSender<PendingAnswer>; 4],
    /// Sound preview requests waiting for the machine's answer.
    previews: HashMap<PreviewId, oneshot::Sender<PreviewOutcome>>,
    next_preview: u64,
    /// Where the recording modes' audio goes while a mode is armed.
    diagnostic: Option<mpsc::Sender<echomuse_voice::DiagnosticAudio>>,
    /// A disarmed recorder still owed its subscription's end; the chunks and the end of
    /// the subscription that is closing go here.
    diagnostic_closing: Option<mpsc::Sender<echomuse_voice::DiagnosticAudio>>,
    /// This speaker's correction snapshot (the published tables and its preferences),
    /// with the number of the compile that produced it: a compile finishing after a
    /// newer one started is not kept.
    snapshot: Arc<watch::Sender<(u64, Option<echomuse_correction::Snapshot>)>>,
    snapshot_compiles: u64,
    /// The records of correction checks running on beside their turns, by correction
    /// call: each goes to the records service with its turn's row.
    fix_records: HashMap<CallId, echomuse_records::FixFollows>,
    /// Decision evidence the machine queued while a session recording runs, for the
    /// runtime to hand to the recorder ([`VoiceSession::take_evidence`]).
    evidence: Vec<(
        echomuse_turn::MonoInstant,
        echomuse_voice::evidence::Evidence,
    )>,
}

/// A speech job's answer once its work returns; `None` when it was cancelled.
type PendingAnswer = std::pin::Pin<Box<dyn Future<Output = Option<Input>> + Send>>;

/// Posts one worker's answers to the runtime in the order they were queued. Ends when
/// the session's machine is gone or the runtime stops listening.
async fn forward_answers(
    session: SessionId,
    mut pending: mpsc::UnboundedReceiver<PendingAnswer>,
    messages: mpsc::Sender<VoiceMessage>,
) -> crate::device::Internal {
    while let Some(answer) = pending.recv().await {
        if let Some(input) = answer.await
            && messages
                .send(VoiceMessage::Input { session, input })
                .await
                .is_err()
        {
            break;
        }
    }
    crate::device::Internal::VoiceTask
}

fn now() -> Now {
    Now {
        at: tokio::time::Instant::now(),
        wall_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
            }),
    }
}

fn models_of(status: &SpeechStatus) -> Option<Arc<dyn ModelSet>> {
    match status {
        SpeechStatus::Ready(models) => Some(Arc::clone(models)),
        SpeechStatus::Loading | SpeechStatus::Unavailable(_) => None,
    }
}

/// The device's selected wake model, as the registry knows it.
fn wake_model(settings: &SettingsHandle, device: &DeviceId) -> Option<WakeModelInfo> {
    let snapshot = settings.snapshot();
    let model = snapshot.selected_model(device).ok()?;
    let registry = snapshot.registry();
    let pinned = |file: &str, sha: &echomuse_protocol::Sha256| {
        Sha256Digest::parse_hex(sha.as_str())
            .ok()
            .map(|digest| PinnedFile::new(registry.dir().join(file), digest))
    };
    let files = pinned(model.graph_file(), model.graph_sha256())
        .zip(pinned(model.sidecar_file(), model.sidecar_sha256()))
        .map(|(graph, sidecar)| BcresnetModelFiles { graph, sidecar })
        .filter(|_| model.is_usable());
    let thresholds = model.thresholds();
    Some(WakeModelInfo {
        graph_sha256: model.graph_sha256().clone(),
        name: echomuse_records::WakeModelName::new(model.wake_phrase().as_str()),
        phrase: model.wake_phrase().as_str().to_owned(),
        verify_core: model.verify_core().as_str().to_owned(),
        files,
        thresholds: Some((thresholds.idle(), thresholds.playback())),
    })
}

macro_rules! each_body {
    ($body:expr, $b:ident => $e:expr) => {
        match $body {
            Outbound::Uplink(UplinkCommand::Open($b)) => $e,
            Outbound::Uplink(UplinkCommand::Renew($b)) => $e,
            Outbound::Uplink(UplinkCommand::Close($b)) => $e,
            Outbound::Playback(PlaybackCommand::RenderStart($b)) => $e,
            Outbound::Playback(PlaybackCommand::RenderEnd($b)) => $e,
            Outbound::Playback(PlaybackCommand::RenderCancel($b)) => $e,
            Outbound::Playback(PlaybackCommand::FocusAcquire($b)) => $e,
            Outbound::Playback(PlaybackCommand::FocusRenew($b)) => $e,
            Outbound::Playback(PlaybackCommand::FocusRelease($b)) => $e,
            Outbound::Led(LedCommand::Frame($b)) => $e,
            Outbound::Led(LedCommand::Anim($b)) => $e,
            Outbound::TurnView($b) => $e,
            Outbound::CandidateAck($b) => $e,
            Outbound::VolumeSet($b) => $e,
            Outbound::Config($b) => $e,
        }
    };
}

impl VoiceSession {
    /// Starts a session's voice path; `None` when the device has no usable wake model in
    /// the registry (the session then runs without one, which admission prevents).
    pub(crate) fn start(start: VoiceStart, tasks: &mut JoinSet<crate::device::Internal>) -> Self {
        let VoiceStart {
            services,
            device,
            session,
            link,
            capabilities,
            afe_metadata,
            muted,
            volume,
            session_scope,
            capture,
            capture_permitted,
            kiosk_token,
            workers,
            messages,
        } = start;
        let status = services.speech.borrow().clone();
        let models = models_of(&status);
        let settings = services.settings.watch_device(device.clone());
        let wake = wake_model(&services.settings, &device).unwrap_or_else(|| WakeModelInfo {
            graph_sha256: services
                .settings
                .effective(&device)
                .wakeword
                .wake_model
                .0
                .clone(),
            name: None,
            phrase: "ophelia".to_owned(),
            verify_core: "ophel".to_owned(),
            files: None,
            thresholds: None,
        });
        let rings = services.alerts.rings();
        let mut timers = services.alerts.timer_views();
        // The device's timers at attach reach the machine as the watch's first change.
        timers.mark_changed();
        // The speaker's preferences are compiled against the tables at attach as the
        // tables watch's first change.
        let mut tables = services.correction.tables();
        tables.mark_changed();
        let voice = Voice::new(
            VoiceConfig {
                device: device.clone(),
                session,
                capabilities,
                afe_metadata,
                muted,
                volume,
                wake,
                grammar: services.grammar.clone(),
                arbiter: Arc::clone(&services.arbiter),
                settings: settings.current(),
                session_scope: session_scope.clone(),
                capture,
                speech_ready: models.is_some() && capture_permitted,
                rings: Arc::clone(&rings.borrow()),
                time_zone: None,
                ha_device: None,
                kiosk_token,
            },
            now(),
        );
        let (render, frames) = mpsc::channel::<RenderFrame>(RENDER_QUEUE);
        let writer = link.clone();
        tasks.spawn(async move {
            write_frames(writer, frames).await;
            crate::device::Internal::VoiceTask
        });
        let answers = std::array::from_fn(|_| {
            let (sender, pending) = mpsc::unbounded_channel();
            tasks.spawn(forward_answers(session, pending, messages.clone()));
            sender
        });
        let started = Self {
            session,
            device: device.clone(),
            voice,
            link,
            services: Arc::clone(&services),
            workers,
            session_scope,
            messages,
            decodes: HashMap::new(),
            render,
            models,
            capture_permitted,
            watch: VoiceWatch {
                rings,
                settings,
                speech: services.speech.clone(),
                timers,
                device,
                timers_seen: None,
                rings_open: true,
                settings_open: true,
                speech_open: true,
                timers_open: true,
                tables,
                tables_open: true,
            },
            answers,
            previews: HashMap::new(),
            next_preview: 0,
            diagnostic: None,
            diagnostic_closing: None,
            snapshot: Arc::new(watch::channel((0, None)).0),
            snapshot_compiles: 0,
            fix_records: HashMap::new(),
            evidence: Vec::new(),
        };
        started.lookups(tasks);
        started
    }

    /// The session broke the stream rules (or ended): its ingress is ignored.
    pub(crate) fn is_lost(&self) -> bool {
        self.voice.is_lost()
    }

    /// Looks up HA's time zone and the satellite's HA device, retrying until HA answers
    /// or the session ends.
    fn lookups(&self, tasks: &mut JoinSet<crate::device::Internal>) {
        let ha = self.services.ha.clone();
        let messages = self.messages.clone();
        let session = self.session;
        let mac = echomuse_discovery::voice_mac(&self.device).to_string();
        let ended = self.link.cancellation();
        tasks.spawn(async move {
            let lookup = async {
                let mut zone = None;
                let mut device = None;
                loop {
                    let mut status = ha.status();
                    if status.wait_for(|s| s.generation.is_some()).await.is_err() {
                        return;
                    }
                    if zone.is_none()
                        && let Ok(name) = ha.time_zone().await
                    {
                        zone = HaTimeZone::named(&name).ok();
                        if zone.is_some() {
                            let input = Input::TimeZone(zone.clone());
                            let _ = messages.send(VoiceMessage::Input { session, input }).await;
                        }
                    }
                    if device.is_none()
                        && let Ok(found) = ha.device_with_mac(&mac).await
                    {
                        device = Some(found.clone());
                        let input = Input::HaDevice(found);
                        let _ = messages.send(VoiceMessage::Input { session, input }).await;
                    }
                    if zone.is_some() && device.is_some() {
                        return;
                    }
                    tokio::time::sleep(LOOKUP_RETRY).await;
                }
            };
            tokio::select! {
                () = ended.cancelled() => {}
                () = lookup => {}
            }
            crate::device::Internal::VoiceTask
        });
    }

    pub(crate) fn status(&self) -> VoiceStatus {
        self.voice.status()
    }

    /// Says whether a session recording runs: only then does the machine queue its
    /// decision evidence.
    pub(crate) fn set_recording(&mut self, recording: bool) {
        self.voice.set_recording(recording);
        if !recording {
            self.evidence.clear();
        }
    }

    /// The decision evidence queued since the last call, with the machine's clock origin
    /// its times count from.
    pub(crate) fn take_evidence(
        &mut self,
    ) -> (
        tokio::time::Instant,
        Vec<(
            echomuse_turn::MonoInstant,
            echomuse_voice::evidence::Evidence,
        )>,
    ) {
        (
            self.voice.clock_origin(),
            std::mem::take(&mut self.evidence),
        )
    }

    pub(crate) fn deadline(&self) -> Option<tokio::time::Instant> {
        self.voice.next_deadline()
    }

    /// Hands the machine one input and carries out what it asks.
    pub(crate) fn handle(
        &mut self,
        input: Input,
        nodes: &EsphomeNodes,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        // A correction that decided without a record to follow leaves nothing to route.
        if let Input::Corrected { call, correction } = &input
            && !correction.record_follows
        {
            self.fix_records.remove(call);
        }
        self.voice.handle(input, now());
        self.execute(nodes, tasks);
    }

    /// An answer a task posted.
    pub(crate) fn message(
        &mut self,
        message: VoiceMessage,
        nodes: &EsphomeNodes,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        match message {
            VoiceMessage::Input { input, .. } => self.handle(input, nodes, tasks),
            VoiceMessage::Decoded {
                call,
                stream,
                event,
                ..
            } => match self.decodes.remove(&call) {
                Some(Decode::Pulling) => {
                    let report = match event {
                        Some(event) => {
                            self.decodes.insert(call, Decode::Idle(stream));
                            DecodeReport::Event(event)
                        }
                        None => DecodeReport::Closed,
                    };
                    self.handle(
                        Input::Decode {
                            call,
                            event: report,
                        },
                        nodes,
                        tasks,
                    );
                }
                // Cancelled while pulling (dropping the stream stops ffmpeg), or unknown.
                _ => stream.cancel(),
            },
        }
    }

    pub(crate) fn changed(
        &mut self,
        change: WatchChange,
        nodes: &EsphomeNodes,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        match change {
            WatchChange::Rings(rings) => self.handle(Input::Rings(rings), nodes, tasks),
            WatchChange::Settings(settings) => {
                self.compile_snapshot(settings.transcript.clone(), tasks);
                self.handle(Input::Settings(settings), nodes, tasks);
            }
            WatchChange::Speech => {
                let status = self.watch.speech.borrow_and_update().clone();
                self.models = models_of(&status);
                if self.models.is_some() && !self.capture_permitted {
                    // Capture is granted only by a new session.ready (§3.1.4).
                    tracing::info!(device = %self.device, "speech is ready; renegotiating the session");
                    self.link.close();
                }
                let ready = self.models.is_some() && self.capture_permitted;
                self.handle(Input::Speech(ready), nodes, tasks);
            }
            WatchChange::Timers(view) => self.handle(Input::Timers(view), nodes, tasks),
            WatchChange::Tables => {
                let transcript = self
                    .services
                    .settings
                    .effective(&self.device)
                    .transcript
                    .clone();
                self.compile_snapshot(transcript, tasks);
            }
        }
    }

    /// Compiles the speaker's correction preferences against the current tables, off
    /// this thread (on the table builder's). Until it finishes, turns use the snapshot
    /// they find, and correction reports a snapshot of other tables as not ready.
    fn compile_snapshot(
        &mut self,
        transcript: echomuse_settings::settings::TranscriptSettings,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        self.snapshot_compiles += 1;
        let compile = self.snapshot_compiles;
        let published = Arc::clone(&self.snapshot);
        let keep = move |snapshot| {
            published.send_if_modified(|current| {
                if current.0 < compile {
                    *current = (compile, snapshot);
                    true
                } else {
                    false
                }
            });
        };
        let tables = self.watch.tables.borrow_and_update().clone();
        match tables {
            None => keep(None),
            Some(tables) => {
                let correction = self.services.correction.clone();
                tasks.spawn(async move {
                    keep(correction.snapshot(tables, transcript).await);
                    crate::device::Internal::VoiceTask
                });
            }
        }
    }

    /// Decides a turn's transcript correction (§4.7) beside the runtime: the bounded
    /// proposal on a blocking thread, then one check with HA's sentence matcher. A check
    /// that cannot change the text (shadow mode, or nothing to propose) answers at once
    /// with the heard text and sends its record when it finishes.
    fn correct(
        &mut self,
        call: CallId,
        request: echomuse_voice::CorrectionRequest,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        use echomuse_correction::{FixContext, HaContext, Prepared, TablesId, TurnText};
        use echomuse_voice::Correction;
        let started = tokio::time::Instant::now();
        let ha = self.services.ha.clone();
        let snapshot = self.snapshot.borrow().1.clone();
        let tables = self.services.correction.tables();
        let settings = self.services.settings.clone();
        let device = self.device.clone();
        let messages = self.messages.clone();
        let session = self.session;
        // A check that cannot change the text records its result for the turn's row; it
        // goes to the records service, which holds the row for it.
        let (fix_sender, fix_record) = oneshot::channel();
        self.fix_records.insert(call, fix_record);
        tasks.spawn(async move {
            let echomuse_voice::CorrectionRequest {
                scope,
                heard,
                wake_phrase,
                contextual,
                settings: transcript,
                target,
            } = request;
            let answer = |correction: Correction| {
                let messages = messages.clone();
                async move {
                    let input = Input::Corrected { call, correction };
                    let _ = messages.send(VoiceMessage::Input { session, input }).await;
                }
            };
            let generation = ha.status().borrow().generation;
            let language = match generation {
                Some(_) => ha
                    .pipeline_language(&scope, &target.pipeline)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let context = HaContext {
                // Without a connection the language is unknown, and nothing is checked.
                connection: generation.unwrap_or(echomuse_core::services::HaConnectionGeneration(0)),
                ha_device: target.ha_device.clone(),
                pipeline: Some(target.pipeline.clone()),
                language,
            };
            let turn = TurnText {
                heard: heard.clone(),
                wake_phrase,
                contextual,
            };
            let prepared = tokio::task::spawn_blocking(move || {
                echomuse_correction::prepare(&transcript, &turn, snapshot.as_ref(), &context)
            })
            .await;
            let check = match prepared {
                Ok(Prepared::Check(check)) => check,
                Ok(Prepared::Done(outcome)) => {
                    answer(Correction {
                        send: outcome.send,
                        applied: outcome.applied,
                        record: stt_fix(outcome.record),
                        record_follows: false,
                    })
                    .await;
                    return crate::device::Internal::VoiceTask;
                }
                Err(error) => {
                    tracing::warn!(%device, %error, "transcript correction failed; the heard text is sent");
                    answer(Correction {
                        send: heard,
                        applied: false,
                        record: None,
                        record_follows: false,
                    })
                    .await;
                    return crate::device::Internal::VoiceTask;
                }
            };
            let prepared_in = check.context().clone();
            let current = {
                let ha = ha.clone();
                move || FixContext {
                    connection: ha
                        .status()
                        .borrow()
                        .generation
                        .unwrap_or(echomuse_core::services::HaConnectionGeneration(0)),
                    // Withdrawn tables match no result.
                    tables: tables.borrow().as_ref().map_or(
                        TablesId {
                            revision: u64::MAX,
                            digest: String::new(),
                        },
                        |t| t.id().clone(),
                    ),
                    config: echomuse_correction::ConfigDigest::of(
                        &settings.effective(&device).transcript,
                    ),
                    ..prepared_in
                }
            };
            if check.can_change_text() {
                let outcome = echomuse_correction::run(&ha, *check, scope, started, current).await;
                answer(Correction {
                    send: outcome.send,
                    applied: outcome.applied,
                    record: stt_fix(outcome.record),
                    record_follows: false,
                })
                .await;
            } else {
                answer(Correction {
                    send: heard,
                    applied: false,
                    record: None,
                    record_follows: true,
                })
                .await;
                // The record is for the turn's row, not for the turn: it is checked
                // under a scope of its own, which the turn's end does not cancel, and
                // bounded by the check's limit.
                let detached = WorkScope::new(
                    scope.identity().clone(),
                    tokio_util::sync::CancellationToken::new(),
                );
                let outcome =
                    echomuse_correction::run(&ha, *check, detached, started, current).await;
                let _ = fix_sender.send(stt_fix(outcome.record));
            }
            crate::device::Internal::VoiceTask
        });
    }

    /// Plays an alert sound's preview; `reply` gets the machine's answer.
    pub(crate) fn preview(
        &mut self,
        sound: echomuse_playback::AlertPreviewSound,
        reply: oneshot::Sender<PreviewOutcome>,
        nodes: &EsphomeNodes,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        let id = self.preview_id(reply);
        self.handle(Input::Preview { id, sound }, nodes, tasks);
    }

    /// Stops the sound preview; `reply` gets whether one was playing.
    pub(crate) fn stop_preview(
        &mut self,
        reply: oneshot::Sender<PreviewOutcome>,
        nodes: &EsphomeNodes,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        let id = self.preview_id(reply);
        self.handle(Input::StopPreview { id }, nodes, tasks);
    }

    /// Arms the recording modes' subscription with `sink` receiving its audio (every
    /// subscription's, across reopenings), or disarms it (`None`). A disarmed recorder
    /// still receives the end of the subscription that was open, so it can save what it
    /// has; a recorder that falls behind is dropped at once and its receiver sees the
    /// channel close in the middle of a subscription.
    pub(crate) fn set_diagnostic(
        &mut self,
        sink: Option<mpsc::Sender<echomuse_voice::DiagnosticAudio>>,
        nodes: &EsphomeNodes,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        let armed = sink.is_some();
        let previous = std::mem::replace(&mut self.diagnostic, sink);
        if self.voice.diagnostic_active() && self.diagnostic_closing.is_none() {
            self.diagnostic_closing = previous;
        }
        self.handle(Input::Diagnostic(armed), nodes, tasks);
    }

    fn preview_id(&mut self, reply: oneshot::Sender<PreviewOutcome>) -> PreviewId {
        self.next_preview += 1;
        let id = PreviewId(self.next_preview);
        self.previews.insert(id, reply);
        id
    }

    fn post(&self, input: Input) {
        if self
            .messages
            .try_send(VoiceMessage::Input {
                session: self.session,
                input,
            })
            .is_err()
        {
            tracing::warn!(device = %self.device, "the voice answer queue is full; closing the session");
            self.link.close();
        }
    }

    /// Carries out every effect the machine queued.
    pub(crate) fn execute(
        &mut self,
        nodes: &EsphomeNodes,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        let mut sink_overflowed = false;
        for effect in self.voice.drain_effects() {
            match effect {
                Effect::Diagnostic(audio) => {
                    let ended = matches!(audio, echomuse_voice::DiagnosticAudio::Ended(_));
                    if let Some(closing) = self.diagnostic_closing.take() {
                        // The subscription a disarm closed: its chunks and its end.
                        // A closing recorder that falls behind is dropped; it discards.
                        if closing.try_send(audio).is_ok() && !ended {
                            self.diagnostic_closing = Some(closing);
                        }
                    } else if let Some(sink) = &self.diagnostic
                        && sink.try_send(audio).is_err()
                    {
                        tracing::warn!(device = %self.device, "the recording fell behind; disarming it");
                        self.diagnostic = None;
                        sink_overflowed = true;
                    }
                }
                Effect::Send { body, generation } => {
                    let sent: Result<MessageId, SendError> =
                        each_body!(&body, b => self.link.send(b, generation));
                    if let Err(error) = sent {
                        tracing::debug!(device = %self.device, %error, "a message was not sent");
                    }
                }
                Effect::Request {
                    id,
                    body,
                    generation,
                } => {
                    let requested: Result<PendingAck, SendError> =
                        each_body!(&body, b => self.link.request(b, generation, ACK_TIMEOUT));
                    match requested {
                        Ok(pending) => {
                            let messages = self.messages.clone();
                            let session = self.session;
                            tasks.spawn(async move {
                                let outcome = match pending.acknowledged().await {
                                    Ok(ack) => AckOutcome::from_ack(&ack),
                                    Err(CommandFailure::Timeout { .. }) => {
                                        AckOutcome::Failed(AckFailure::TimedOut)
                                    }
                                    Err(CommandFailure::SessionEnded { .. }) => {
                                        AckOutcome::Failed(AckFailure::SessionEnded)
                                    }
                                };
                                let input = Input::Ack { id, outcome };
                                let _ = messages.send(VoiceMessage::Input { session, input }).await;
                                crate::device::Internal::VoiceTask
                            });
                        }
                        Err(_) => self.post(Input::Ack {
                            id,
                            outcome: AckOutcome::Failed(AckFailure::SessionEnded),
                        }),
                    }
                }
                Effect::RenderFrame(frame) => {
                    if let Err(mpsc::error::TrySendError::Full(_)) = self.render.try_send(frame) {
                        tracing::warn!(device = %self.device, "the render queue is full; closing the session");
                        self.link.close();
                    }
                }
                Effect::Violation(error) => self.link.protocol_violation(error),
                Effect::Mic { scope, key, job } => self.mic(scope, key, job),
                Effect::Reference { scope, key, job } => self.reference(scope, key, job),
                Effect::Verify { scope, job } => {
                    let candidate = job.candidate.clone();
                    let bytes = job.pcm.len() * 2;
                    self.submit(
                        3,
                        scope,
                        bytes,
                        {
                            let candidate = candidate.clone();
                            move |result| Input::Verify { candidate, result }
                        },
                        move |models, slot| slot.state(|| WakeVerification::new(models)).run(job),
                    );
                }
                Effect::Span { scope, turn, job } => {
                    let bytes = crate::speech_workers::span_job_bytes(&job);
                    self.submit(
                        4,
                        scope,
                        bytes,
                        move |result| Input::Span { turn, result },
                        move |models, slot| slot.state(|| SpanDecoding::new(models)).run(job),
                    );
                }
                Effect::Ha {
                    call,
                    scope,
                    request,
                } => self.ha(call, scope, request, tasks),
                Effect::Decode(command) => self.decode(command, tasks),
                Effect::Alert { call, request } => self.alert(call, request, tasks),
                Effect::Persist { call, request } => self.persist(call, request, tasks),
                Effect::Satellite(command) => self.satellite(command, nodes, tasks),
                Effect::PreviewAnswer { id, outcome } => {
                    if let Some(reply) = self.previews.remove(&id) {
                        let _ = reply.send(outcome);
                    }
                }
                Effect::Correct { call, request } => self.correct(call, request, tasks),
                Effect::Evidence { at, evidence } => self.evidence.push((at, *evidence)),
                Effect::ServerTranscribe {
                    call,
                    through,
                    request,
                } => self.server_transcribe(call, through, request, tasks),
            }
        }
        if sink_overflowed {
            self.handle(Input::Diagnostic(false), nodes, tasks);
        }
    }

    /// Runs a transcription on the pause server the turn's settings name, within the
    /// request's limit, off the device's runtime thread (it is network I/O); the machine
    /// applies the answer only to the call and audio it asked about.
    fn server_transcribe(
        &mut self,
        call: CallId,
        through: echomuse_turn::Sample,
        request: echomuse_voice::ServerRequest,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        let messages = self.messages.clone();
        let session = self.session;
        tasks.spawn(async move {
            let scope = request.scope.clone();
            let decode = async {
                let server = echomuse_wyoming::ServerAddress::new(&request.host, request.port)
                    .map_err(|error| echomuse_turn::ServerFailure::Failed(error.to_string()))?;
                echomuse_wyoming::transcribe(
                    &server,
                    echomuse_wyoming::TranscribeRequest {
                        model: Some(request.model.as_str()),
                        language: Some(request.language.as_str()),
                        pcm: &request.pcm,
                    },
                    request.limit,
                )
                .await
                .map(|transcript| transcript.text)
                .map_err(|error| match error {
                    echomuse_wyoming::WyomingError::TimedOut(_) => {
                        echomuse_turn::ServerFailure::Timeout
                    }
                    error => echomuse_turn::ServerFailure::Failed(format!("{server}: {error}")),
                })
            };
            let result = tokio::select! {
                () = scope.cancelled() => return crate::device::Internal::VoiceTask,
                result = decode => result,
            };
            let input = Input::ServerTranscript {
                call,
                through,
                result,
            };
            let _ = messages.send(VoiceMessage::Input { session, input }).await;
            crate::device::Internal::VoiceTask
        });
    }

    fn mic(
        &mut self,
        scope: WorkScope,
        key: Option<echomuse_voice::speech::StreamKey>,
        job: MicJob,
    ) {
        let bytes = match &job {
            MicJob::Mic { pcm, .. } | MicJob::Reference { pcm, .. } => pcm.as_bytes().len(),
            MicJob::DeviceCells { cells, .. } => cells.len() * 8,
            _ => 64,
        };
        // The worker's notice that a pause decode is starting goes straight to the machine,
        // ahead of the job's own answer, so the pause server is asked at the same moment.
        let messages = self.messages.clone();
        let session = self.session;
        let notice_key = key.clone();
        self.submit(
            1,
            scope,
            bytes,
            move |result| Input::Mic { key, result },
            move |models, slot| {
                let mut pause = |through| {
                    if let Some(key) = &notice_key {
                        let input = Input::PauseDecoding {
                            key: key.clone(),
                            through,
                        };
                        // A full queue loses only the early notice: the pause decode's
                        // own result asks the server instead.
                        let _ = messages.try_send(VoiceMessage::Input { session, input });
                    }
                };
                slot.state(|| MicAnalysis::new(models))
                    .run_noting_pauses(job, &mut pause)
            },
        );
    }

    fn reference(
        &mut self,
        scope: WorkScope,
        key: Option<echomuse_voice::speech::StreamKey>,
        job: ReferenceJob,
    ) {
        let bytes = match &job {
            ReferenceJob::Pcm { pcm, .. } => pcm.as_bytes().len(),
            _ => 64,
        };
        self.submit(
            2,
            scope,
            bytes,
            move |result| Input::Reference { key, result },
            move |models, slot| slot.state(|| ReferenceScoring::new(models)).run(job),
        );
    }

    /// Runs a job on worker `which` (1 microphone analysis, 2 reference scoring, 3 wake
    /// verification, 4 span decoding) and posts its answer behind the answers of the
    /// worker's earlier jobs. A cancelled job's answer is dropped: what asked for it is
    /// gone.
    fn submit<T, A, W>(&mut self, which: usize, scope: WorkScope, bytes: usize, answer: A, work: W)
    where
        T: Send + 'static,
        A: FnOnce(Result<T, RuntimeError>) -> Input + Send + 'static,
        W: FnOnce(Arc<dyn ModelSet>, &mut WorkerSlot) -> T + Send + 'static,
    {
        let answer: PendingAnswer = match self.models.clone() {
            None => Box::pin(std::future::ready(Some(answer(Err(RuntimeError::Closed))))),
            Some(models) => {
                match self.workers[which - 1]
                    .submit_stateful(scope, bytes, move |slot| work(models, slot))
                {
                    Ok(work) => Box::pin(async move {
                        match work.result().await {
                            Err(RuntimeError::Cancelled) => None,
                            result => Some(answer(result)),
                        }
                    }),
                    Err(RuntimeError::Cancelled) => return,
                    Err(error) => Box::pin(std::future::ready(Some(answer(Err(error))))),
                }
            }
        };
        // The forwarder ends only with the session, when nothing waits for answers.
        let _ = self.answers[which - 1].send(answer);
    }

    fn ha(
        &mut self,
        call: CallId,
        scope: WorkScope,
        request: HaRequest,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        let ha = self.services.ha.clone();
        let alerts = self.services.alerts.clone();
        let device = self.device.clone();
        let messages = self.messages.clone();
        let session = self.session;
        tasks.spawn(async move {
            let send = |reply: HaReply| {
                let messages = messages.clone();
                async move {
                    let input = Input::Ha { call, reply };
                    let _ = messages.send(VoiceMessage::Input { session, input }).await;
                }
            };
            match request {
                HaRequest::Pipeline { ha_device } => {
                    send(HaReply::Pipeline(ha.pipeline_for(scope, ha_device).await)).await;
                }
                HaRequest::Stt { target, audio } => {
                    let result = ha
                        .run_stt(SttRequest {
                            scope,
                            target,
                            audio,
                        })
                        .await;
                    send(HaReply::Stt(result)).await;
                }
                HaRequest::IntentTts {
                    target,
                    text,
                    conversation,
                } => {
                    // Holds this speaker's place among turns waiting on an intent, for
                    // the LLM relay's choice of speaker.
                    let _waiting = alerts.intent_wait(device).ok();
                    let request = IntentRequest {
                        scope,
                        target,
                        text,
                        conversation,
                    };
                    match ha.run_intent_tts(request).await {
                        Err(refused) => send(HaReply::IntentDispatched(Err(refused))).await,
                        Ok(mut run) => {
                            send(HaReply::IntentDispatched(Ok(()))).await;
                            let mut ended = false;
                            while let Some(event) = run.events.recv().await {
                                ended = event.is_terminal();
                                send(HaReply::IntentEvent(event)).await;
                                if ended {
                                    break;
                                }
                            }
                            if !ended {
                                // A run whose events stopped without an end may or may
                                // not have acted: an unknown outcome.
                                send(HaReply::IntentEvent(RunEvent::Lost)).await;
                            }
                        }
                    }
                }
                HaRequest::Tts { target, text } => {
                    let result = ha
                        .run_tts(TtsRequest {
                            scope,
                            target,
                            text,
                        })
                        .await;
                    send(HaReply::Tts(result)).await;
                }
                HaRequest::Recognize { text, ha_device } => {
                    let result = ha
                        .recognize(RecognizeRequest {
                            scope,
                            ha_device,
                            sentences: echomuse_voice::recognizer_sentences(&text),
                            language: None,
                        })
                        .await;
                    send(HaReply::Recognize(result)).await;
                }
                HaRequest::TimerStatus { ha_device } => {
                    send(HaReply::TimerStatus(
                        ha.timer_status(scope, ha_device).await,
                    ))
                    .await;
                }
                HaRequest::CallIntent { intent, ha_device } => {
                    let outcome = ha
                        .call_intent(DirectIntentRequest {
                            scope,
                            ha_device,
                            intent,
                        })
                        .await;
                    send(HaReply::CallIntent(outcome)).await;
                }
            }
            crate::device::Internal::VoiceTask
        });
    }

    fn decode(&mut self, command: DecodeCommand, tasks: &mut JoinSet<crate::device::Internal>) {
        match command {
            DecodeCommand::Start {
                call,
                url,
                skip_frames,
                equalizer,
            } => match resolve_media_url(&self.services.ha_base, &url) {
                Ok(url) => {
                    let stream = self.services.decoder.start(DecodeRequest {
                        url,
                        skip_frames,
                        equalizer: equalizer
                            .map(|settings| Equalizer::new(&settings, RENDER_SAMPLE_RATE)),
                    });
                    self.decodes.insert(call, Decode::Idle(stream));
                }
                Err(error) => {
                    tracing::warn!(device = %self.device, %error, "a media URL cannot be fetched");
                    self.decodes.insert(call, Decode::Cancelled);
                    self.post(Input::Decode {
                        call,
                        event: DecodeReport::Closed,
                    });
                }
            },
            DecodeCommand::Pull(call) => match self.decodes.remove(&call) {
                Some(Decode::Idle(mut stream)) => {
                    self.decodes.insert(call, Decode::Pulling);
                    let messages = self.messages.clone();
                    let session = self.session;
                    tasks.spawn(async move {
                        let event = stream.recv().await;
                        let message = VoiceMessage::Decoded {
                            session,
                            call,
                            stream,
                            event,
                        };
                        let _ = messages.send(message).await;
                        crate::device::Internal::VoiceTask
                    });
                }
                Some(other) => {
                    self.decodes.insert(call, other);
                }
                None => {}
            },
            DecodeCommand::Cancel(call) => match self.decodes.remove(&call) {
                Some(Decode::Idle(stream)) => stream.cancel(),
                Some(Decode::Pulling) => {
                    self.decodes.insert(call, Decode::Cancelled);
                }
                Some(Decode::Cancelled) | None => {}
            },
        }
    }

    fn alert(
        &mut self,
        call: CallId,
        request: AlertRequest,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        let alerts = self.services.alerts.clone();
        let device = self.device.clone();
        let messages = self.messages.clone();
        let session = self.session;
        tasks.spawn(async move {
            let reply = match request {
                AlertRequest::Act(act) => AlertReply::Act(alerts.act(act).await),
                AlertRequest::SetAlarm(set) => AlertReply::SetAlarm(alerts.set_alarm(set).await),
                AlertRequest::CancelAlarms(cancel) => {
                    AlertReply::CancelAlarms(alerts.cancel_alarms(cancel).await)
                }
                AlertRequest::ListAlarms => {
                    AlertReply::ListAlarms(alerts.list_alarms(device, ALARM_LIMIT).await)
                }
            };
            let input = Input::Alert { call, reply };
            let _ = messages.send(VoiceMessage::Input { session, input }).await;
            crate::device::Internal::VoiceTask
        });
    }

    /// Writes a record. Turns and continuations are handed to the records service here,
    /// on the runtime thread and in the order the machine asked, and it writes them on its
    /// own tasks: neither depends on the session or this runtime any more.
    fn persist(
        &mut self,
        call: CallId,
        request: PersistRequest,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        type Reply = std::pin::Pin<Box<dyn Future<Output = PersistReply> + Send>>;
        let records = self.services.records.clone();
        let device = self.device.clone();
        let messages = self.messages.clone();
        let session = self.session;
        fn submitted<T>(
            answer: Result<Result<T, echomuse_records::RecordsError>, oneshot::error::RecvError>,
        ) -> Result<T, echomuse_records::RecordsError> {
            answer.unwrap_or(Err(echomuse_records::RecordsError::TaskFailed))
        }
        let reply: Reply = match request {
            PersistRequest::Turn {
                record,
                audio,
                fix_follows,
            } => {
                let fix = fix_follows.and_then(|call| self.fix_records.remove(&call));
                let written = records.submit_turn(*record, audio, fix);
                Box::pin(async move {
                    PersistReply::Turn(submitted(written.await).map_err(|error| error.to_string()))
                })
            }
            PersistRequest::Continuation { turn_uuid, end } => {
                let written = records.submit_continuation(&device, turn_uuid, end);
                Box::pin(async move {
                    PersistReply::Continuation(
                        submitted(written.await)
                            .map(drop)
                            .map_err(|error| error.to_string()),
                    )
                })
            }
            PersistRequest::NearMiss { record, audio } => Box::pin(async move {
                PersistReply::NearMiss(
                    records
                        .persist_near_miss(*record, audio)
                        .await
                        .map_err(|error| error.to_string()),
                )
            }),
        };
        tasks.spawn(async move {
            let reply = reply.await;
            if let PersistReply::Turn(Err(error))
            | PersistReply::NearMiss(Err(error))
            | PersistReply::Continuation(Err(error)) = &reply
            {
                tracing::warn!(%device, error, "a voice record was not written");
            }
            let input = Input::Persisted { call, reply };
            let _ = messages.try_send(VoiceMessage::Input { session, input });
            crate::device::Internal::VoiceTask
        });
    }

    fn satellite(
        &mut self,
        command: SatelliteCommand,
        nodes: &EsphomeNodes,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        if let SatelliteCommand::StartRun { call, .. } = &command {
            let call = *call;
            let (started, result) = oneshot::channel();
            nodes.satellite(command, Some(started));
            let messages = self.messages.clone();
            let session = self.session;
            tasks.spawn(async move {
                let result = result
                    .await
                    .unwrap_or_else(|_| Err("the satellite stopped".to_owned()));
                let input = Input::RunStarted { call, result };
                let _ = messages.send(VoiceMessage::Input { session, input }).await;
                crate::device::Internal::VoiceTask
            });
            return;
        }
        nodes.satellite(command, None);
    }

    /// The session ended: the machine revokes what is live and queues its final
    /// records, which are still written.
    pub(crate) fn end(
        &mut self,
        nodes: &EsphomeNodes,
        tasks: &mut JoinSet<crate::device::Internal>,
    ) {
        self.handle(Input::SessionLost, nodes, tasks);
        for (_, decode) in self.decodes.drain() {
            if let Decode::Idle(stream) = decode {
                stream.cancel();
            }
        }
        self.session_scope.cancel();
    }
}

/// Writes render frames in order, waiting for room on the audio socket; a cancelled
/// playback's frames are dropped unwritten.
async fn write_frames(link: SessionLink, mut frames: mpsc::Receiver<RenderFrame>) {
    while let Some(frame) = frames.recv().await {
        if frame.playback.is_cancelled() {
            continue;
        }
        match link.send_render(frame.bytes, &frame.playback).await {
            Ok(()) | Err(echomuse_link::RenderError::PlaybackCancelled) => {}
            Err(_) => return,
        }
    }
}

/// A correction's record as `turns.stt_fix` stores it.
fn stt_fix(record: Option<echomuse_correction::FixRecord>) -> Option<echomuse_records::SttFix> {
    let record = record?;
    echomuse_records::JsonObject::parse(&record.to_json()).map(echomuse_records::SttFix)
}
