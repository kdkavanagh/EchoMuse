//! Assist pipeline runs (`assist_pipeline/run`): STT only, intent to TTS, and TTS only.
//!
//! The event shapes are HA's (`assist_pipeline/pipeline.py`, 2026.8.1): `run-start` carries
//! `runner_data.stt_binary_handler_id` and, when a reply will be spoken, `tts_output`;
//! `stt-end` carries `stt_output.text`; `intent-progress` carries either a `chat_log_delta`
//! or `tts_start_streaming`; `intent-end` carries `intent_output` (a conversation result)
//! and `processed_locally`; `tts-end` carries `tts_output.url`; `error` carries `code` and
//! `message`. HA confirms a run with a successful `result` once it has validated it, and
//! refuses it with an error `result` before anything runs. When the run's own timeout
//! expires HA sends an `error` event with code `timeout`.

use std::{sync::Arc, time::Duration};

use echomuse_core::{
    services::{
        ConversationId, HaError, INTENT_RUN_TIMEOUT, IntentEnd, IntentRequest, IntentRun, MediaUrl,
        NotDispatched, RUN_EVENT_CAPACITY, RunEvent, STT_RUN_TIMEOUT, STT_SAMPLE_RATE_HZ,
        SatelliteTarget, SttRequest, TTS_RUN_TIMEOUT, Transcript, TtsRequest,
    },
    types::WorkScope,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::{
    client::{HaClient, object},
    connection::{Link, Refusal, Route, RouteGuard, RunItem},
    endpoint::HaEndpoint,
};

/// Audio goes to HA in frames of half a second of PCM16 (16,000 bytes), well under the
/// 4 MiB HA's WebSocket server accepts per message, and small enough that other requests
/// are not held behind a long write.
const STT_FRAME_SAMPLES: usize = STT_SAMPLE_RATE_HZ as usize / 2;

// ── Wire shapes ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
enum EventKind {
    #[serde(rename = "run-start")]
    RunStart,
    #[serde(rename = "stt-end")]
    SttEnd,
    #[serde(rename = "intent-progress")]
    IntentProgress,
    #[serde(rename = "intent-end")]
    IntentEnd,
    #[serde(rename = "tts-end")]
    TtsEnd,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "run-end")]
    RunEnd,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct PipelineEvent {
    #[serde(rename = "type")]
    kind: EventKind,
    #[serde(default)]
    data: Value,
}

#[derive(Deserialize)]
struct RunStartData {
    runner_data: Option<RunnerData>,
    tts_output: Option<RunStartTts>,
}

#[derive(Deserialize)]
struct RunnerData {
    stt_binary_handler_id: Option<i64>,
}

#[derive(Deserialize)]
struct RunStartTts {
    url: Option<String>,
    stream_response: Option<bool>,
}

#[derive(Deserialize)]
struct SttEndData {
    stt_output: SttOutput,
}

#[derive(Deserialize)]
struct SttOutput {
    text: String,
}

#[derive(Deserialize)]
struct IntentProgressData {
    chat_log_delta: Option<ChatLogDelta>,
    tts_start_streaming: Option<bool>,
}

#[derive(Deserialize)]
struct ChatLogDelta {
    /// A string while the agent writes; absent or null in role and tool deltas.
    content: Option<Value>,
}

#[derive(Deserialize)]
struct IntentEndData {
    processed_locally: Option<bool>,
    intent_output: ConversationResult,
}

/// HA's `ConversationResult.as_dict`.
#[derive(Deserialize)]
struct ConversationResult {
    response: ConversationResponse,
    conversation_id: Option<String>,
    continue_conversation: bool,
}

/// The parts of HA's `IntentResponse.as_dict` read here.
#[derive(Deserialize)]
struct ConversationResponse {
    speech: Option<Speech>,
    response_type: Option<String>,
}

#[derive(Deserialize)]
struct Speech {
    plain: Option<SpeechText>,
    ssml: Option<SpeechText>,
}

#[derive(Deserialize)]
struct SpeechText {
    speech: Option<String>,
}

#[derive(Deserialize)]
struct TtsEndData {
    tts_output: TtsOutput,
}

#[derive(Deserialize)]
struct TtsOutput {
    url: String,
}

#[derive(Deserialize)]
struct ErrorData {
    code: String,
    message: String,
}

fn parse<T: DeserializeOwned>(data: Value) -> Result<T, String> {
    serde_json::from_value(data).map_err(|err| err.to_string())
}

fn parse_event(event: Value) -> Result<PipelineEvent, String> {
    parse(event)
}

fn speech_of(speech: Option<Speech>) -> String {
    let Some(speech) = speech else {
        return String::new();
    };
    [speech.plain, speech.ssml]
        .into_iter()
        .flatten()
        .filter_map(|text| text.speech)
        .find(|text| !text.is_empty())
        .unwrap_or_default()
}

fn intent_end(data: IntentEndData) -> IntentEnd {
    IntentEnd {
        speech: speech_of(data.intent_output.response.speech),
        conversation: data.intent_output.conversation_id.map(ConversationId),
        continue_conversation: data.intent_output.continue_conversation,
        response_type: data.intent_output.response.response_type,
        processed_locally: data.processed_locally,
    }
}

fn run_body(
    start_stage: &str,
    end_stage: &str,
    input: Value,
    target: &SatelliteTarget,
    timeout: Duration,
) -> Map<String, Value> {
    object(json!({
        "type": "assist_pipeline/run",
        "start_stage": start_stage,
        "end_stage": end_stage,
        "input": input,
        "pipeline": target.pipeline.0,
        "device_id": target.ha_device.as_ref().map(|device| device.0.as_str()),
        "timeout": timeout.as_secs_f64(),
    }))
}

// ── Intent to TTS ────────────────────────────────────────────────────────────

/// Turns one intent run's messages into [`RunEvent`]s as they arrive. It runs inside the
/// connection's reader, so it never waits: an event that does not fit the channel fences
/// the run.
pub(crate) struct IntentRoute {
    scope: WorkScope,
    events: mpsc::Sender<RunEvent>,
    endpoint: Arc<HaEndpoint>,
    /// The run-start URL of a reply HA can stream, released by `tts_start_streaming`.
    stream_url: Option<MediaUrl>,
    tts_sent: bool,
}

impl IntentRoute {
    /// Handles the run's `result`; true when the route is finished.
    pub fn on_result(&mut self, outcome: Result<Value, Refusal>) -> bool {
        if self.scope.is_cancelled() {
            return true;
        }
        match outcome {
            Ok(_) => false,
            Err(refusal) => {
                let _ = self.events.try_send(RunEvent::Rejected {
                    code: refusal.code,
                    message: refusal.message,
                });
                true
            }
        }
    }

    /// Handles one run event; true when the route is finished. A cancelled scope, an
    /// unreadable event or a full or closed channel fences the run without a terminal
    /// event, which the receiver reads as an unknown outcome.
    pub fn on_event(&mut self, event: Value) -> bool {
        if self.scope.is_cancelled() {
            return true;
        }
        let event = match parse_event(event).and_then(|event| self.translate(event)) {
            Ok(Some(event)) => event,
            Ok(None) => return false,
            Err(_) => return true,
        };
        let terminal = event.is_terminal();
        self.events.try_send(event).is_err() || terminal
    }

    /// The connection ended before the run did.
    pub fn lose(self) {
        if !self.scope.is_cancelled() {
            let _ = self.events.try_send(RunEvent::Lost);
        }
    }

    /// Ok(None) for events that carry nothing for the caller; Err for unreadable ones.
    fn translate(&mut self, event: PipelineEvent) -> Result<Option<RunEvent>, String> {
        Ok(match event.kind {
            EventKind::RunStart => {
                let data: RunStartData = parse(event.data)?;
                if let Some(RunStartTts {
                    url: Some(url),
                    stream_response: Some(true),
                }) = data.tts_output
                {
                    self.stream_url = Some(self.absolute(&url)?);
                }
                None
            }
            EventKind::IntentProgress => {
                let data: IntentProgressData = parse(event.data)?;
                if data.tts_start_streaming == Some(true) {
                    match (&self.stream_url, self.tts_sent) {
                        (Some(url), false) => {
                            self.tts_sent = true;
                            Some(RunEvent::TtsReady {
                                url: url.clone(),
                                streamed: true,
                            })
                        }
                        _ => None,
                    }
                } else {
                    match data.chat_log_delta.and_then(|delta| delta.content) {
                        Some(Value::String(text)) if !text.is_empty() => {
                            Some(RunEvent::AnswerDelta(text))
                        }
                        _ => None,
                    }
                }
            }
            EventKind::IntentEnd => Some(RunEvent::IntentEnded(intent_end(parse(event.data)?))),
            EventKind::TtsEnd => {
                let data: TtsEndData = parse(event.data)?;
                if self.tts_sent {
                    None
                } else {
                    self.tts_sent = true;
                    Some(RunEvent::TtsReady {
                        url: self.absolute(&data.tts_output.url)?,
                        streamed: false,
                    })
                }
            }
            EventKind::Error => {
                let data: ErrorData = parse(event.data)?;
                Some(RunEvent::Failed {
                    code: data.code,
                    message: data.message,
                })
            }
            EventKind::RunEnd => Some(RunEvent::Ended),
            EventKind::SttEnd | EventKind::Other => None,
        })
    }

    fn absolute(&self, url: &str) -> Result<MediaUrl, String> {
        self.endpoint
            .absolute(url)
            .ok_or_else(|| format!("unusable media URL {url:?}"))
    }
}

impl HaClient {
    pub(crate) async fn dispatch_intent_run(
        &self,
        request: IntentRequest,
    ) -> Result<IntentRun, NotDispatched> {
        let link = self.current_link().ok_or(NotDispatched::Unavailable)?;
        let mut body = run_body(
            "intent",
            "tts",
            json!({"text": request.text}),
            &request.target,
            INTENT_RUN_TIMEOUT,
        );
        body.insert(
            "conversation_id".to_owned(),
            request
                .conversation
                .map_or(Value::Null, |conversation| Value::String(conversation.0)),
        );
        let (events, receiver) = mpsc::channel(RUN_EVENT_CAPACITY);
        let route = IntentRoute {
            scope: request.scope.clone(),
            events,
            endpoint: self.endpoint().clone(),
            stream_url: None,
            tts_sent: false,
        };
        link.dispatch(Some(&request.scope), body, Route::Intent(Box::new(route)))
            .await?;
        Ok(IntentRun {
            generation: link.generation,
            events: receiver,
        })
    }
}

// ── STT and TTS ──────────────────────────────────────────────────────────────

/// How one STT attempt failed.
enum SttFailure {
    /// The transport failed; one retry on a new connection is allowed.
    Transport { dispatched: bool },
    /// Anything else, which is not retried.
    Final(HaError),
}

impl HaClient {
    pub(crate) async fn transcribe(&self, request: SttRequest) -> Result<Transcript, HaError> {
        let (failure, failed_generation) = match self.current_link() {
            Some(link) => match self.stt_attempt(&link, &request).await {
                Ok(transcript) => return Ok(transcript),
                Err(failure) => (failure, Some(link.generation)),
            },
            None => (SttFailure::Transport { dispatched: false }, None),
        };
        let dispatched = match failure {
            SttFailure::Final(error) => return Err(error),
            SttFailure::Transport { dispatched } => dispatched,
        };
        if request.scope.is_cancelled() {
            return Err(HaError::NotDispatched(NotDispatched::Cancelled));
        }
        let wait = self.options().stt_reconnect_wait;
        let Some(link) = self.wait_for_link(failed_generation, wait).await else {
            return Err(if dispatched {
                HaError::ConnectionLost
            } else {
                HaError::NotDispatched(NotDispatched::Unavailable)
            });
        };
        match self.stt_attempt(&link, &request).await {
            Ok(transcript) => Ok(transcript),
            Err(SttFailure::Final(error)) => Err(error),
            Err(SttFailure::Transport {
                dispatched: retry_dispatched,
            }) => Err(if dispatched || retry_dispatched {
                HaError::ConnectionLost
            } else {
                HaError::NotDispatched(NotDispatched::Unavailable)
            }),
        }
    }

    async fn stt_attempt(
        &self,
        link: &Link,
        request: &SttRequest,
    ) -> Result<Transcript, SttFailure> {
        let body = run_body(
            "stt",
            "stt",
            // `no_vad` turns HA's voice command segmenter off, so HA transcribes exactly the
            // audio sent, up to the empty frame that ends it.
            json!({"sample_rate": STT_SAMPLE_RATE_HZ, "no_vad": true}),
            &request.target,
            STT_RUN_TIMEOUT,
        );
        let (items, mut received) = mpsc::unbounded_channel();
        let id = match link
            .dispatch(Some(&request.scope), body, Route::Run(items))
            .await
        {
            Ok(id) => id,
            Err(NotDispatched::Cancelled) => {
                return Err(SttFailure::Final(HaError::NotDispatched(
                    NotDispatched::Cancelled,
                )));
            }
            Err(NotDispatched::Unavailable) => {
                return Err(SttFailure::Transport { dispatched: false });
            }
        };
        let _route = RouteGuard::new(link, id);
        let transport = || SttFailure::Transport { dispatched: true };
        let attempt = async {
            loop {
                let event = match received.recv().await {
                    None | Some(RunItem::Lost) => return Err(transport()),
                    Some(RunItem::Started) => continue,
                    Some(RunItem::Refused(refusal)) => {
                        return Err(SttFailure::Final(refusal.into()));
                    }
                    Some(RunItem::Event(event)) => {
                        parse_event(event).map_err(|err| SttFailure::Final(bad(err)))?
                    }
                };
                match event.kind {
                    EventKind::RunStart => {
                        let data: RunStartData =
                            parse(event.data).map_err(|err| SttFailure::Final(bad(err)))?;
                        let handler = data
                            .runner_data
                            .and_then(|runner| runner.stt_binary_handler_id)
                            .and_then(|id| u8::try_from(id).ok())
                            .filter(|id| *id != 0)
                            .ok_or_else(|| {
                                SttFailure::Final(bad("run-start without a binary handler id"))
                            })?;
                        if send_audio(link, handler, &request.audio).await.is_err() {
                            return Err(transport());
                        }
                    }
                    EventKind::SttEnd => {
                        let data: SttEndData =
                            parse(event.data).map_err(|err| SttFailure::Final(bad(err)))?;
                        return Ok(Transcript {
                            text: data.stt_output.text.trim().to_owned(),
                        });
                    }
                    EventKind::Error => {
                        let data: ErrorData =
                            parse(event.data).map_err(|err| SttFailure::Final(bad(err)))?;
                        return Err(SttFailure::Final(HaError::Rejected {
                            code: data.code,
                            message: data.message,
                        }));
                    }
                    EventKind::RunEnd => {
                        return Err(SttFailure::Final(bad("the STT run ended without stt-end")));
                    }
                    _ => {}
                }
            }
        };
        // HA stops the run itself at the timeout it was given; past that plus one reply
        // deadline, HA counts as silent. A timeout is not a transport failure and is not
        // retried, so the turn's wait stays bounded.
        let deadline = STT_RUN_TIMEOUT + self.options().command_timeout;
        tokio::time::timeout(deadline, attempt)
            .await
            .unwrap_or(Err(SttFailure::Final(HaError::Timeout)))
    }

    pub(crate) async fn synthesize(&self, request: TtsRequest) -> Result<MediaUrl, HaError> {
        let link = self
            .current_link()
            .ok_or(HaError::NotDispatched(NotDispatched::Unavailable))?;
        let body = run_body(
            "tts",
            "tts",
            json!({"text": request.text}),
            &request.target,
            TTS_RUN_TIMEOUT,
        );
        let (items, mut received) = mpsc::unbounded_channel();
        let id = link
            .dispatch(Some(&request.scope), body, Route::Run(items))
            .await
            .map_err(HaError::NotDispatched)?;
        let _route = RouteGuard::new(&link, id);
        let run = async {
            loop {
                let event = match received.recv().await {
                    None | Some(RunItem::Lost) => return Err(HaError::ConnectionLost),
                    Some(RunItem::Started) => continue,
                    Some(RunItem::Refused(refusal)) => return Err(refusal.into()),
                    Some(RunItem::Event(event)) => parse_event(event).map_err(bad)?,
                };
                match event.kind {
                    EventKind::TtsEnd => {
                        let data: TtsEndData = parse(event.data).map_err(bad)?;
                        return self
                            .endpoint()
                            .absolute(&data.tts_output.url)
                            .ok_or_else(|| bad("unusable media URL"));
                    }
                    EventKind::Error => {
                        let data: ErrorData = parse(event.data).map_err(bad)?;
                        return Err(HaError::Rejected {
                            code: data.code,
                            message: data.message,
                        });
                    }
                    EventKind::RunEnd => return Err(bad("the TTS run ended without tts-end")),
                    _ => {}
                }
            }
        };
        let deadline = TTS_RUN_TIMEOUT + self.options().command_timeout;
        tokio::time::timeout(deadline, run)
            .await
            .unwrap_or(Err(HaError::Timeout))
    }
}

/// Sends the audio as binary frames, each prefixed with the handler byte HA assigned, then
/// the handler byte alone, which ends HA's audio stream.
async fn send_audio(link: &Link, handler: u8, audio: &[i16]) -> Result<(), ()> {
    for chunk in audio.chunks(STT_FRAME_SAMPLES) {
        let mut frame = Vec::with_capacity(1 + chunk.len() * 2);
        frame.push(handler);
        for sample in chunk {
            frame.extend_from_slice(&sample.to_le_bytes());
        }
        link.send_binary(frame).await?;
    }
    link.send_binary(vec![handler]).await
}

fn bad(detail: impl Into<String>) -> HaError {
    HaError::BadResponse(detail.into())
}
