//! Wake candidates (spec §4.1, §4.2, §3.1.5) and near misses (§4.11).
//!
//! A `wake.candidate` is answered with a `command.ack` at once: rejected with the
//! arrival refusal's reason, or accepted, which releases the candidate subscription's
//! audio. A candidate the device opened while quiet is decided on arrival; one opened
//! while the device made sound waits for the reference scorer, the echo comparison and
//! the verifier, within the wake decider's 700 ms budget. A refused candidate is closed
//! and persisted with its decision trace here. An accepted one leaves as a
//! [`WakeTransfer`] that owns its subscription, analysis, scope, trace and verifier
//! timing: the machine admits it through the wake arbiter and the conversation turns it
//! into a turn, or hands it back to be finalized as outclaimed.
//!
//! [`Wake`] owns the pending candidate, its decider and analysis, and the near misses
//! being recorded. It knows nothing of turns, replies or playback.

use std::collections::{BTreeMap, HashMap};

use echomuse_core::types::WorkScope;
use echomuse_echo::{Alignment, CandidateComparison};
use echomuse_protocol::{MessageId, StreamId, session::CommandAck, vocab::AckStatus};
use echomuse_records::{
    NearMissAudio, NearMissRecord, NearMissUuid, Score, VerificationTiming, WakeContext,
    near_misses::support_duration,
    vocab::{NearMissEnd, NearMissRecording as RecordingState},
};
use echomuse_turn::{
    ArrivalConditions, CandidateEcho, Epoch, MonoInstant, ReferenceScore, Sample, SampleSpan,
    Samples, VerifierResult, WakeDecider, WakeDecision, WakeOutcome, WakePolicy, WakeRefusal,
    WakeTrace,
};
use echomuse_uplink::{
    CandidateOpen, ChunkBody, CloseReason, Delivery, EndCause, NearMissOpen, Owner as SubOwner,
    SubscriptionId, buffer::Span, starts::CandidateStarts,
};
use serde_json::{Map, Value};

use crate::{
    analysis::{Analysis, StreamNews, mic_job, reference_job},
    io::{Effect, Now, Outbound, PersistRequest},
    machine::WakeModelInfo,
    session::{Shown, WakeCx},
    speech::{MicJob, ReferenceJob, ReferenceOutput, StreamKey, VerifyJob, VerifyOutput},
    wire::{CandidateEnd, CandidateMessage, NearMiss, decode_candidate},
};

/// A candidate the wake decider has not decided yet (the device was making sound).
struct Candidate {
    message: CandidateMessage,
    decider: WakeDecider,
    analysis: Analysis,
    /// The capture scope it arrived under.
    scope: WorkScope,
    arrived: Now,
    verify_sent: bool,
    judge_sent: bool,
    /// From `wake.candidate_end`, once it arrives.
    support_end: Option<Sample>,
    end_peak: Option<f64>,
    /// Milliseconds after arrival when the verifier's input was complete and sent.
    verify_input_ms: Option<u64>,
}

impl Candidate {
    /// The verifier's timing for the decision trace: when its input went, and how far
    /// past the open the mic had reached.
    fn verification_timing(&self) -> VerificationTiming {
        let open = self.message.candidate.open();
        VerificationTiming {
            input_ready_ms: self.verify_input_ms,
            mic_past_open_ms: self
                .analysis
                .mic_end
                .zip(open)
                .map(|(end, open)| end.0.saturating_sub(open.0) / 16),
        }
    }

    /// `[support_start, open)`.
    fn support(&self) -> Option<SampleSpan> {
        let open = self.message.candidate.open()?;
        Some(SampleSpan::new(self.message.candidate.support_start, open))
    }
}

/// What a decided, accepted wake carries into its turn.
#[derive(Clone, Debug)]
pub(crate) struct AcceptedWake {
    pub(crate) message: CandidateMessage,
    pub(crate) trace: WakeTrace,
    pub(crate) support_end: Option<Sample>,
    pub(crate) end_peak: Option<f64>,
    /// The verifier's timing when it was decided.
    pub(crate) verification: VerificationTiming,
}

/// An accepted candidate leaving the wake handling: its subscription (named in the
/// message), its analysis with the evidence kept so far, and the capture scope it
/// arrived under, moved and never copied. Taking it from [`Wake`] cancels nothing.
pub(crate) struct WakeTransfer {
    pub(crate) wake: AcceptedWake,
    pub(crate) analysis: Analysis,
    pub(crate) scope: WorkScope,
    pub(crate) arrived: Now,
}

/// A near miss whose audio is being recorded.
#[derive(Debug)]
struct NearMissRecording {
    near_miss: NearMiss,
    mic_start: Sample,
    wall_ms: i64,
    /// The AFE records its subscription brought (an `afe_metadata` session), by period.
    afe: BTreeMap<u64, echomuse_protocol::ema::AfeRecord>,
}

/// What a candidate held when it was refused or outclaimed, for its row.
struct Held<'a> {
    support_end: Option<Sample>,
    afe: &'a BTreeMap<u64, echomuse_protocol::ema::AfeRecord>,
    verification: VerificationTiming,
}

/// The wake handling: the pending candidate and the near misses being recorded.
#[derive(Default)]
pub(crate) struct Wake {
    candidate: Option<Candidate>,
    /// Near misses whose recording subscription is open, by subscription.
    near_misses: HashMap<SubscriptionId, NearMissRecording>,
}

/// The device's wake context for a candidate's row: the model, the graph it ran and the
/// peak the device reported at the candidate's end.
pub(crate) fn wake_context(
    model: &WakeModelInfo,
    message: &CandidateMessage,
    end_peak: Option<f64>,
) -> WakeContext {
    WakeContext {
        model: model.name.clone(),
        graph_sha256: message
            .graph_sha256
            .clone()
            .unwrap_or_else(|| model.graph_sha256.clone()),
        candidate_end_peak: end_peak,
    }
}

fn ack_candidate(cx: &mut WakeCx<'_>, message_id: MessageId, accepted: bool, error: Option<&str>) {
    let ack = CommandAck {
        message_id,
        status: if accepted {
            AckStatus::Accepted
        } else {
            AckStatus::Rejected
        },
        error: error.map(str::to_owned),
    };
    // The candidate subscription's generation (1).
    cx.link.send(Outbound::CandidateAck(ack), 1);
}

impl Wake {
    /// A candidate is pending a decision.
    pub(crate) fn pending(&self) -> bool {
        self.candidate.is_some()
    }

    pub(crate) fn owns_subscription(&self, subscription: &SubscriptionId) -> bool {
        self.candidate
            .as_ref()
            .is_some_and(|c| &c.message.subscription == subscription)
    }

    pub(crate) fn owns_key(&self, key: &StreamKey) -> bool {
        self.candidate
            .as_ref()
            .is_some_and(|c| &c.analysis.key == key)
    }

    pub(crate) fn owns_near_miss(&self, subscription: &SubscriptionId) -> bool {
        self.near_misses.contains_key(subscription)
    }

    /// When the pending candidate's decider is next due.
    pub(crate) fn next_deadline(&self) -> Option<MonoInstant> {
        self.candidate
            .as_ref()
            .and_then(|c| c.decider.next_deadline())
    }

    /// A `wake.candidate` arrived.
    pub(crate) fn arrive(
        &mut self,
        cx: &mut WakeCx<'_>,
        message_id: MessageId,
        body: &Map<String, Value>,
        now: Now,
    ) -> Option<WakeTransfer> {
        let message = match decode_candidate(body) {
            Ok(message) => message,
            Err(broken) => {
                tracing::warn!(
                    device = %cx.facts.device,
                    candidate = ?broken.candidate_id,
                    fault = %broken.fault,
                    "a wake candidate did not decode"
                );
                cx.malformed("wake.candidate");
                ack_candidate(cx, message_id, false, Some("invalid"));
                return None;
            }
        };
        let conditions = ArrivalConditions {
            diagnostic_recording: cx.audio.diagnostic_live(),
            speech_unavailable: !cx.facts.speech_ready,
            muted: cx.facts.muted || cx.facts.capture.is_none(),
            candidate_pending: self.candidate.is_some(),
        };
        let policy = WakePolicy {
            verify_core: cx.facts.wake.verify_core.clone(),
            ..WakePolicy::default()
        };
        let decider = WakeDecider::arrive(
            message.candidate.clone(),
            conditions,
            policy.clone(),
            cx.facts.mono(now.at),
        );
        let decision = decider.decision();
        cx.note(now, || crate::evidence::Evidence::WakeArrived {
            candidate: message.candidate.clone(),
            conditions,
            policy,
            decision,
        });
        if let Some(WakeDecision {
            outcome: WakeOutcome::Refused(refusal),
            ..
        }) = decider.decision()
            && decider.trace().rule == Some(echomuse_turn::wake::WakeRule::Arrival)
        {
            ack_candidate(cx, message_id, false, Some(refusal_code(refusal)));
            cx.counters.candidates_refused += 1;
            if matches!(
                refusal,
                WakeRefusal::SpeechUnavailable | WakeRefusal::DiagnosticRecording
            ) {
                cx.show(Shown::Cue(echomuse_presentation::ring::Cue::Error), now);
            }
            persist_candidate(cx, &message, decider.trace(), None, false, None, now);
            return None;
        }
        let Some(scope) = cx.facts.capture.clone() else {
            // Muted is an arrival refusal above; capture cannot be missing here.
            ack_candidate(cx, message_id, false, Some("muted"));
            return None;
        };
        let afe = cx.audio.terms().afe_metadata;
        let starts = CandidateStarts::new(message.candidate.support_start, afe);
        let accepted = cx.audio.accept_candidate(
            CandidateOpen {
                subscription_id: message.subscription.clone(),
                capture_epoch: message.capture_epoch,
                support_start: message.candidate.support_start,
            },
            cx.facts.mono(now.at),
        );
        if let Err(error) = accepted {
            // The device named an epoch it has not announced or one already replaced:
            // nothing can be recorded for it.
            tracing::warn!(device = %cx.facts.device, %error, "a wake candidate's subscription was refused");
            ack_candidate(cx, message_id, false, Some("invalid"));
            return None;
        }
        ack_candidate(cx, message_id, true, None);
        let epoch = Epoch::new(message.capture_epoch.get())?;
        let key = StreamKey {
            session: cx.facts.session,
            subscription: message.subscription.clone(),
            epoch,
        };
        let analysis = Analysis::new(key.clone(), message.capture_epoch);
        let candidate = self.candidate.insert(Candidate {
            message,
            decider,
            analysis,
            scope,
            arrived: now,
            verify_sent: false,
            judge_sent: false,
            support_end: None,
            end_peak: None,
            verify_input_ms: None,
        });
        cx.audio.open_analysis(
            cx.link,
            &candidate.scope,
            key.clone(),
            echomuse_echo::EchoPlan {
                epoch,
                cells_from: starts.cells.cell(),
                vad_from: starts.mic.cell(),
            },
            cx.facts.wake.files.clone(),
        );
        match candidate.decider.decision() {
            Some(decision) => self.decided(cx, decision, now),
            None => {
                // Producing sound: compare the support with the reference now; the
                // verifier and the reference judge follow as their inputs arrive.
                if let Some(support) = candidate.support() {
                    let id = candidate.message.candidate.candidate_id.clone();
                    mic_job(
                        cx.link,
                        &candidate.scope,
                        MicJob::CompareCandidate {
                            key,
                            candidate: id,
                            support,
                        },
                    );
                }
                None
            }
        }
    }

    /// `wake.candidate_end` for the pending candidate; `false` when it names another.
    pub(crate) fn candidate_end(&mut self, end: &CandidateEnd) -> bool {
        match self
            .candidate
            .as_mut()
            .filter(|c| c.message.candidate.candidate_id == end.candidate_id)
        {
            Some(candidate) => {
                candidate.support_end = Some(end.support_end);
                candidate.end_peak = end.peak_smoothed;
                true
            }
            None => false,
        }
    }

    /// Audio or the end of the pending candidate's subscription.
    pub(crate) fn delivery(
        &mut self,
        cx: &mut WakeCx<'_>,
        delivery: Delivery,
        now: Now,
    ) -> Option<WakeTransfer> {
        let news = match delivery {
            Delivery::Ended(ended) => StreamNews::Ended(ended),
            Delivery::Chunk(chunk) => {
                let candidate = self.candidate.as_mut()?;
                // A candidate's evidence waits for its turn's policy: nothing is fresh.
                let (news, _) = candidate.analysis.chunk(cx.link, &candidate.scope, chunk)?;
                news
            }
        };
        match news {
            StreamNews::Mic => self.maybe_verify(cx, now),
            StreamNews::Ended(ended) => {
                // The device or a mute ended it before a decision: it can never be
                // verified now.
                tracing::info!(device = %cx.facts.device, cause = ?ended.cause, "a pending candidate's subscription ended");
                self.evidence_failed(cx, now)
            }
            StreamNews::Other => None,
        }
    }

    /// Submits the verification decode once the verification window is held.
    fn maybe_verify(&mut self, cx: &mut WakeCx<'_>, now: Now) -> Option<WakeTransfer> {
        let candidate = self.candidate.as_mut()?;
        if candidate.verify_sent {
            return None;
        }
        let window = candidate.decider.trace().verification_window?;
        if candidate
            .analysis
            .mic_end
            .is_none_or(|end| end < window.end)
        {
            return None;
        }
        let mut pcm = Vec::new();
        let read = cx.audio.timeline().read_pcm(
            StreamId::Mic,
            candidate.analysis.capture_epoch,
            Span {
                start: window.start.0,
                end: window.end.0,
            },
            &mut pcm,
        );
        let candidate_id = candidate.message.candidate.candidate_id.clone();
        let input_ms = u64::try_from(
            now.at
                .saturating_duration_since(candidate.arrived.at)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        candidate.verify_sent = true;
        candidate.verify_input_ms = Some(input_ms);
        match read {
            Ok(()) => {
                if !candidate.scope.is_cancelled() {
                    cx.link.effects.push(Effect::Verify {
                        scope: candidate.scope.clone(),
                        job: VerifyJob {
                            candidate: candidate_id,
                            pcm: pcm.into(),
                        },
                    });
                }
                None
            }
            Err(error) => {
                // Part of the window never arrived: nothing to verify against.
                tracing::info!(device = %cx.facts.device, %error, "the verification window is not held");
                self.verified(cx, &candidate_id, VerifierResult::Error, now)
            }
        }
    }

    pub(crate) fn verify_result(
        &mut self,
        cx: &mut WakeCx<'_>,
        candidate: &str,
        result: Result<VerifyOutput, echomuse_core::types::RuntimeError>,
        now: Now,
    ) -> Option<WakeTransfer> {
        let verdict = match result {
            Ok(output) => output.result,
            Err(error) => {
                cx.worker_failed(&error);
                VerifierResult::Error
            }
        };
        self.verified(cx, candidate, verdict, now)
    }

    fn verified(
        &mut self,
        cx: &mut WakeCx<'_>,
        id: &str,
        verdict: VerifierResult,
        now: Now,
    ) -> Option<WakeTransfer> {
        let mono = cx.facts.mono(now.at);
        let Some(candidate) = self
            .candidate
            .as_mut()
            .filter(|c| c.message.candidate.candidate_id == id)
        else {
            cx.counters.stale_results += 1;
            return None;
        };
        let decision = candidate.decider.verified(verdict.clone(), mono);
        cx.note(now, || crate::evidence::Evidence::WakeVerified {
            candidate_id: id.to_owned(),
            result: verdict,
            decision,
        });
        self.decided(cx, decision?, now)
    }

    /// The reference placement for the pending candidate's stream: its reference judge
    /// goes once.
    pub(crate) fn alignment(&mut self, cx: &mut WakeCx<'_>, alignment: Alignment) {
        let Some(candidate) = self.candidate.as_mut() else {
            return;
        };
        if candidate.judge_sent {
            return;
        }
        candidate.judge_sent = true;
        if let Some(support) = candidate.support() {
            reference_job(
                cx.link,
                &candidate.scope,
                ReferenceJob::Judge {
                    key: candidate.analysis.key.clone(),
                    candidate: candidate.message.candidate.candidate_id.clone(),
                    support,
                    alignment,
                },
            );
        }
    }

    pub(crate) fn comparison(
        &mut self,
        cx: &mut WakeCx<'_>,
        id: &str,
        comparison: CandidateComparison,
        now: Now,
    ) -> Option<WakeTransfer> {
        let mono = cx.facts.mono(now.at);
        let candidate = self
            .candidate
            .as_mut()
            .filter(|c| c.message.candidate.candidate_id == id)?;
        let decision = candidate.decider.echo_compared(comparison.echo, mono);
        cx.note(now, || crate::evidence::Evidence::WakeEcho {
            candidate_id: id.to_owned(),
            echo: comparison.echo,
            decision,
        });
        self.decided(cx, decision?, now)
    }

    /// The microphone worker's VAD, echo labels and ASR for the pending candidate's
    /// stream: kept with its analysis for the turn's policy.
    pub(crate) fn worker_evidence(&mut self, output: crate::speech::MicOutput) {
        if let Some(candidate) = self.candidate.as_mut() {
            // A candidate's analysis keeps its evidence until its turn takes it.
            let _ = candidate
                .analysis
                .worker_evidence(output.vad, output.echo, output.asr);
        }
    }

    /// The reference scoring worker's answer for the pending candidate's stream.
    pub(crate) fn reference_output(
        &mut self,
        cx: &mut WakeCx<'_>,
        output: ReferenceOutput,
        now: Now,
    ) -> Option<WakeTransfer> {
        if let Some(failure) = &output.failure {
            tracing::warn!(device = %cx.facts.device, failure, "reference scoring failed");
        }
        for (id, judgement) in output.judgements {
            let mono = cx.facts.mono(now.at);
            let Some(candidate) = self
                .candidate
                .as_mut()
                .filter(|c| c.message.candidate.candidate_id == id)
            else {
                continue;
            };
            let decision = candidate.decider.reference_scored(judgement.score, mono);
            cx.note(now, || crate::evidence::Evidence::WakeReference {
                candidate_id: id,
                score: judgement.score,
                decision,
            });
            if let Some(decision) = decision {
                // The candidate is decided: no later judgement finds it.
                return self.decided(cx, decision, now);
            }
        }
        None
    }

    /// The pending candidate's evidence stopped (a worker failed, the stream ended):
    /// what is missing stays missing and the deadline decides.
    pub(crate) fn evidence_failed(
        &mut self,
        cx: &mut WakeCx<'_>,
        now: Now,
    ) -> Option<WakeTransfer> {
        let mono = cx.facts.mono(now.at);
        let candidate = self.candidate.as_mut()?;
        let id = candidate.message.candidate.candidate_id.clone();
        let mut notes = Vec::new();
        let mut decision = candidate
            .decider
            .echo_compared(CandidateEcho::Unknown, mono);
        notes.push(crate::evidence::Evidence::WakeEcho {
            candidate_id: id.clone(),
            echo: CandidateEcho::Unknown,
            decision,
        });
        if decision.is_none() {
            decision = candidate
                .decider
                .reference_scored(ReferenceScore::Unavailable, mono);
            notes.push(crate::evidence::Evidence::WakeReference {
                candidate_id: id.clone(),
                score: ReferenceScore::Unavailable,
                decision,
            });
        }
        if decision.is_none() && !candidate.verify_sent {
            candidate.verify_sent = true;
            decision = candidate.decider.verified(VerifierResult::Error, mono);
            notes.push(crate::evidence::Evidence::WakeVerified {
                candidate_id: id,
                result: VerifierResult::Error,
                decision,
            });
        }
        for note in notes {
            cx.note(now, || note);
        }
        self.decided(cx, decision?, now)
    }

    /// The reference scoring worker failed: the pending candidate's reference score is
    /// unavailable.
    pub(crate) fn reference_failed(
        &mut self,
        cx: &mut WakeCx<'_>,
        now: Now,
    ) -> Option<WakeTransfer> {
        let mono = cx.facts.mono(now.at);
        let candidate = self.candidate.as_mut()?;
        let id = candidate.message.candidate.candidate_id.clone();
        let decision = candidate
            .decider
            .reference_scored(ReferenceScore::Unavailable, mono);
        cx.note(now, || crate::evidence::Evidence::WakeReference {
            candidate_id: id,
            score: ReferenceScore::Unavailable,
            decision,
        });
        self.decided(cx, decision?, now)
    }

    pub(crate) fn timer(&mut self, cx: &mut WakeCx<'_>, now: Now) -> Option<WakeTransfer> {
        let mono = cx.facts.mono(now.at);
        let candidate = self.candidate.as_mut()?;
        if candidate
            .decider
            .next_deadline()
            .is_none_or(|due| due > mono)
        {
            return None;
        }
        let id = candidate.message.candidate.candidate_id.clone();
        let decision = candidate.decider.on_timer(mono);
        cx.note(now, || crate::evidence::Evidence::WakeTimer {
            candidate_id: id,
            decision,
        });
        self.decided(cx, decision?, now)
    }

    /// Mute or session loss before the decision: the candidate ends and its
    /// subscription is already gone. The session's capture permission is the
    /// machine's; nothing here touches it.
    pub(crate) fn revoke(&mut self, cx: &mut WakeCx<'_>) {
        let Some(candidate) = self.candidate.take() else {
            return;
        };
        cx.audio
            .close_analysis(cx.link, candidate.analysis.key.clone());
        // The decider has no refusal for a revoke after arrival; its trace stays
        // undecided, which a candidate row cannot hold, so the candidate is logged.
        tracing::info!(
            device = %cx.facts.device,
            candidate = %candidate.message.candidate.candidate_id,
            "a pending wake candidate ended with capture revoked"
        );
    }

    /// Acts on the wake decider's decision for the pending (or just arrived) candidate:
    /// a refusal is closed and persisted here; an acceptance leaves as a transfer.
    fn decided(
        &mut self,
        cx: &mut WakeCx<'_>,
        decision: WakeDecision,
        now: Now,
    ) -> Option<WakeTransfer> {
        let candidate = self.candidate.take()?;
        match decision.outcome {
            WakeOutcome::Refused(refusal) => {
                cx.counters.candidates_refused += 1;
                cx.audio.close(
                    &candidate.message.subscription,
                    CloseReason::Rejected,
                    cx.facts.mono(now.at),
                );
                cx.audio
                    .close_analysis(cx.link, candidate.analysis.key.clone());
                tracing::info!(device = %cx.facts.device, ?refusal, "wake candidate refused");
                persist_candidate(
                    cx,
                    &candidate.message,
                    candidate.decider.trace(),
                    candidate.end_peak,
                    false,
                    Some(Held {
                        support_end: candidate.support_end,
                        afe: &candidate.analysis.afe,
                        verification: candidate.verification_timing(),
                    }),
                    now,
                );
                None
            }
            WakeOutcome::Accepted => {
                cx.counters.candidates_accepted += 1;
                let verification = candidate.verification_timing();
                let Candidate {
                    message,
                    decider,
                    analysis,
                    scope,
                    arrived,
                    support_end,
                    end_peak,
                    ..
                } = candidate;
                Some(WakeTransfer {
                    wake: AcceptedWake {
                        message,
                        trace: decider.trace().clone(),
                        support_end,
                        end_peak,
                        verification,
                    },
                    analysis,
                    scope,
                    arrived,
                })
            }
        }
    }

    /// Another device took the wake and this one may not answer it: the candidate is
    /// closed and persisted as outclaimed.
    pub(crate) fn outclaimed(&self, cx: &mut WakeCx<'_>, transfer: WakeTransfer, now: Now) {
        let WakeTransfer { wake, analysis, .. } = transfer;
        cx.audio.close(
            &wake.message.subscription,
            CloseReason::ArbitrationLost,
            cx.facts.mono(now.at),
        );
        cx.audio.close_analysis(cx.link, analysis.key.clone());
        persist_candidate(
            cx,
            &wake.message,
            &wake.trace,
            wake.end_peak,
            true,
            Some(Held {
                support_end: wake.support_end,
                afe: &analysis.afe,
                verification: wake.verification,
            }),
            now,
        );
    }

    /// An accepted wake whose capture scope was cancelled before it could claim the
    /// room: its subscription and analysis close, and no turn starts.
    pub(crate) fn withdraw(&self, cx: &mut WakeCx<'_>, transfer: WakeTransfer, now: Now) {
        cx.audio.close(
            &transfer.wake.message.subscription,
            CloseReason::Closed,
            cx.facts.mono(now.at),
        );
        cx.audio.close_analysis(cx.link, transfer.analysis.key);
    }

    // ---- near misses ----------------------------------------------------------------

    pub(crate) fn near_miss(&mut self, cx: &mut WakeCx<'_>, near_miss: NearMiss, now: Now) {
        let save = cx.facts.settings.wakeword.save_wake_clips;
        let permitted = !cx.facts.muted && cx.facts.capture.is_some();
        if save && permitted && cx.audio.terms().near_miss {
            let mono = cx.facts.mono(now.at);
            let opened = SubOwner::new(near_miss.id.clone()).and_then(|owner| {
                cx.audio
                    .open_near_miss(
                        NearMissOpen {
                            near_miss_id: owner,
                            capture_epoch: near_miss.capture_epoch,
                            support_start: near_miss.support_start,
                            support_end: near_miss.support_end,
                        },
                        mono,
                    )
                    .ok()
            });
            if let Some(subscription) = opened {
                let mic_start = (near_miss.support_start - Samples(4_800)).round_down(512);
                self.near_misses.insert(
                    subscription,
                    NearMissRecording {
                        near_miss,
                        mic_start,
                        wall_ms: now.wall_ms,
                        afe: BTreeMap::new(),
                    },
                );
                return;
            }
            persist_near_miss(
                cx,
                &near_miss,
                NearMissAudio::NotRecorded(RecordingState::Failed),
                None,
                now.wall_ms,
            );
            return;
        }
        let state = if save {
            RecordingState::NotPermitted
        } else {
            RecordingState::Off
        };
        persist_near_miss(
            cx,
            &near_miss,
            NearMissAudio::NotRecorded(state),
            None,
            now.wall_ms,
        );
    }

    pub(crate) fn near_miss_delivery(&mut self, cx: &mut WakeCx<'_>, delivery: Delivery) {
        let Delivery::Ended(ended) = delivery else {
            // The mic stays in the uplink's timeline until the subscription closes; the
            // AFE records are kept for the chart.
            if let Delivery::Chunk(chunk) = delivery {
                match chunk.body {
                    ChunkBody::Missing(_) => {
                        tracing::debug!(device = %cx.facts.device, "near-miss audio missing");
                    }
                    ChunkBody::Data {
                        data: echomuse_uplink::ChunkData::Afe(records),
                        ..
                    } => {
                        if let Some(recording) = self.near_misses.get_mut(&chunk.subscription) {
                            crate::analysis::store_afe(
                                &mut recording.afe,
                                chunk.span.start,
                                records,
                            );
                        }
                    }
                    ChunkBody::Data { .. } => {}
                }
            }
            return;
        };
        let Some(recording) = self.near_misses.remove(&ended.subscription) else {
            return;
        };
        let near_miss = &recording.near_miss;
        let end = near_miss.support_end + Samples(4_800);
        let audio = match ended.cause {
            EndCause::Closed(CloseReason::Closed) => {
                let mut pcm = Vec::new();
                match cx.audio.timeline().read_pcm(
                    StreamId::Mic,
                    near_miss.capture_epoch,
                    Span {
                        start: recording.mic_start.0,
                        end: end.0,
                    },
                    &mut pcm,
                ) {
                    Ok(()) => NearMissAudio::Recorded(pcm.into()),
                    Err(_) => NearMissAudio::NotRecorded(RecordingState::Failed),
                }
            }
            EndCause::Muted | EndCause::SessionLost => {
                NearMissAudio::NotRecorded(RecordingState::NotPermitted)
            }
            _ => NearMissAudio::NotRecorded(RecordingState::Failed),
        };
        // The chart comes only with the recording (afe-metadata.md §2.1): from a second
        // before the support through the recording's end.
        let series = match &audio {
            NearMissAudio::Recorded(_) => {
                let support = (near_miss.support_start.0, near_miss.support_end.0);
                let spans = crate::afe::Spans {
                    support: Some(support),
                    recording: Some((recording.mic_start.0, end.0)),
                    ..crate::afe::Spans::default()
                };
                let start = support.0.saturating_sub(crate::afe::LEAD);
                crate::afe::series(start, end.0, &recording.afe, &spans, None)
            }
            NearMissAudio::NotRecorded(_) => None,
        };
        persist_near_miss(cx, &recording.near_miss, audio, series, recording.wall_ms);
    }

    /// Mute or session loss: the near misses being recorded are forgotten (their
    /// subscriptions end with the capture).
    pub(crate) fn clear_near_misses(&mut self) {
        self.near_misses.clear();
    }
}

/// Persists a refused or outclaimed candidate with its decision trace and, in an
/// `afe_metadata` session, the AFE evidence of what its subscription brought. `held` is
/// what the candidate held (its support's end, records and verifier timing); `None` for
/// one refused on arrival.
fn persist_candidate(
    cx: &mut WakeCx<'_>,
    message: &CandidateMessage,
    trace: &WakeTrace,
    end_peak: Option<f64>,
    lost_arbitration: bool,
    held: Option<Held<'_>>,
    now: Now,
) {
    let context = wake_context(cx.facts.wake, message, end_peak);
    let record = echomuse_records::TurnRecord::candidate(
        cx.facts.device.clone(),
        crate::rows::wall_seconds(now.wall_ms),
        trace,
        context,
        lost_arbitration,
        crate::rows::policy_hash(None),
        std::time::Duration::from_millis(trace.decided_ms.unwrap_or(0)),
    );
    match record {
        Ok(mut record) => {
            record.set_trace_details(
                None,
                &echomuse_records::TraceDetails {
                    verification: held
                        .as_ref()
                        .map(|held| held.verification)
                        .unwrap_or_default(),
                },
            );
            if cx.audio.terms().afe_metadata {
                let none = BTreeMap::new();
                let start = message.candidate.support_start;
                let (support_end, records) = held
                    .as_ref()
                    .map_or((None, &none), |h| (h.support_end, h.afe));
                let end = support_end.unwrap_or_else(|| {
                    message.candidate.open().unwrap_or(start) + Samples::from_ms(480)
                });
                record.set_afe_evidence(crate::afe::evidence(
                    records,
                    Some((start.0, end.0)),
                    None,
                ));
            }
            let call = cx.ids.call();
            cx.link.effects.push(Effect::Persist {
                call,
                request: PersistRequest::Turn {
                    record: Box::new(record),
                    audio: echomuse_records::TurnAudio::default(),
                    fix_follows: None,
                },
            });
        }
        Err(error) => {
            tracing::warn!(device = %cx.facts.device, ?error, "a candidate row could not be built");
        }
    }
}

fn persist_near_miss(
    cx: &mut WakeCx<'_>,
    near_miss: &NearMiss,
    audio: NearMissAudio,
    afe_series: Option<echomuse_records::AfeSeries>,
    wall_ms: i64,
) {
    let (Some(uuid), Some(floor), Some(peak)) = (
        NearMissUuid::new(near_miss.id.clone()),
        Score::new(near_miss.floor),
        Score::new(near_miss.peak),
    ) else {
        cx.malformed("wake.near_miss");
        return;
    };
    let Some(end_reason) = NearMissEnd::parse(&near_miss.reason) else {
        cx.malformed("wake.near_miss");
        return;
    };
    let wake_threshold = cx.facts.wake.thresholds.and_then(|(idle, playback)| {
        Score::new(match near_miss.profile {
            echomuse_turn::WakeProfile::Idle => idle,
            echomuse_turn::WakeProfile::Playback => playback,
        })
    });
    let record = NearMissRecord {
        device: cx.facts.device.clone(),
        ts: crate::rows::wall_seconds(wall_ms),
        uuid,
        profile: near_miss.profile,
        floor,
        peak,
        wake_threshold,
        wake_model_sha256: near_miss.graph_sha256.clone(),
        duration: support_duration(near_miss.support_start.0, near_miss.support_end.0),
        end_reason,
        afe_series,
    };
    let call = cx.ids.call();
    cx.link.effects.push(Effect::Persist {
        call,
        request: PersistRequest::NearMiss {
            record: Box::new(record),
            audio,
        },
    });
}

/// The `command.ack` error code for an arrival refusal: the refusal's own spelling.
fn refusal_code(refusal: WakeRefusal) -> &'static str {
    echomuse_records::vocab::CandidateRefusal::from(refusal).as_str()
}
