//! Pause transcription by a Wyoming server (spec §4.7, `pauseAsr` with engine `wyoming`):
//! the requests the machine makes, and where their answers go. What the server's words
//! change is the turn policy's to decide ([`echomuse_turn::TurnPolicy::pause_server_answer`]).
//!
//! - **Pause decodes.** When the microphone worker starts a turn's pause decode it says so
//!   ([`Input::PauseDecoding`](crate::Input::PauseDecoding)), and the same audio goes to the
//!   server at once: the utterance from its start (where the streaming recognizer started)
//!   through the pause decode's end, at the recognizers' +20 dB evidence gain, within
//!   [`PAUSE_SERVER_LIMIT`]. Should that notice have been lost, the pause decode's own result
//!   asks instead; each pause is asked about once. The request and its answer are handed to
//!   the policy and noted for the session recorder.
//! - **Re-check.** A route B or fallback commit on the server's words is re-checked by the
//!   same server: the committed span at +20 dB within [`SPAN_SERVER_LIMIT`], its wake phrase
//!   removed as from HA's transcript. A different text retries the turn; no answer
//!   interrupts it.
//! - **Identity.** An answer is applied only to the turn and call it was asked for, and only
//!   about the audio end it was asked about; a turn that ends cancels its requests with its
//!   scope.
//! - **Failure.** An unreachable server, a timeout or a protocol error is logged and handed
//!   to the policy, which records it with the pause; Kroko's words stand.

use std::{sync::Arc, time::Duration};

use echomuse_core::types::WorkScope;
use echomuse_protocol::StreamId;
use echomuse_records::vocab::TerminalReason;
use echomuse_settings::values::PauseAsr;
use echomuse_turn::{Commit, PauseServerAnswer, Sample, ServerFailure};
use echomuse_uplink::buffer::Span;
use uuid::Uuid;

use crate::{
    evidence::Evidence,
    io::{CallId, Effect, Now},
    machine::Voice,
    speech::StreamKey,
    turn::{Call, Stage},
};

/// How long a pause decode's transcription may take, from the request to the answer:
/// later than this it could not shorten even the longest pause.
pub const PAUSE_SERVER_LIMIT: Duration = Duration::from_millis(1_500);

/// How long a committed span's re-check transcription may take.
pub const SPAN_SERVER_LIMIT: Duration = Duration::from_millis(2_000);

/// A transcription for the runtime to run on the pause server.
#[derive(Clone, Debug)]
pub struct ServerRequest {
    /// The turn's scope; a turn that ended withdraws its interest.
    pub scope: WorkScope,
    /// The Wyoming server's host.
    pub host: String,
    /// Its port.
    pub port: u16,
    /// The model to ask for; empty for the server's default.
    pub model: String,
    /// The language to ask for; empty for the server's default.
    pub language: String,
    /// The audio: 16 kHz mono PCM at the recognizers' +20 dB evidence gain.
    pub pcm: Arc<[i16]>,
    /// From the request to the answer, at most.
    pub limit: Duration,
}

impl ServerRequest {
    fn new(scope: WorkScope, server: &PauseAsr, pcm: &[i16], limit: Duration) -> Self {
        Self {
            scope,
            host: server.host().to_owned(),
            port: server.port(),
            model: server.model().to_owned(),
            language: server.language().to_owned(),
            // The recognizers' evidence copy: canonical × 10 (+20 dB), saturated.
            pcm: pcm.iter().map(|s| s.saturating_mul(10)).collect(),
            limit,
        }
    }
}

impl Voice {
    /// The microphone worker is starting a pause decode of stream `key`'s utterance.
    pub(crate) fn pause_decoding(&mut self, key: &StreamKey, through: Sample, now: Now) {
        let ours = self
            .turn
            .as_ref()
            .and_then(|t| t.analysis.as_ref())
            .is_some_and(|a| a.key == *key);
        if ours {
            self.ask_pause_server(through, now);
        }
    }

    /// Sends the utterance through `through` to the pause server, once per pause, while the
    /// turn listens under a policy that has one.
    pub(crate) fn ask_pause_server(&mut self, through: Sample, now: Now) {
        let mono = self.mono(now.at);
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        let Some(policy) = turn.policy.as_ref() else {
            return;
        };
        let start = policy.trace().start;
        if !policy.trace().config.pause_server
            || !matches!(turn.stage, Stage::Listening { .. })
            || turn.pause_asked.is_some_and(|asked| asked >= through)
            || through <= start
        {
            return;
        }
        let Some(epoch) = turn.epoch() else {
            return;
        };
        let mut pcm = Vec::new();
        if self
            .uplink
            .timeline()
            .read_pcm(
                StreamId::Mic,
                epoch,
                Span {
                    start: start.0,
                    end: through.0,
                },
                &mut pcm,
            )
            .is_err()
        {
            tracing::info!(device = %self.device, "the pause decode's audio is not held; the pause server is not asked");
            return;
        }
        let request = ServerRequest::new(
            turn.scope.clone(),
            &turn.settings.microphones.pause_asr,
            &pcm,
            PAUSE_SERVER_LIMIT,
        );
        let (uuid, id) = (turn.uuid, turn.evidence_id);
        let call = self.call_id();
        self.calls.insert(
            call,
            Call::PauseServer {
                turn: uuid,
                pause: through,
            },
        );
        self.link.effects.push(Effect::ServerTranscribe {
            call,
            through,
            request,
        });
        if let Some(turn) = self.turn.as_mut() {
            turn.pause_asked = Some(through);
            if let Some(policy) = turn.policy.as_mut() {
                policy.pause_server_asked(through, mono);
            }
        }
        self.note(now, || Evidence::PauseServerAsked {
            turn: id,
            pause: through,
        });
    }

    /// Re-checks a commit made on the server's words with the same server (§4.7), on the
    /// committed span `pcm`. Returns `false`, asking nothing, when the turn's utterance had
    /// no pause server.
    pub(crate) fn recheck_on_server(&mut self, commit: &Commit, pcm: &[i16]) -> bool {
        let Some(turn) = self.turn.as_ref() else {
            return false;
        };
        if !turn
            .utterance
            .as_ref()
            .is_some_and(|u| u.config.pause_server)
        {
            return false;
        }
        let request = ServerRequest::new(
            turn.scope.clone(),
            &turn.settings.microphones.pause_asr,
            pcm,
            SPAN_SERVER_LIMIT,
        );
        let uuid = turn.uuid;
        let call = self.call_id();
        self.calls.insert(call, Call::SpanServer { turn: uuid });
        self.link.effects.push(Effect::ServerTranscribe {
            call,
            through: commit.span.end,
            request,
        });
        true
    }

    /// The answer to [`Effect::ServerTranscribe`].
    pub(crate) fn server_transcript(
        &mut self,
        call: CallId,
        through: Sample,
        result: Result<String, ServerFailure>,
        now: Now,
    ) {
        match self.calls.get(&call) {
            Some(Call::PauseServer { .. } | Call::SpanServer { .. }) => {}
            _ => return,
        }
        match self.calls.remove(&call) {
            Some(Call::PauseServer { turn, pause }) if pause == through => {
                self.pause_server_answer(turn, pause, result, now);
            }
            Some(Call::SpanServer { turn }) => self.span_server_answer(turn, through, result, now),
            _ => self.counters.stale_results += 1,
        }
    }

    fn pause_server_answer(
        &mut self,
        uuid: Uuid,
        pause: Sample,
        result: Result<String, ServerFailure>,
        now: Now,
    ) {
        if !self.turn.as_ref().is_some_and(|t| t.uuid == uuid) {
            self.counters.stale_results += 1;
            return;
        }
        if let Err(failure) = &result {
            tracing::info!(device = %self.device, ?failure, "the pause server gave no words; Kroko's stand");
        }
        let mono = self.mono(now.at);
        let answer = PauseServerAnswer {
            pause,
            outcome: result,
        };
        let noted = self.recording.then(|| answer.clone());
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let id = turn.evidence_id;
        // A turn already decided has no policy: the decision came first.
        let Some(policy) = turn.policy.as_mut() else {
            return;
        };
        policy.pause_server_answer(answer, mono);
        if let Some(answer) = noted {
            self.note(now, || Evidence::PauseServerAnswer { turn: id, answer });
        }
    }

    fn span_server_answer(
        &mut self,
        uuid: Uuid,
        through: Sample,
        result: Result<String, ServerFailure>,
        now: Now,
    ) {
        let current = self.turn.as_ref().is_some_and(|t| {
            t.uuid == uuid
                && t.stage == Stage::Committed
                && t.commit
                    .as_ref()
                    .is_some_and(|(c, _, _)| c.span.end == through)
        });
        if !current {
            self.counters.stale_results += 1;
            return;
        }
        match result {
            Ok(text) => {
                if self.command_differs(&self.final_command(&text)) {
                    tracing::info!(device = %self.device, "the pause server heard the committed span differently");
                    self.end_turn(TerminalReason::Retry, now);
                } else {
                    self.transcribe(now);
                }
            }
            Err(failure) => {
                tracing::warn!(device = %self.device, ?failure, "the pause server could not re-check the commit");
                self.end_turn(TerminalReason::Interrupted, now);
            }
        }
    }
}
