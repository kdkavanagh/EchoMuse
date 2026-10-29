//! Wake candidates (spec §4.1, §4.2, §3.1.5) and near misses (§4.11).
//!
//! A `wake.candidate` is answered with a `command.ack` at once: rejected with the
//! arrival refusal's reason, or accepted, which releases the candidate subscription's
//! audio. A candidate the device opened while quiet is decided on arrival; one opened
//! while the device made sound waits for the reference scorer, the echo comparison and
//! the verifier, within the wake decider's 700 ms budget. An accepted candidate claims
//! the room through the wake arbiter (unless it interrupts this device's own audible
//! reply) and becomes a turn by renewing its subscription at the next generation; a
//! refused or outclaimed one is closed and persisted with its decision trace.

use echomuse_core::types::WorkScope;
use echomuse_echo::{Alignment, CandidateComparison, ReferenceJudgement};
use echomuse_protocol::{MessageId, StreamId, session::CommandAck, vocab::AckStatus};
use echomuse_records::{
    NearMissAudio, NearMissRecord, NearMissUuid, Score, WakeContext,
    near_misses::support_duration,
    vocab::{NearMissEnd, NearMissRecording as RecordingState},
};
use echomuse_turn::{
    ArrivalConditions, CandidateEcho, Epoch, ReferenceScore, Sample, SampleSpan, Samples,
    VerifierResult, WakeDecider, WakeDecision, WakeOutcome, WakePolicy, WakeRefusal,
};
use echomuse_uplink::{
    CandidateOpen, ChunkBody, CloseReason, Delivery, EndCause, NearMissOpen, Owner as SubOwner,
    buffer::Span, starts::CandidateStarts,
};
use serde_json::{Map, Value};

use crate::{
    analysis::{Analysis, Owner, StreamNews},
    arbiter::ClaimDecision,
    io::{Effect, Now, Outbound, PersistRequest},
    machine::Voice,
    speech::{MicJob, ReferenceJob, StreamKey, VerifyJob},
    wire::{CandidateMessage, NearMiss, decode_candidate},
};

/// A candidate the wake decider has not decided yet (the device was making sound).
pub(crate) struct Candidate {
    pub(crate) message: CandidateMessage,
    pub(crate) decider: WakeDecider,
    pub(crate) analysis: Analysis,
    /// The capture scope it arrived under.
    pub(crate) scope: WorkScope,
    pub(crate) arrived: Now,
    verify_sent: bool,
    judge_sent: bool,
    alignment: Option<Alignment>,
    /// From `wake.candidate_end`, once it arrives.
    pub(crate) support_end: Option<Sample>,
    pub(crate) end_peak: Option<f64>,
    /// Milliseconds after arrival when the verifier's input was complete and sent.
    pub(crate) verify_input_ms: Option<u64>,
}

impl Candidate {
    /// The verifier's timing for the decision trace: when its input went, and how far
    /// past the open the mic had reached.
    pub(crate) fn verification_timing(&self) -> echomuse_records::VerificationTiming {
        let open = self.message.candidate.open();
        echomuse_records::VerificationTiming {
            input_ready_ms: self.verify_input_ms,
            mic_past_open_ms: self
                .analysis
                .mic_end
                .zip(open)
                .map(|(end, open)| end.0.saturating_sub(open.0) / 16),
        }
    }
}

/// What a decided, accepted wake carries into its turn.
#[derive(Clone, Debug)]
pub(crate) struct AcceptedWake {
    pub(crate) message: CandidateMessage,
    pub(crate) trace: echomuse_turn::WakeTrace,
    pub(crate) support_end: Option<Sample>,
    pub(crate) end_peak: Option<f64>,
    /// The verifier's timing when it was decided.
    pub(crate) verification: echomuse_records::VerificationTiming,
}

/// A near miss whose audio is being recorded.
#[derive(Debug)]
pub(crate) struct NearMissRecording {
    pub(crate) near_miss: NearMiss,
    pub(crate) mic_start: Sample,
    pub(crate) wall_ms: i64,
    /// The AFE records its subscription brought (an `afe_metadata` session), by period.
    pub(crate) afe: std::collections::BTreeMap<u64, echomuse_protocol::ema::AfeRecord>,
}

impl Voice {
    pub(crate) fn wake_policy(&self) -> WakePolicy {
        WakePolicy {
            verify_core: self.wake.verify_core.clone(),
            ..WakePolicy::default()
        }
    }

    fn ack_candidate(&mut self, message_id: MessageId, accepted: bool, error: Option<&str>) {
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
        self.link.send(Outbound::CandidateAck(ack), 1);
    }

    pub(crate) fn wake_candidate(
        &mut self,
        message_id: MessageId,
        body: &Map<String, Value>,
        now: Now,
    ) {
        let message = match decode_candidate(body) {
            Ok(message) => message,
            Err(broken) => {
                tracing::warn!(
                    device = %self.device,
                    candidate = ?broken.candidate_id,
                    fault = %broken.fault,
                    "a wake candidate did not decode"
                );
                self.malformed("wake.candidate");
                self.ack_candidate(message_id, false, Some("invalid"));
                return;
            }
        };
        let conditions = ArrivalConditions {
            diagnostic_recording: self.uplink.diagnostic_live(),
            speech_unavailable: !self.speech_ready,
            muted: self.muted || self.capture.is_none(),
            candidate_pending: self.candidate.is_some(),
        };
        let policy = self.wake_policy();
        let decider = WakeDecider::arrive(
            message.candidate.clone(),
            conditions,
            policy.clone(),
            self.mono(now.at),
        );
        let decision = decider.decision();
        self.note(now, || crate::evidence::Evidence::WakeArrived {
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
            self.ack_candidate(message_id, false, Some(refusal_code(refusal)));
            self.counters.candidates_refused += 1;
            if matches!(
                refusal,
                WakeRefusal::SpeechUnavailable | WakeRefusal::DiagnosticRecording
            ) {
                self.cue(echomuse_presentation::ring::Cue::Error, now);
            }
            self.persist_candidate(&message, decider.trace(), None, false, None, now);
            return;
        }
        let Some(scope) = self.capture.clone() else {
            // Muted is an arrival refusal above; capture cannot be missing here.
            self.ack_candidate(message_id, false, Some("muted"));
            return;
        };
        let afe = self.uplink.terms().afe_metadata;
        let starts = CandidateStarts::new(message.candidate.support_start, afe);
        let grants = match self.uplink.accept_candidate(
            CandidateOpen {
                subscription_id: message.subscription.clone(),
                capture_epoch: message.capture_epoch,
                support_start: message.candidate.support_start,
            },
            self.mono(now.at),
        ) {
            Ok(grants) => grants,
            Err(error) => {
                // The device named an epoch it has not announced or one already
                // replaced: nothing can be recorded for it.
                tracing::warn!(device = %self.device, %error, "a wake candidate's subscription was refused");
                self.ack_candidate(message_id, false, Some("invalid"));
                return;
            }
        };
        self.apply_grants(&grants);
        self.ack_candidate(message_id, true, None);
        let Some(epoch) = Epoch::new(message.capture_epoch.get()) else {
            return;
        };
        let key = StreamKey {
            session: self.session,
            subscription: message.subscription.clone(),
            epoch,
        };
        let analysis = Analysis::new(key.clone(), message.capture_epoch);
        let candidate = Candidate {
            message,
            decider,
            analysis,
            scope,
            arrived: now,
            verify_sent: false,
            judge_sent: false,
            alignment: None,
            support_end: None,
            end_peak: None,
            verify_input_ms: None,
        };
        self.candidate = Some(candidate);
        self.mic_job(
            Owner::Candidate,
            MicJob::Open {
                key: key.clone(),
                plan: echomuse_echo::EchoPlan {
                    epoch,
                    cells_from: starts.cells.cell(),
                    vad_from: starts.mic.cell(),
                },
            },
        );
        self.reference_job(
            Owner::Candidate,
            ReferenceJob::Open {
                key: key.clone(),
                graph: self.wake.files.clone(),
            },
        );
        let decision = self.candidate.as_ref().and_then(|c| c.decider.decision());
        match decision {
            Some(decision) => self.candidate_decided(decision, now),
            None => {
                // Producing sound: compare the support with the reference now; the
                // verifier and the reference judge follow as their inputs arrive.
                let support = self.candidate_support();
                if let Some(support) = support {
                    self.mic_job(
                        Owner::Candidate,
                        MicJob::CompareCandidate {
                            key,
                            candidate: self.candidate_id().unwrap_or_default(),
                            support,
                        },
                    );
                }
            }
        }
    }

    fn candidate_id(&self) -> Option<String> {
        self.candidate
            .as_ref()
            .map(|c| c.message.candidate.candidate_id.clone())
    }

    /// `[support_start, open)` of the pending candidate.
    fn candidate_support(&self) -> Option<SampleSpan> {
        let candidate = self.candidate.as_ref()?;
        let open = candidate.message.candidate.open()?;
        Some(SampleSpan::new(
            candidate.message.candidate.support_start,
            open,
        ))
    }

    pub(crate) fn candidate_end(&mut self, end: crate::wire::CandidateEnd) {
        if let Some(candidate) = self
            .candidate
            .as_mut()
            .filter(|c| c.message.candidate.candidate_id == end.candidate_id)
        {
            candidate.support_end = Some(end.support_end);
            candidate.end_peak = end.peak_smoothed;
            return;
        }
        if let Some(wake) = self
            .turn
            .as_mut()
            .and_then(|t| t.wake.as_mut())
            .filter(|w| w.message.candidate.candidate_id == end.candidate_id)
        {
            wake.support_end = Some(end.support_end);
            wake.end_peak = end.peak_smoothed;
        }
    }

    /// Mic audio or the end arrived on the candidate's subscription.
    pub(crate) fn candidate_news(&mut self, news: StreamNews, now: Now) {
        match news {
            StreamNews::Mic => self.maybe_verify(now),
            StreamNews::Ended(ended) => {
                // The device or a mute ended it before a decision: it can never be
                // verified now.
                tracing::info!(device = %self.device, cause = ?ended.cause, "a pending candidate's subscription ended");
                self.candidate_evidence_failed(now);
            }
            StreamNews::Other => {}
        }
    }

    /// Submits the verification decode once the verification window is held.
    fn maybe_verify(&mut self, now: Now) {
        let Some(candidate) = self.candidate.as_ref() else {
            return;
        };
        if candidate.verify_sent {
            return;
        }
        let Some(window) = candidate.decider.trace().verification_window else {
            return;
        };
        if candidate
            .analysis
            .mic_end
            .is_none_or(|end| end < window.end)
        {
            return;
        }
        let mut pcm = Vec::new();
        let read = self.uplink.timeline().read_pcm(
            StreamId::Mic,
            candidate.analysis.capture_epoch,
            Span {
                start: window.start.0,
                end: window.end.0,
            },
            &mut pcm,
        );
        let candidate_id = candidate.message.candidate.candidate_id.clone();
        let scope = candidate.scope.clone();
        let input_ms = u64::try_from(
            now.at
                .saturating_duration_since(candidate.arrived.at)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        if let Some(candidate) = self.candidate.as_mut() {
            candidate.verify_sent = true;
            candidate.verify_input_ms = Some(input_ms);
        }
        match read {
            Ok(()) => {
                if !scope.is_cancelled() {
                    self.link.effects.push(Effect::Verify {
                        scope,
                        job: VerifyJob {
                            candidate: candidate_id,
                            pcm: pcm.into(),
                        },
                    });
                }
            }
            Err(error) => {
                // Part of the window never arrived: nothing to verify against.
                tracing::info!(device = %self.device, %error, "the verification window is not held");
                self.candidate_verified(&candidate_id, VerifierResult::Error, now);
            }
        }
    }

    pub(crate) fn verify_result(
        &mut self,
        candidate: String,
        result: Result<crate::speech::VerifyOutput, echomuse_core::types::RuntimeError>,
        now: Now,
    ) {
        let verdict = match result {
            Ok(output) => output.result,
            Err(error) => {
                self.worker_failed(&error);
                VerifierResult::Error
            }
        };
        self.candidate_verified(&candidate, verdict, now);
    }

    fn candidate_verified(&mut self, id: &str, verdict: VerifierResult, now: Now) {
        let mono = self.mono(now.at);
        let Some(candidate) = self
            .candidate
            .as_mut()
            .filter(|c| c.message.candidate.candidate_id == id)
        else {
            self.counters.stale_results += 1;
            return;
        };
        let decision = candidate.decider.verified(verdict.clone(), mono);
        self.note(now, || crate::evidence::Evidence::WakeVerified {
            candidate_id: id.to_owned(),
            result: verdict,
            decision,
        });
        if let Some(decision) = decision {
            self.candidate_decided(decision, now);
        }
    }

    pub(crate) fn candidate_alignment(&mut self, alignment: Alignment, _now: Now) {
        let Some(candidate) = self.candidate.as_mut() else {
            return;
        };
        candidate.alignment = Some(alignment);
        if candidate.judge_sent {
            return;
        }
        candidate.judge_sent = true;
        let key = candidate.analysis.key.clone();
        let id = candidate.message.candidate.candidate_id.clone();
        if let Some(support) = self.candidate_support() {
            self.reference_job(
                Owner::Candidate,
                ReferenceJob::Judge {
                    key,
                    candidate: id,
                    support,
                    alignment,
                },
            );
        }
    }

    pub(crate) fn candidate_comparison(
        &mut self,
        id: &str,
        comparison: CandidateComparison,
        now: Now,
    ) {
        let mono = self.mono(now.at);
        let Some(candidate) = self
            .candidate
            .as_mut()
            .filter(|c| c.message.candidate.candidate_id == id)
        else {
            return;
        };
        let decision = candidate.decider.echo_compared(comparison.echo, mono);
        self.note(now, || crate::evidence::Evidence::WakeEcho {
            candidate_id: id.to_owned(),
            echo: comparison.echo,
            decision,
        });
        if let Some(decision) = decision {
            self.candidate_decided(decision, now);
        }
    }

    pub(crate) fn candidate_judgement(
        &mut self,
        id: &str,
        judgement: ReferenceJudgement,
        now: Now,
    ) {
        let mono = self.mono(now.at);
        let Some(candidate) = self
            .candidate
            .as_mut()
            .filter(|c| c.message.candidate.candidate_id == id)
        else {
            return;
        };
        let decision = candidate.decider.reference_scored(judgement.score, mono);
        self.note(now, || crate::evidence::Evidence::WakeReference {
            candidate_id: id.to_owned(),
            score: judgement.score,
            decision,
        });
        if let Some(decision) = decision {
            self.candidate_decided(decision, now);
        }
    }

    /// The pending candidate's evidence stopped (a worker failed, the stream ended):
    /// what is missing stays missing and the deadline decides.
    pub(crate) fn candidate_evidence_failed(&mut self, now: Now) {
        let mono = self.mono(now.at);
        let Some(candidate) = self.candidate.as_mut() else {
            return;
        };
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
            self.note(now, || note);
        }
        if let Some(decision) = decision {
            self.candidate_decided(decision, now);
        }
    }

    pub(crate) fn candidate_reference_failed(&mut self, now: Now) {
        let mono = self.mono(now.at);
        let Some(candidate) = self.candidate.as_mut() else {
            return;
        };
        let id = candidate.message.candidate.candidate_id.clone();
        let decision = candidate
            .decider
            .reference_scored(ReferenceScore::Unavailable, mono);
        self.note(now, || crate::evidence::Evidence::WakeReference {
            candidate_id: id,
            score: ReferenceScore::Unavailable,
            decision,
        });
        if let Some(decision) = decision {
            self.candidate_decided(decision, now);
        }
    }

    pub(crate) fn candidate_timer(&mut self, now: Now) {
        let mono = self.mono(now.at);
        let Some(candidate) = self.candidate.as_mut() else {
            return;
        };
        if candidate
            .decider
            .next_deadline()
            .is_none_or(|due| due > mono)
        {
            return;
        }
        let id = candidate.message.candidate.candidate_id.clone();
        let decision = candidate.decider.on_timer(mono);
        self.note(now, || crate::evidence::Evidence::WakeTimer {
            candidate_id: id,
            decision,
        });
        if let Some(decision) = decision {
            self.candidate_decided(decision, now);
        }
    }

    /// Mute or session loss before the decision: the candidate is refused as muted and
    /// its subscription is already gone.
    pub(crate) fn end_candidate_on_revoke(&mut self, now: Now) {
        let Some(candidate) = self.candidate.take() else {
            return;
        };
        self.close_analysis(candidate.analysis.key.clone());
        // The decider has no refusal for a revoke after arrival; its trace stays
        // undecided, which a candidate row cannot hold, so the candidate is logged.
        tracing::info!(
            device = %self.device,
            candidate = %candidate.message.candidate.candidate_id,
            "a pending wake candidate ended with capture revoked"
        );
        let _ = now;
    }

    /// Acts on the wake decider's decision for the pending (or just arrived) candidate.
    pub(crate) fn candidate_decided(&mut self, decision: WakeDecision, now: Now) {
        let Some(candidate) = self.candidate.take() else {
            return;
        };
        match decision.outcome {
            WakeOutcome::Refused(refusal) => {
                self.counters.candidates_refused += 1;
                let _ = self.uplink.close(
                    &candidate.message.subscription,
                    CloseReason::Rejected,
                    self.mono(now.at),
                );
                self.close_analysis(candidate.analysis.key.clone());
                tracing::info!(device = %self.device, ?refusal, "wake candidate refused");
                self.persist_candidate(
                    &candidate.message,
                    candidate.decider.trace(),
                    candidate.end_peak,
                    false,
                    Some(&candidate),
                    now,
                );
            }
            WakeOutcome::Accepted => {
                self.counters.candidates_accepted += 1;
                self.arbitrate(candidate, now);
            }
        }
    }

    /// Claims the room for an accepted wake (§4.2) and starts its turn, or stands down.
    fn arbitrate(&mut self, candidate: Candidate, now: Now) {
        // A wake that interrupts this device's own audible reply skips arbitration:
        // the room already answered here.
        let interrupting = self.playouts.answer_playing();
        let window = std::time::Duration::from_millis(u64::from(
            self.settings.wakeword.wake_arbitration_ms.get(),
        ));
        let grant = if interrupting {
            None
        } else {
            match self
                .arbiter
                .claim(&candidate.scope, window, now.at.into_std())
            {
                Ok(ClaimDecision::Won(grant)) => Some(grant),
                Ok(ClaimDecision::Lost { winner }) => {
                    self.lost_arbitration(candidate, &winner, now);
                    return;
                }
                Err(error) => {
                    // The capture scope was cancelled in between: capture is revoked.
                    tracing::info!(device = %self.device, %error, "an accepted wake could not claim");
                    let _ = self.uplink.close(
                        &candidate.message.subscription,
                        CloseReason::Closed,
                        self.mono(now.at),
                    );
                    self.close_analysis(candidate.analysis.key.clone());
                    return;
                }
            }
        };
        self.start_wake_turn(candidate, grant, false, now);
    }

    fn lost_arbitration(
        &mut self,
        candidate: Candidate,
        winner: &echomuse_core::types::DeviceId,
        now: Now,
    ) {
        tracing::info!(device = %self.device, %winner, "another device took the wake");
        // A media-context loser may still pause or stop its own media (§4.2): the turn
        // runs, but only those commands act.
        if self.media_playing() {
            self.start_wake_turn(candidate, None, true, now);
            return;
        }
        let _ = self.uplink.close(
            &candidate.message.subscription,
            CloseReason::ArbitrationLost,
            self.mono(now.at),
        );
        self.close_analysis(candidate.analysis.key.clone());
        self.persist_candidate(
            &candidate.message,
            candidate.decider.trace(),
            candidate.end_peak,
            true,
            Some(&candidate),
            now,
        );
    }

    pub(crate) fn wake_context(
        &self,
        message: &CandidateMessage,
        end_peak: Option<f64>,
    ) -> WakeContext {
        WakeContext {
            model: self.wake.name.clone(),
            graph_sha256: message
                .graph_sha256
                .clone()
                .unwrap_or_else(|| self.wake.graph_sha256.clone()),
            candidate_end_peak: end_peak,
        }
    }

    /// Persists a refused or outclaimed candidate with its decision trace and, in an
    /// `afe_metadata` session, the AFE evidence of what its subscription brought. `pending`
    /// is the candidate as it was held (its support's end, records and verifier timing);
    /// `None` for one refused on arrival.
    fn persist_candidate(
        &mut self,
        message: &CandidateMessage,
        trace: &echomuse_turn::WakeTrace,
        end_peak: Option<f64>,
        lost_arbitration: bool,
        pending: Option<&Candidate>,
        now: Now,
    ) {
        let context = self.wake_context(message, end_peak);
        let record = echomuse_records::TurnRecord::candidate(
            self.device.clone(),
            crate::record::wall_seconds(now.wall_ms),
            trace,
            context,
            lost_arbitration,
            crate::record::policy_hash(None),
            std::time::Duration::from_millis(trace.decided_ms.unwrap_or(0)),
        );
        match record {
            Ok(mut record) => {
                record.set_trace_details(
                    None,
                    &echomuse_records::TraceDetails {
                        verification: pending
                            .map(Candidate::verification_timing)
                            .unwrap_or_default(),
                    },
                );
                if self.uplink.terms().afe_metadata {
                    let none = std::collections::BTreeMap::new();
                    let start = message.candidate.support_start;
                    let (support_end, records) =
                        pending.map_or((None, &none), |c| (c.support_end, &c.analysis.afe));
                    let end = support_end.unwrap_or_else(|| {
                        message.candidate.open().unwrap_or(start) + Samples::from_ms(480)
                    });
                    record.set_afe_evidence(crate::afe::evidence(
                        records,
                        Some((start.0, end.0)),
                        None,
                    ));
                }
                let call = self.call_id();
                self.link.effects.push(Effect::Persist {
                    call,
                    request: PersistRequest::Turn {
                        record: Box::new(record),
                        audio: echomuse_records::TurnAudio::default(),
                        fix_follows: None,
                    },
                });
            }
            Err(error) => {
                tracing::warn!(device = %self.device, ?error, "a candidate row could not be built");
            }
        }
    }

    // ---- near misses ----------------------------------------------------------------

    pub(crate) fn near_miss(&mut self, near_miss: NearMiss, now: Now) {
        let save = self.settings.wakeword.save_wake_clips;
        let permitted = !self.muted && self.capture.is_some();
        if save && permitted && self.uplink.terms().near_miss {
            let owner = SubOwner::new(near_miss.id.clone());
            let opened = owner.and_then(|owner| {
                self.uplink
                    .open_near_miss(
                        NearMissOpen {
                            near_miss_id: owner,
                            capture_epoch: near_miss.capture_epoch,
                            support_start: near_miss.support_start,
                            support_end: near_miss.support_end,
                        },
                        self.mono(now.at),
                    )
                    .ok()
            });
            if let Some(opened) = opened {
                self.apply_grants(&opened.grants);
                let mic_start = (near_miss.support_start - Samples(4_800)).round_down(512);
                self.near_misses.insert(
                    opened.subscription_id,
                    NearMissRecording {
                        near_miss,
                        mic_start,
                        wall_ms: now.wall_ms,
                        afe: std::collections::BTreeMap::new(),
                    },
                );
                return;
            }
            self.persist_near_miss(
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
        self.persist_near_miss(
            &near_miss,
            NearMissAudio::NotRecorded(state),
            None,
            now.wall_ms,
        );
    }

    pub(crate) fn near_miss_delivery(&mut self, delivery: Delivery, _now: Now) {
        let Delivery::Ended(ended) = delivery else {
            // The mic stays in the uplink's timeline until the subscription closes; the
            // AFE records are kept for the chart.
            if let Delivery::Chunk(chunk) = delivery {
                match chunk.body {
                    ChunkBody::Missing(_) => {
                        tracing::debug!(device = %self.device, "near-miss audio missing");
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
                match self.uplink.timeline().read_pcm(
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
        let near_miss = recording.near_miss.clone();
        self.persist_near_miss(&near_miss, audio, series, recording.wall_ms);
    }

    fn persist_near_miss(
        &mut self,
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
            self.malformed("wake.near_miss");
            return;
        };
        let Some(end_reason) = NearMissEnd::parse(&near_miss.reason) else {
            self.malformed("wake.near_miss");
            return;
        };
        let wake_threshold = self.wake.thresholds.and_then(|(idle, playback)| {
            Score::new(match near_miss.profile {
                echomuse_turn::WakeProfile::Idle => idle,
                echomuse_turn::WakeProfile::Playback => playback,
            })
        });
        let record = NearMissRecord {
            device: self.device.clone(),
            ts: crate::record::wall_seconds(wall_ms),
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
        let call = self.call_id();
        self.link.effects.push(Effect::Persist {
            call,
            request: PersistRequest::NearMiss {
                record: Box::new(record),
                audio,
            },
        });
    }
}

/// The `command.ack` error code for an arrival refusal: the refusal's own spelling.
fn refusal_code(refusal: WakeRefusal) -> &'static str {
    echomuse_records::vocab::CandidateRefusal::from(refusal).as_str()
}
