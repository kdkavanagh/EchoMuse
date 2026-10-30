//! A test rig around one [`Voice`]: a simulated device (real EMA1 frames and message
//! bodies), stand-in speech workers that run the real worker code over tone models, and
//! recorded service requests a test answers by hand. Time is advanced explicitly.

use std::{
    collections::VecDeque,
    sync::{Arc, LazyLock},
    time::Duration,
};

use bytes::Bytes;
use echomuse_alerts::{RingsSnapshot, schedule::HaTimeZone};
use echomuse_core::types::{DeviceId, RuntimeId, SessionId, WorkIdentity, WorkScope};
use echomuse_grammar::Grammar;
use echomuse_playback::AckOutcome;
use echomuse_protocol::{
    Capabilities, DeviceEnvelope, DeviceMessage, EnvelopeHeader, LinkSessionId, MessageId,
    MessageType, ProtocolError, Sha256, ema::Packet, streams::StreamOpen,
};
use echomuse_settings::DeviceSettings;
use echomuse_speech::{BcresnetModelFiles, PinnedFile, Sha256Digest};
use echomuse_turn::Sample;
use echomuse_uplink::{StreamStart, UplinkCommand, starts::CandidateStarts};
use serde_json::{Map, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{
    AlertRequest, CallId, DecodeCommand, Effect, HaReply, HaRequest, Input, Now, Outbound,
    PersistRequest, SatelliteCommand, Voice, VoiceConfig, WakeModelInfo,
    arbiter::WakeArbiter,
    sim::SimDevice,
    speech::{
        mic::MicAnalysis, reference::ReferenceScoring, span::SpanDecoding, testing::ToneModels,
        verify::WakeVerification,
    },
};

pub(crate) const WORDS: &[&str] = &[
    "ophelia", "turn", "on", "the", "kitchen", "light", "stop", "what", "time", "is", "it",
];

static GRAMMAR: LazyLock<Grammar> = LazyLock::new(|| Grammar::new().expect("the grammar compiles"));

pub(crate) fn capabilities(extra: &[&str]) -> Capabilities {
    let mut names: Vec<&str> = echomuse_protocol::REQUIRED_CAPABILITIES
        .iter()
        .map(|c| c.as_str())
        .collect();
    names.extend(["leds", "led_anim", "buttons", "button_hold"]);
    names.extend(extra);
    Capabilities::from_names(names)
}

pub(crate) struct Options {
    pub(crate) device: &'static str,
    pub(crate) capabilities: Capabilities,
    pub(crate) settings: DeviceSettings,
    pub(crate) arbiter: Arc<WakeArbiter>,
    pub(crate) speech_ready: bool,
    pub(crate) rings: Arc<RingsSnapshot>,
    /// `session.ready` asked for the `afe` stream (the device announces
    /// `afe_metadata_v1`).
    pub(crate) afe_metadata: bool,
    pub(crate) kiosk_token: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            device: "G090LF0000000001",
            capabilities: capabilities(&[]),
            settings: DeviceSettings::default(),
            arbiter: Arc::new(WakeArbiter::new()),
            speech_ready: true,
            rings: Arc::new(RingsSnapshot::default()),
            afe_metadata: false,
            kiosk_token: None,
        }
    }
}

pub(crate) struct Rig {
    pub(crate) voice: Voice,
    pub(crate) now: Now,
    pub(crate) device: SimDevice,
    pub(crate) models: ToneModels,
    device_id: DeviceId,
    mic: MicAnalysis,
    reference: ReferenceScoring,
    verify: WakeVerification,
    span: SpanDecoding,
    pending: VecDeque<Input>,
    pub(crate) sent: Vec<(Outbound, u32)>,
    pub(crate) requests: Vec<(MessageId, Outbound, u32)>,
    pub(crate) frames: usize,
    pub(crate) ha: Vec<(CallId, HaRequest)>,
    pub(crate) decodes: Vec<DecodeCommand>,
    pub(crate) alerts: Vec<(CallId, AlertRequest)>,
    pub(crate) persisted: Vec<PersistRequest>,
    pub(crate) satellite: Vec<SatelliteCommand>,
    pub(crate) previews: Vec<(crate::PreviewId, crate::PreviewOutcome)>,
    pub(crate) diagnostics: Vec<crate::DiagnosticAudio>,
    /// Decision evidence queued for the session recorder, with its machine time.
    pub(crate) evidence: Vec<(echomuse_turn::MonoInstant, crate::evidence::Evidence)>,
    /// Transcript correction requests a test answers by hand.
    pub(crate) corrections: Vec<(CallId, crate::CorrectionRequest)>,
    pub(crate) violations: Vec<ProtocolError>,
    /// Requests the device has not acknowledged yet (when not acknowledging at once).
    pub(crate) unacked: Vec<MessageId>,
    pub(crate) auto_ack: bool,
    /// Answer each transcript correction at once with the heard text and no record (as
    /// correction `off` would); a test that drives correction itself turns this off.
    pub(crate) auto_correct: bool,
    /// The stand-in pause server: what it answers at once, in order (while any are left;
    /// after that a request waits for the test), and every transcription asked of it.
    pub(crate) server_answers: VecDeque<Result<String, echomuse_turn::ServerFailure>>,
    pub(crate) server_requests: Vec<(CallId, Sample, crate::ServerRequest)>,
    /// Commit re-decodes sent to the span worker.
    pub(crate) redecodes: usize,
    /// Every effect in order, as text, for diagnosing a failing test.
    pub(crate) log: Vec<String>,
    /// Microphone worker results are held back (as a busy worker's queue would) until
    /// [`Rig::release_mic_results`].
    pub(crate) hold_mic_results: bool,
    held: Vec<Input>,
}

impl Rig {
    pub(crate) fn new(options: Options) -> Self {
        let models = ToneModels::new(WORDS);
        let shared: Arc<dyn crate::speech::models::ModelSet> = Arc::new(models.clone());
        let device_id = DeviceId(options.device.to_owned());
        let session_token = CancellationToken::new();
        let identity = WorkIdentity {
            device: device_id.clone(),
            runtime: RuntimeId(1),
            session: SessionId(1),
            turn: None,
        };
        let session_scope = WorkScope::new(identity.clone(), session_token.child_token());
        let capture = WorkScope::new(identity, session_token.child_token());
        let now = Now {
            at: Instant::now(),
            wall_ms: 1_800_000_000_000,
        };
        let digest = Sha256Digest::of_bytes(b"graph");
        let afe_metadata = options.afe_metadata;
        let voice = Voice::new(
            VoiceConfig {
                device: device_id.clone(),
                session: SessionId(1),
                capabilities: options.capabilities,
                afe_metadata: options.afe_metadata,
                muted: false,
                volume: echomuse_esphome::entities::DeviceVolume::new(64),
                wake: WakeModelInfo {
                    graph_sha256: Sha256::parse(
                        "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f",
                    )
                    .expect("a digest"),
                    name: echomuse_records::WakeModelName::new("ophelia"),
                    phrase: "ophelia".to_owned(),
                    verify_core: "ophel".to_owned(),
                    files: Some(BcresnetModelFiles {
                        graph: PinnedFile::new("/nonexistent/graph.onnx", digest),
                        sidecar: PinnedFile::new("/nonexistent/graph.json", digest),
                    }),
                    thresholds: Some((0.9, 0.95)),
                },
                grammar: GRAMMAR.clone(),
                arbiter: options.arbiter,
                settings: Arc::new(options.settings),
                session_scope,
                capture: Some(capture),
                speech_ready: options.speech_ready,
                rings: options.rings,
                time_zone: HaTimeZone::named("Europe/London").ok(),
                ha_device: Some(echomuse_core::services::HaDeviceId("ha-dev-1".to_owned())),
                kiosk_token: options.kiosk_token,
            },
            now,
        );
        let mut rig = Self {
            voice,
            now,
            device: if afe_metadata {
                SimDevice::new(7, 9).with_afe()
            } else {
                SimDevice::new(7, 9)
            },
            models,
            device_id,
            mic: MicAnalysis::new(Arc::clone(&shared)),
            reference: ReferenceScoring::new(Arc::clone(&shared)),
            verify: WakeVerification::new(Arc::clone(&shared)),
            span: SpanDecoding::new(shared),
            pending: VecDeque::new(),
            sent: Vec::new(),
            requests: Vec::new(),
            frames: 0,
            ha: Vec::new(),
            decodes: Vec::new(),
            alerts: Vec::new(),
            persisted: Vec::new(),
            satellite: Vec::new(),
            previews: Vec::new(),
            diagnostics: Vec::new(),
            evidence: Vec::new(),
            corrections: Vec::new(),
            violations: Vec::new(),
            unacked: Vec::new(),
            hold_mic_results: false,
            held: Vec::new(),
            auto_ack: true,
            auto_correct: true,
            server_answers: VecDeque::new(),
            server_requests: Vec::new(),
            redecodes: 0,
            log: Vec::new(),
        };
        rig.process();
        for body in rig.device.stream_opens() {
            let open = StreamOpen::parse(&body).expect("a stream.open body");
            rig.handle(Input::Control(
                rig.envelope(DeviceMessage::StreamOpen(open), 0),
            ));
        }
        rig
    }

    pub(crate) fn envelope(&self, message: DeviceMessage, generation: u32) -> DeviceEnvelope {
        DeviceEnvelope {
            header: EnvelopeHeader {
                session_id: Some(LinkSessionId::from("session-1".to_owned())),
                message_id: MessageId::generate(),
                device_id: self.device_id.clone(),
                generation,
            },
            message,
        }
    }

    pub(crate) fn control(
        &mut self,
        message_type: MessageType,
        body: Map<String, Value>,
        generation: u32,
    ) {
        let envelope = self.envelope(DeviceMessage::Other { message_type, body }, generation);
        self.handle(Input::Control(envelope));
    }

    pub(crate) fn handle(&mut self, input: Input) {
        self.voice.handle(input, self.now);
        self.process();
    }

    /// Carries out every effect, feeding the answers back, until nothing is left.
    pub(crate) fn process(&mut self) {
        loop {
            let effects = self.voice.drain_effects();
            if effects.is_empty() && self.pending.is_empty() {
                break;
            }
            for effect in effects {
                self.effect(effect);
            }
            while let Some(input) = self.pending.pop_front() {
                self.voice.handle(input, self.now);
                for effect in self.voice.drain_effects() {
                    self.effect(effect);
                }
            }
        }
    }

    fn effect(&mut self, effect: Effect) {
        self.log
            .push(format!("{effect:?}").chars().take(300).collect());
        match effect {
            Effect::Send { body, generation } => self.sent.push((body, generation)),
            Effect::Request {
                id,
                body,
                generation,
            } => {
                if let Outbound::Uplink(UplinkCommand::Open(open)) = &body {
                    self.backfill_open(open);
                }
                if self.auto_ack {
                    self.pending.push_back(Input::Ack {
                        id: id.clone(),
                        outcome: AckOutcome::Accepted,
                    });
                } else {
                    self.unacked.push(id.clone());
                }
                self.requests.push((id, body, generation));
            }
            Effect::RenderFrame(_) => self.frames += 1,
            Effect::Violation(error) => self.violations.push(error),
            Effect::Mic { scope, key, job } => {
                if !scope.is_cancelled() {
                    // As the runtime does, the worker's notice that it is starting a pause
                    // decode arrives before the job's output.
                    let mut pauses = Vec::new();
                    let output = self
                        .mic
                        .run_noting_pauses(job, &mut |through| pauses.push(through));
                    if let Some(key) = &key {
                        for through in pauses {
                            self.pending.push_back(Input::PauseDecoding {
                                key: key.clone(),
                                through,
                            });
                        }
                    }
                    let result = Input::Mic {
                        key,
                        result: Ok(output),
                    };
                    if self.hold_mic_results {
                        self.held.push(result);
                    } else {
                        self.pending.push_back(result);
                    }
                }
            }
            Effect::Reference { scope, key, job } => {
                if !scope.is_cancelled() {
                    let output = self.reference.run(job);
                    self.pending.push_back(Input::Reference {
                        key,
                        result: Ok(output),
                    });
                }
            }
            Effect::Verify { scope, job } => {
                if !scope.is_cancelled() {
                    let candidate = job.candidate.clone();
                    let output = self.verify.run(job);
                    self.pending.push_back(Input::Verify {
                        candidate,
                        result: Ok(output),
                    });
                }
            }
            Effect::Span { scope, turn, job } => {
                if matches!(job, crate::speech::SpanJob::Redecode { .. }) {
                    self.redecodes += 1;
                }
                if !scope.is_cancelled() {
                    let output = self.span.run(job);
                    self.pending.push_back(Input::Span {
                        turn,
                        result: Ok(output),
                    });
                }
            }
            Effect::Ha {
                call,
                scope,
                request,
            } => {
                // A cancelled scope is refused by the client before anything is sent.
                if !scope.is_cancelled() {
                    self.ha.push((call, request));
                }
            }
            Effect::Decode(command) => self.decodes.push(command),
            Effect::Alert { call, request } => self.alerts.push((call, request)),
            Effect::Persist { call, request } => {
                self.persisted.push(request);
                self.pending.push_back(Input::Persisted {
                    call,
                    reply: match self.persisted.last() {
                        // Each row gets the next id, as the database gives it.
                        Some(PersistRequest::Turn { .. }) => {
                            crate::PersistReply::Turn(Ok(echomuse_records::PersistedTurn {
                                insert: echomuse_records::TurnInsert {
                                    id: echomuse_records::TurnRowId(self.persisted.len() as i64),
                                    inserted: true,
                                },
                                utterance: None,
                                wake_clip: None,
                                turn_recording: None,
                            }))
                        }
                        _ => crate::PersistReply::Continuation(Ok(())),
                    },
                });
            }
            Effect::Satellite(command) => self.satellite.push(command),
            Effect::PreviewAnswer { id, outcome } => self.previews.push((id, outcome)),
            Effect::Diagnostic(audio) => self.diagnostics.push(audio),
            Effect::Evidence { at, evidence } => self.evidence.push((at, *evidence)),
            Effect::Correct { call, request } => {
                if self.auto_correct {
                    self.pending.push_back(Input::Corrected {
                        call,
                        correction: crate::Correction {
                            send: request.heard.clone(),
                            applied: false,
                            record: None,
                            record_follows: false,
                        },
                    });
                }
                self.corrections.push((call, request));
            }
            Effect::ServerTranscribe {
                call,
                through,
                request,
            } => {
                self.server_requests.push((call, through, request));
                if let Some(result) = self.server_answers.pop_front() {
                    self.pending.push_back(Input::ServerTranscript {
                        call,
                        through,
                        result,
                    });
                }
            }
        }
    }

    /// The device answers a controller-opened subscription with its backfill runs.
    fn backfill_open(&mut self, open: &echomuse_uplink::UplinkOpen) {
        for (stream, start) in &open.streams {
            if let StreamStart::At(sample) = start {
                let packets = self.device.backfill(stream.as_str(), sample.0);
                self.queue_packets(packets);
            }
        }
    }

    fn queue_packets(&mut self, packets: Vec<Bytes>) {
        for bytes in packets {
            let packet = Packet::parse(bytes).expect("a valid frame");
            self.pending.push_back(Input::Audio(packet));
        }
    }

    /// Hands the held microphone worker results to the machine, in the order they were
    /// produced, and stops holding them.
    pub(crate) fn release_mic_results(&mut self) {
        self.hold_mic_results = false;
        self.pending.extend(self.held.drain(..));
        self.process();
    }

    /// Advances the controller's clock, firing the machine's timers on the way.
    pub(crate) fn advance(&mut self, by: Duration) {
        let end = self.now.at + by;
        loop {
            match self.voice.next_deadline() {
                Some(due) if due <= end => {
                    if due > self.now.at {
                        self.now.wall_ms += (due - self.now.at).as_millis() as i64;
                        self.now.at = due;
                    }
                    self.handle(Input::Timer);
                }
                _ => break,
            }
        }
        self.now.wall_ms += (end - self.now.at).as_millis() as i64;
        self.now.at = end;
    }

    /// The room says `pcm`: captured and uploaded live in 80 ms blocks, with the
    /// controller's clock running along.
    pub(crate) fn hear(&mut self, pcm: &[i16]) {
        for block in pcm.chunks(crate::sim::BLOCK as usize) {
            self.device.hear(block);
            let packets = self.device.live();
            self.queue_packets(packets);
            self.process();
            self.advance(Duration::from_millis(80));
        }
    }

    /// The device plays `pcm` and hears only its echo.
    pub(crate) fn play(&mut self, pcm: &[i16]) {
        for block in pcm.chunks(crate::sim::BLOCK as usize) {
            self.device.play(block);
            let packets = self.device.live();
            self.queue_packets(packets);
            self.process();
            self.advance(Duration::from_millis(80));
        }
    }

    pub(crate) fn silence(&mut self, ms: u64) {
        self.hear(&crate::speech::testing::silence(ms));
    }

    pub(crate) fn words(&self, words: &[&str]) -> Vec<i16> {
        crate::speech::testing::utterance(&self.models, words, 400, 250, 6_000)
    }

    /// The device opens a wake candidate supported from `support_start`, open at `open`,
    /// and (once acknowledged) uploads the candidate subscription's backfill.
    pub(crate) fn wake(&mut self, id: &str, support_start: u64, open: u64, producing_sound: bool) {
        let body = self.device.candidate(
            id,
            &format!("lease-{id}"),
            support_start,
            open,
            producing_sound,
        );
        self.control(MessageType::WakeCandidate, body, 1);
        let accepted = self.sent.iter().rev().any(|(body, _)| {
            matches!(body, Outbound::CandidateAck(ack)
                if ack.status == echomuse_protocol::vocab::AckStatus::Accepted)
        });
        if accepted {
            let afe = self.device.serves_afe();
            let starts = CandidateStarts::new(Sample(support_start), afe);
            let mut packets = self.device.backfill("mic", starts.mic.0);
            packets.extend(self.device.backfill("reference", starts.reference.0));
            packets.extend(self.device.backfill("cells", starts.cells.0));
            if let Some(afe) = starts.afe {
                packets.extend(self.device.backfill("afe", afe.0));
            }
            self.queue_packets(packets);
            self.process();
        }
    }

    /// Takes the first HA request matching `matches`.
    pub(crate) fn take_ha(&mut self, what: &str, matches: impl Fn(&HaRequest) -> bool) -> CallId {
        let index = self
            .ha
            .iter()
            .position(|(_, request)| matches(request))
            .unwrap_or_else(|| {
                panic!(
                    "no {what} request; HA saw {:?}\nlog:\n{}",
                    self.ha
                        .iter()
                        .map(|(_, r)| format!("{r:?}").chars().take(80).collect::<String>())
                        .collect::<Vec<_>>(),
                    self.log.join("\n")
                )
            });
        self.ha.remove(index).0
    }

    pub(crate) fn reply(&mut self, call: CallId, reply: HaReply) {
        self.handle(Input::Ha { call, reply });
    }

    /// Plays a network playback to its end: the decode delivers `seconds` of audio and
    /// the device drains it.
    pub(crate) fn play_out(&mut self, seconds: f64) {
        let Some(DecodeCommand::Start { call, .. }) = self
            .decodes
            .iter()
            .rev()
            .find(|c| matches!(c, DecodeCommand::Start { .. }))
            .cloned()
        else {
            panic!("no decode started; log:\n{}", self.log.join("\n"));
        };
        self.handle(Input::Decode {
            call,
            event: crate::DecodeReport::Event(echomuse_playback::DecodeEvent::Opened {
                seekable: echomuse_playback::Seekability::Seekable,
            }),
        });
        let frames = (seconds * 48_000.0) as usize;
        let mut sent = 0;
        while sent < frames {
            let chunk = 3_840.min(frames - sent);
            self.handle(Input::Decode {
                call,
                event: crate::DecodeReport::Event(echomuse_playback::DecodeEvent::Audio(vec![
                    800;
                    chunk
                ])),
            });
            sent += chunk;
        }
        self.handle(Input::Decode {
            call,
            event: crate::DecodeReport::Event(echomuse_playback::DecodeEvent::Finished(Ok(
                echomuse_playback::DecodeSummary {
                    decoded_frames: frames as u64,
                    delivered_frames: frames as u64,
                },
            ))),
        });
        self.finish_render(frames as u64);
    }

    /// The device reports the latest network playback started and drained.
    pub(crate) fn finish_render(&mut self, frames: u64) {
        let (playback_id, generation) = self.last_render_start();
        let progress = serde_json::json!({
            "playback_id": playback_id, "event": "start", "submitted_frames": frames.to_string(),
            "completed_frames": "0", "mono_ns": "1", "uncertainty_us": 0,
            "timing_quality": "estimated", "reference_coverage": "full",
        });
        self.control(
            MessageType::RenderProgress,
            progress.as_object().expect("an object").clone(),
            generation,
        );
        let finished = serde_json::json!({
            "playback_id": playback_id, "last_completed_frame": frames.to_string(),
            "reason": "drained", "timing_quality": "estimated",
        });
        self.control(
            MessageType::RenderFinished,
            finished.as_object().expect("an object").clone(),
            generation,
        );
    }

    pub(crate) fn last_render_start(&self) -> (String, u32) {
        self.requests
            .iter()
            .rev()
            .find_map(|(_, body, generation)| match body {
                Outbound::Playback(echomuse_playback::Command::RenderStart(start))
                    if start.epoch.is_some() =>
                {
                    Some((start.playback_id.as_str().to_owned(), *generation))
                }
                _ => None,
            })
            .expect("a render.start")
    }

    pub(crate) fn turn_rows(&self) -> Vec<&echomuse_records::TurnRecord> {
        self.persisted
            .iter()
            .filter_map(|request| match request {
                PersistRequest::Turn { record, .. } => Some(record.as_ref()),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn uplink_commands(&self) -> Vec<&UplinkCommand> {
        self.requests
            .iter()
            .filter_map(|(_, body, _)| match body {
                Outbound::Uplink(command) => Some(command),
                _ => None,
            })
            .collect()
    }
}
