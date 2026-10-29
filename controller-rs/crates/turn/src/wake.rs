//! The wake decision (§4.1; post-AFE architecture §6.1 and §16.6 "Wake verification").
//!
//! A BCResNet score on the device proposes a wake; this module decides it:
//!
//! - **At arrival** a candidate is refused, each case with its own reason, when it is malformed,
//!   a diagnostic recording is active, speech is unavailable, the device is muted, or another
//!   candidate is already pending.
//! - **Device quiet:** accepted on the device's score alone.
//! - **Device making sound:** decided from three pieces of evidence, in order. (1) A reference
//!   wake candidate overlapping the mic candidate means the device itself said something
//!   wake-like: `self_output`, unless the echo comparison shows near-end speech (double talk).
//!   (2) The echo comparison saying the mic is only the device's own output: `echo_only`. (3) An
//!   ASR check of the wake phrase over `[support_start − 300 ms, open + 480 ms)`: a failed or
//!   empty transcript is `unverified_wake`. Otherwise accepted.
//! - **Budget:** the decision is due 700 ms after the candidate arrived, on the controller's
//!   monotonic clock. At the deadline, missing reference or echo evidence is treated as
//!   unavailable (it never rejects on its own, except that an overlapping reference candidate
//!   without near-end evidence is still `self_output`), and a missing verdict refuses as
//!   `verifier_timeout`: a candidate is never accepted unverified.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::text::verification_distance;
use crate::time::{MonoInstant, Sample, SampleSpan, Samples};

/// The device's threshold profile when the candidate opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeProfile {
    /// The device was quiet.
    Idle,
    /// The device was producing sound.
    Playback,
}

/// One scored hop of a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WakeHop {
    /// The hop's last sample, exclusive.
    pub end: Sample,
    /// Its raw probability; `None` for an unscored window.
    pub raw: Option<f64>,
    /// Its smoothed probability.
    pub smoothed: Option<f64>,
}

/// A `wake.candidate` as the device reports it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WakeCandidate {
    /// The candidate's identifier.
    pub candidate_id: String,
    /// The profile that opened it.
    pub profile: WakeProfile,
    /// The threshold of the rule that opened it.
    pub threshold: f64,
    /// Whether any source was audible in the final mix during the support or the 2 s before.
    pub producing_sound: bool,
    /// The end of the first hop that crossed.
    pub first_crossing_end: Sample,
    /// The first sample of the scored support.
    pub support_start: Sample,
    /// The hops up to the open, oldest first.
    pub hops: Vec<WakeHop>,
}

impl WakeCandidate {
    /// The open: the end of the last reported hop.
    pub fn open(&self) -> Option<Sample> {
        self.hops.last().map(|h| h.end)
    }

    /// The trigger sample: the end of the last hop whose raw probability reaches the threshold.
    pub fn trigger(&self) -> Option<Sample> {
        self.hops
            .iter()
            .rev()
            .find(|h| h.raw.is_some_and(|r| r >= self.threshold))
            .map(|h| h.end)
    }

    /// The verifier's input: 300 ms before the support start to 480 ms after the open.
    pub fn verification_window(&self, policy: &WakePolicy) -> Option<SampleSpan> {
        self.open().map(|open| {
            SampleSpan::new(
                self.support_start - policy.verification_lead,
                open + policy.verification_lookahead,
            )
        })
    }

    /// What is wrong with the candidate, if anything.
    fn malformed(&self) -> Option<&'static str> {
        let ordered = self.hops.windows(2).all(|w| w[0].end < w[1].end);
        let open = self.open()?;
        if self.candidate_id.is_empty() {
            Some("empty candidate id")
        } else if !(self.threshold > 0.0 && self.threshold <= 1.0) {
            Some("threshold outside (0, 1]")
        } else if !ordered {
            Some("hops out of order")
        } else if self.support_start > self.first_crossing_end || self.first_crossing_end > open {
            Some("support, first crossing and open out of order")
        } else if self.trigger().is_none() {
            Some("no hop reaches the threshold")
        } else {
            None
        }
    }
}

/// The device runtime's state when a candidate arrives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArrivalConditions {
    /// A dashboard diagnostic recording is active.
    pub diagnostic_recording: bool,
    /// The speech path is unavailable.
    pub speech_unavailable: bool,
    /// The device is privacy-muted.
    pub muted: bool,
    /// Another candidate is still undecided.
    pub candidate_pending: bool,
}

/// Why a wake was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeRefusal {
    /// The candidate breaks the message rules.
    Malformed,
    /// A diagnostic recording is active.
    DiagnosticRecording,
    /// The speech path is unavailable.
    SpeechUnavailable,
    /// The device is muted.
    Muted,
    /// Another candidate is pending.
    CandidatePending,
    /// The device itself rendered something wake-like.
    SelfOutput,
    /// The mic held only the device's own output.
    EchoOnly,
    /// The verifier did not hear the wake word.
    UnverifiedWake,
    /// No verdict by the decision deadline.
    VerifierTimeout,
}

/// A wake decision's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeOutcome {
    /// Accepted: the turn may start.
    Accepted,
    /// Refused for this reason.
    Refused(WakeRefusal),
}

/// The rule that produced a wake decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeRule {
    /// A refusal at arrival.
    Arrival,
    /// The device was quiet: accepted on its score.
    Idle,
    /// An overlapping reference candidate without near-end speech.
    ReferenceWake,
    /// The echo comparison found only the device's output.
    Echo,
    /// The verifier heard the wake word.
    Verified,
    /// The verifier's transcript did not match.
    VerifierFailed,
    /// The verifier reported an error instead of a transcript.
    VerifierError,
    /// The decision deadline passed without a verdict.
    Deadline,
}

/// The reference scorer's evidence for the candidate's support.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceScore {
    /// No reference candidate overlaps the mic candidate.
    NoOverlap,
    /// A reference candidate overlaps it, mapped into mic time.
    Overlap,
    /// The reference could not be scored (missing or uncovered).
    Unavailable,
}

/// The candidate-level echo comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateEcho {
    /// The device's own output explains the mic.
    EchoOnly,
    /// Near-end speech the reference does not explain is present.
    NearEndPresent,
    /// The comparison could not decide.
    Unknown,
}

/// What the verifier returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifierResult {
    /// The decoded transcript, possibly empty.
    Text(String),
    /// The decode failed.
    Error,
}

/// The wake decision's values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WakePolicy {
    /// The decision budget from the candidate's arrival, in milliseconds (initially 700).
    pub decision_budget_ms: u64,
    /// The registry entry's `verify_core` (`ophel` for the deployed model).
    pub verify_core: String,
    /// A transcript passes at or below this distance.
    pub verify_pass_max: f64,
    /// The verifier's input starts this far before the support start.
    pub verification_lead: Samples,
    /// ...and ends this far after the open.
    pub verification_lookahead: Samples,
}

impl Default for WakePolicy {
    fn default() -> Self {
        Self {
            decision_budget_ms: 700,
            verify_core: "ophel".to_owned(),
            verify_pass_max: crate::text::VERIFY_PASS_MAX,
            verification_lead: Samples::from_ms(300),
            verification_lookahead: Samples::from_ms(480),
        }
    }
}

/// The verifier's verdict as recorded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerificationRecord {
    /// The transcript, or `None` when the decode failed.
    pub text: Option<String>,
    /// Its distance to the core; `None` when no substring could be compared.
    pub distance: Option<f64>,
    /// Whether the text clears the match.
    pub passes: bool,
    /// When it arrived, in milliseconds after the candidate.
    pub at_ms: u64,
    /// Whether it arrived after the candidate was decided.
    pub late: bool,
}

/// One wake candidate's decision trace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WakeTrace {
    /// The candidate as reported.
    pub candidate: WakeCandidate,
    /// The verifier's input range, when the device was producing sound.
    pub verification_window: Option<SampleSpan>,
    /// The decision budget, in milliseconds.
    pub deadline_ms: u64,
    /// What was wrong with a malformed candidate.
    pub malformed: Option<String>,
    /// The reference scorer's evidence and when it arrived.
    pub reference: Option<(ReferenceScore, u64)>,
    /// The echo comparison and when it arrived.
    pub echo: Option<(CandidateEcho, u64)>,
    /// The verifier's verdict.
    pub verification: Option<VerificationRecord>,
    /// The outcome.
    pub outcome: Option<WakeOutcome>,
    /// The rule that produced it.
    pub rule: Option<WakeRule>,
    /// When it was decided, in milliseconds after the candidate.
    pub decided_ms: Option<u64>,
}

/// A decision, as returned to the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeDecision {
    /// The outcome.
    pub outcome: WakeOutcome,
    /// The rule.
    pub rule: WakeRule,
}

/// Decides one wake candidate.
#[derive(Debug)]
pub struct WakeDecider {
    policy: WakePolicy,
    received_at: MonoInstant,
    deadline: MonoInstant,
    verdict: Option<Result<bool, ()>>,
    decision: Option<WakeDecision>,
    trace: WakeTrace,
}

impl WakeDecider {
    /// Takes a candidate that arrived at `now` and decides what can be decided at once: a refusal
    /// at arrival, or acceptance of an idle candidate. A producing-sound candidate waits for its
    /// evidence.
    pub fn arrive(
        candidate: WakeCandidate,
        conditions: ArrivalConditions,
        policy: WakePolicy,
        now: MonoInstant,
    ) -> Self {
        let deadline = now.after(Duration::from_millis(policy.decision_budget_ms));
        let malformed = candidate.malformed().or(if candidate.hops.is_empty() {
            Some("no hops")
        } else {
            None
        });
        let verification_window = if candidate.producing_sound && malformed.is_none() {
            candidate.verification_window(&policy)
        } else {
            None
        };
        let mut d = Self {
            received_at: now,
            deadline,
            verdict: None,
            decision: None,
            trace: WakeTrace {
                candidate,
                verification_window,
                deadline_ms: policy.decision_budget_ms,
                malformed: malformed.map(str::to_owned),
                reference: None,
                echo: None,
                verification: None,
                outcome: None,
                rule: None,
                decided_ms: None,
            },
            policy,
        };
        let refusal = if malformed.is_some() {
            Some(WakeRefusal::Malformed)
        } else if conditions.diagnostic_recording {
            Some(WakeRefusal::DiagnosticRecording)
        } else if conditions.speech_unavailable {
            Some(WakeRefusal::SpeechUnavailable)
        } else if conditions.muted {
            Some(WakeRefusal::Muted)
        } else if conditions.candidate_pending {
            Some(WakeRefusal::CandidatePending)
        } else {
            None
        };
        if let Some(r) = refusal {
            d.decide(WakeOutcome::Refused(r), WakeRule::Arrival, now);
        } else if !d.trace.candidate.producing_sound {
            d.decide(WakeOutcome::Accepted, WakeRule::Idle, now);
        }
        d
    }

    /// The decision, once made.
    pub fn decision(&self) -> Option<WakeDecision> {
        self.decision
    }

    /// The trace.
    pub fn trace(&self) -> &WakeTrace {
        &self.trace
    }

    /// The decision deadline while undecided.
    pub fn next_deadline(&self) -> Option<MonoInstant> {
        self.decision.is_none().then_some(self.deadline)
    }

    fn ms(&self, now: MonoInstant) -> u64 {
        now.since(self.received_at).as_millis() as u64
    }

    fn decide(&mut self, outcome: WakeOutcome, rule: WakeRule, now: MonoInstant) -> WakeDecision {
        let d = WakeDecision { outcome, rule };
        self.decision = Some(d);
        self.trace.outcome = Some(outcome);
        self.trace.rule = Some(rule);
        self.trace.decided_ms = Some(self.ms(now));
        d
    }

    /// Evidence arriving at or after the deadline is late: the deadline is applied first, with
    /// the evidence that was in before it, so a late verdict can never accept.
    fn expire(&mut self, now: MonoInstant) -> Option<WakeDecision> {
        if now >= self.deadline {
            self.evaluate(now, true)
        } else {
            None
        }
    }

    /// Records the reference scorer's evidence.
    pub fn reference_scored(
        &mut self,
        score: ReferenceScore,
        now: MonoInstant,
    ) -> Option<WakeDecision> {
        let expired = self.expire(now);
        if self.trace.reference.is_none() {
            self.trace.reference = Some((score, self.ms(now)));
        }
        expired.or_else(|| self.evaluate(now, false))
    }

    /// Records the echo comparison over the candidate's support.
    pub fn echo_compared(&mut self, echo: CandidateEcho, now: MonoInstant) -> Option<WakeDecision> {
        let expired = self.expire(now);
        if self.trace.echo.is_none() {
            self.trace.echo = Some((echo, self.ms(now)));
        }
        expired.or_else(|| self.evaluate(now, false))
    }

    /// Records the verifier's result. A result after the decision is kept in the trace, marked
    /// late, and changes nothing.
    pub fn verified(&mut self, result: VerifierResult, now: MonoInstant) -> Option<WakeDecision> {
        let expired = self.expire(now);
        if self.trace.verification.is_some() {
            return expired;
        }
        let (text, distance, verdict) = match result {
            VerifierResult::Text(t) => {
                let distance = verification_distance(&t, &self.policy.verify_core);
                let pass = distance.is_some_and(|d| d <= self.policy.verify_pass_max);
                (Some(t), distance, Ok(pass))
            }
            VerifierResult::Error => (None, None, Err(())),
        };
        self.trace.verification = Some(VerificationRecord {
            text,
            distance,
            passes: verdict == Ok(true),
            at_ms: self.ms(now),
            late: self.decision.is_some(),
        });
        if self.decision.is_some() {
            return expired;
        }
        self.verdict = Some(verdict);
        self.evaluate(now, false)
    }

    /// Applies the deadline at `now`.
    pub fn on_timer(&mut self, now: MonoInstant) -> Option<WakeDecision> {
        self.expire(now)
    }

    /// Walks the steps in order. A step whose evidence is missing waits, unless the deadline has
    /// passed.
    fn evaluate(&mut self, now: MonoInstant, deadline: bool) -> Option<WakeDecision> {
        if self.decision.is_some() {
            return None;
        }
        let reference = self.trace.reference.map(|(r, _)| r);
        let echo = self.trace.echo.map(|(e, _)| e);
        // Step 1: did the device itself render something wake-like?
        match reference {
            None if !deadline => return None,
            Some(ReferenceScore::Overlap) => match echo {
                Some(CandidateEcho::NearEndPresent) => {}
                None if !deadline => return None,
                _ => {
                    return Some(self.decide(
                        WakeOutcome::Refused(WakeRefusal::SelfOutput),
                        WakeRule::ReferenceWake,
                        now,
                    ));
                }
            },
            _ => {}
        }
        // Step 2: does echo explain the mic?
        match echo {
            Some(CandidateEcho::EchoOnly) => {
                return Some(self.decide(
                    WakeOutcome::Refused(WakeRefusal::EchoOnly),
                    WakeRule::Echo,
                    now,
                ));
            }
            None if !deadline => return None,
            _ => {}
        }
        // Step 3: did the verifier hear the wake word?
        let (outcome, rule) = match self.verdict {
            Some(Ok(true)) => (WakeOutcome::Accepted, WakeRule::Verified),
            Some(Ok(false)) => (
                WakeOutcome::Refused(WakeRefusal::UnverifiedWake),
                WakeRule::VerifierFailed,
            ),
            Some(Err(())) => (
                WakeOutcome::Refused(WakeRefusal::UnverifiedWake),
                WakeRule::VerifierError,
            ),
            None if !deadline => return None,
            None => (
                WakeOutcome::Refused(WakeRefusal::VerifierTimeout),
                WakeRule::Deadline,
            ),
        };
        Some(self.decide(outcome, rule, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> MonoInstant {
        MonoInstant(n * 1_000_000)
    }

    fn candidate(producing_sound: bool) -> WakeCandidate {
        WakeCandidate {
            candidate_id: "c1".into(),
            profile: if producing_sound {
                WakeProfile::Playback
            } else {
                WakeProfile::Idle
            },
            threshold: 0.65,
            producing_sound,
            first_crossing_end: Sample(30_720),
            support_start: Sample(8_320),
            hops: vec![
                WakeHop {
                    end: Sample(28_160),
                    raw: Some(0.4),
                    smoothed: Some(0.3),
                },
                WakeHop {
                    end: Sample(30_720),
                    raw: Some(0.9),
                    smoothed: Some(0.6),
                },
                WakeHop {
                    end: Sample(33_280),
                    raw: Some(0.5),
                    smoothed: Some(0.66),
                },
            ],
        }
    }

    fn sounding() -> WakeDecider {
        WakeDecider::arrive(
            candidate(true),
            ArrivalConditions::default(),
            WakePolicy::default(),
            ms(0),
        )
    }

    fn refused(r: WakeRefusal) -> WakeOutcome {
        WakeOutcome::Refused(r)
    }

    #[test]
    fn trigger_open_and_window_follow_the_hops() {
        let c = candidate(true);
        assert_eq!(c.open(), Some(Sample(33_280)));
        assert_eq!(c.trigger(), Some(Sample(30_720)));
        assert_eq!(
            c.verification_window(&WakePolicy::default()),
            Some(SampleSpan::new(Sample(3_520), Sample(40_960)))
        );
    }

    #[test]
    fn arrival_refusals_each_have_their_reason_in_order() {
        let cases = [
            (
                ArrivalConditions {
                    diagnostic_recording: true,
                    speech_unavailable: true,
                    muted: true,
                    candidate_pending: true,
                },
                WakeRefusal::DiagnosticRecording,
            ),
            (
                ArrivalConditions {
                    speech_unavailable: true,
                    muted: true,
                    ..Default::default()
                },
                WakeRefusal::SpeechUnavailable,
            ),
            (
                ArrivalConditions {
                    muted: true,
                    candidate_pending: true,
                    ..Default::default()
                },
                WakeRefusal::Muted,
            ),
            (
                ArrivalConditions {
                    candidate_pending: true,
                    ..Default::default()
                },
                WakeRefusal::CandidatePending,
            ),
        ];
        for (conditions, reason) in cases {
            let d = WakeDecider::arrive(candidate(false), conditions, WakePolicy::default(), ms(0));
            assert_eq!(
                d.decision(),
                Some(WakeDecision {
                    outcome: refused(reason),
                    rule: WakeRule::Arrival
                })
            );
        }
        let mut bad = candidate(false);
        bad.hops[1].raw = Some(0.1);
        bad.hops[2].raw = None;
        let d = WakeDecider::arrive(
            bad,
            ArrivalConditions::default(),
            WakePolicy::default(),
            ms(0),
        );
        assert_eq!(
            d.decision().map(|d| d.outcome),
            Some(refused(WakeRefusal::Malformed))
        );
        let mut bad = candidate(false);
        bad.hops.clear();
        let d = WakeDecider::arrive(
            bad,
            ArrivalConditions::default(),
            WakePolicy::default(),
            ms(0),
        );
        assert_eq!(
            d.decision().map(|d| d.outcome),
            Some(refused(WakeRefusal::Malformed))
        );
        let mut bad = candidate(false);
        bad.hops.swap(0, 1);
        let d = WakeDecider::arrive(
            bad,
            ArrivalConditions::default(),
            WakePolicy::default(),
            ms(0),
        );
        assert_eq!(
            d.decision().map(|d| d.outcome),
            Some(refused(WakeRefusal::Malformed))
        );
    }

    #[test]
    fn a_quiet_device_accepts_on_its_score() {
        let d = WakeDecider::arrive(
            candidate(false),
            ArrivalConditions::default(),
            WakePolicy::default(),
            ms(3),
        );
        assert_eq!(
            d.decision(),
            Some(WakeDecision {
                outcome: WakeOutcome::Accepted,
                rule: WakeRule::Idle
            })
        );
        assert_eq!(d.trace().verification_window, None);
        assert_eq!(d.next_deadline(), None);
    }

    #[test]
    fn a_wake_like_render_is_self_output_unless_double_talk() {
        let mut d = sounding();
        assert_eq!(
            d.reference_scored(ReferenceScore::Overlap, ms(100)),
            None,
            "waits for echo"
        );
        let got = d.echo_compared(CandidateEcho::Unknown, ms(150)).unwrap();
        assert_eq!(got.outcome, refused(WakeRefusal::SelfOutput));
        assert_eq!(got.rule, WakeRule::ReferenceWake);

        let mut d = sounding();
        d.reference_scored(ReferenceScore::Overlap, ms(100));
        assert_eq!(
            d.echo_compared(CandidateEcho::NearEndPresent, ms(150)),
            None
        );
        let got = d
            .verified(VerifierResult::Text("Ophelia,".into()), ms(400))
            .unwrap();
        assert_eq!(got.outcome, WakeOutcome::Accepted);
    }

    #[test]
    fn echo_only_refuses() {
        let mut d = sounding();
        d.verified(VerifierResult::Text("ophelia".into()), ms(300));
        d.reference_scored(ReferenceScore::NoOverlap, ms(320));
        let got = d.echo_compared(CandidateEcho::EchoOnly, ms(330)).unwrap();
        assert_eq!(got.outcome, refused(WakeRefusal::EchoOnly));
    }

    #[test]
    fn verification_decides_after_the_attribution_steps() {
        let mut d = sounding();
        d.reference_scored(ReferenceScore::Unavailable, ms(50));
        d.echo_compared(CandidateEcho::Unknown, ms(60));
        let got = d
            .verified(VerifierResult::Text("Eight".into()), ms(500))
            .unwrap();
        assert_eq!(
            (got.outcome, got.rule),
            (
                refused(WakeRefusal::UnverifiedWake),
                WakeRule::VerifierFailed
            )
        );

        let mut d = sounding();
        d.reference_scored(ReferenceScore::NoOverlap, ms(50));
        d.echo_compared(CandidateEcho::NearEndPresent, ms(60));
        let got = d
            .verified(VerifierResult::Text(String::new()), ms(500))
            .unwrap();
        assert_eq!(got.outcome, refused(WakeRefusal::UnverifiedWake));

        let mut d = sounding();
        d.reference_scored(ReferenceScore::NoOverlap, ms(50));
        d.echo_compared(CandidateEcho::Unknown, ms(60));
        let got = d.verified(VerifierResult::Error, ms(500)).unwrap();
        assert_eq!(
            (got.outcome, got.rule),
            (
                refused(WakeRefusal::UnverifiedWake),
                WakeRule::VerifierError
            )
        );

        let mut d = sounding();
        d.reference_scored(ReferenceScore::NoOverlap, ms(50));
        d.echo_compared(CandidateEcho::Unknown, ms(60));
        let got = d
            .verified(VerifierResult::Text("Pophili".into()), ms(500))
            .unwrap();
        assert_eq!(
            (got.outcome, got.rule),
            (WakeOutcome::Accepted, WakeRule::Verified)
        );
        assert_eq!(d.trace().decided_ms, Some(500));
    }

    #[test]
    fn the_budget_deadline_never_accepts_unverified() {
        let mut d = sounding();
        assert_eq!(d.next_deadline(), Some(ms(700)));
        d.reference_scored(ReferenceScore::NoOverlap, ms(100));
        d.echo_compared(CandidateEcho::Unknown, ms(100));
        assert_eq!(d.on_timer(ms(699)), None, "1 ms before the deadline");
        let got = d.on_timer(ms(700)).unwrap();
        assert_eq!(
            (got.outcome, got.rule),
            (refused(WakeRefusal::VerifierTimeout), WakeRule::Deadline)
        );
        // A verdict after the deadline is recorded, marked late, and changes nothing.
        assert_eq!(
            d.verified(VerifierResult::Text("ophelia".into()), ms(720)),
            None
        );
        let v = d.trace().verification.as_ref().unwrap();
        assert!(v.late && v.passes);
        assert_eq!(
            d.trace().outcome,
            Some(refused(WakeRefusal::VerifierTimeout))
        );
    }

    #[test]
    fn missing_attribution_evidence_at_the_deadline_does_not_refuse_alone() {
        // A verified wake whose reference and echo evidence never came is accepted at the
        // deadline: unavailable evidence never rejects by itself.
        let mut d = sounding();
        assert_eq!(
            d.verified(VerifierResult::Text("ophili".into()), ms(450)),
            None
        );
        let got = d.on_timer(ms(700)).unwrap();
        assert_eq!(
            (got.outcome, got.rule),
            (WakeOutcome::Accepted, WakeRule::Verified)
        );
        // An overlapping reference candidate without near-end evidence still refuses.
        let mut d = sounding();
        d.reference_scored(ReferenceScore::Overlap, ms(100));
        d.verified(VerifierResult::Text("ophili".into()), ms(450));
        let got = d.on_timer(ms(700)).unwrap();
        assert_eq!(got.outcome, refused(WakeRefusal::SelfOutput));
    }

    #[test]
    fn evidence_arriving_after_the_deadline_is_late() {
        // The runtime handled the verdict before the deadline timer: the deadline still applies
        // first, so the verdict is late and the candidate is refused.
        let mut d = sounding();
        let got = d
            .verified(VerifierResult::Text("ophelia".into()), ms(800))
            .unwrap();
        assert_eq!(got.outcome, refused(WakeRefusal::VerifierTimeout));
        assert!(d.trace().verification.as_ref().unwrap().late);
        // Evidence after the decision returns nothing new.
        assert_eq!(d.echo_compared(CandidateEcho::Unknown, ms(900)), None);
    }
}
