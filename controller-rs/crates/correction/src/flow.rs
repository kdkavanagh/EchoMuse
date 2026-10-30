//! A turn's correction from transcript to the text it sends: a synchronous, bounded
//! preparation, one asynchronous check with Home Assistant's sentence matcher, and the
//! decision with its record.

use std::{sync::Arc, time::Duration};

use echomuse_core::{
    services::{
        HaConnectionGeneration, HaDeviceId, HaError, HomeAssistant, NotDispatched, PipelineId,
        RecognizeRequest, SentenceRecognition,
    },
    types::WorkScope,
};
use echomuse_settings::{settings::TranscriptSettings, values::TranscriptFixMode};
use tokio::time::Instant;

use crate::{
    accept::{Checked, choose, judge},
    propose::{Candidate, Proposal, order, propose},
    record::{
        CandidateRecord, ContextRecord, FixRecord, MAX_RECORDED_CANDIDATES, Reason, RecordMode,
        Status, Verdict, Verdicts, WorkRecord,
    },
    tables::{ConfigDigest, Preferences, Tables, TablesId},
};

/// In `on` mode, the most a correction adds to a turn, counted from the final transcript.
pub const FINAL_DEADLINE: Duration = Duration::from_millis(100);
/// In `shadow` mode, the longest the check may run beside the turn.
pub const SHADOW_LIMIT: Duration = Duration::from_secs(10);
/// The most candidates sent to Home Assistant's matcher with the heard text.
pub const VALIDATE_LIMIT: usize = 8;

/// The published tables and one speaker's preferences compiled against them.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// The current tables.
    pub tables: Arc<Tables>,
    /// The speaker's preferences.
    pub preferences: Arc<Preferences>,
}

/// The Home Assistant side of a turn's context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HaContext {
    /// The connection the matcher is asked on.
    pub connection: HaConnectionGeneration,
    /// The speaker's HA device, when HA knows it.
    pub ha_device: Option<HaDeviceId>,
    /// The pipeline the satellite's select chooses.
    pub pipeline: Option<PipelineId>,
    /// That pipeline's effective language; `None` when it could not be read, and then
    /// nothing is corrected: a match in another language proves nothing.
    pub language: Option<String>,
}

/// Everything a result depends on. A result is used only while all of it still holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixContext {
    /// The HA connection.
    pub connection: HaConnectionGeneration,
    /// The tables searched.
    pub tables: TablesId,
    /// The speaker's transcript settings.
    pub config: ConfigDigest,
    /// The speaker's HA device.
    pub ha_device: Option<HaDeviceId>,
    /// The pipeline.
    pub pipeline: Option<PipelineId>,
    /// The language the matcher is asked in.
    pub language: String,
    /// The wake phrase whose leftovers may be removed, on a wake turn.
    pub wake_phrase: Option<String>,
}

impl FixContext {
    fn record(&self) -> ContextRecord {
        ContextRecord {
            connection: self.connection.0,
            tables: self.tables.revision,
            tables_digest: self.tables.digest.clone(),
            config: self.config.as_str().to_owned(),
            pipeline: self.pipeline.as_ref().map(|p| p.0.clone()),
            language: self.language.clone(),
        }
    }
}

/// The transcript a turn would send.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnText {
    /// The transcript as heard, wake phrase removed.
    pub heard: String,
    /// On a wake turn, the wake phrase: its leftovers at the start may be removed.
    pub wake_phrase: Option<String>,
    /// A follow-up in a conversation (a reply turn, an inherited expectation, a
    /// conversation ID): never corrected, since HA's matcher has no conversation context.
    pub contextual: bool,
}

/// What a turn sends and what it records.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    /// The text to send to Home Assistant.
    pub send: String,
    /// Whether that is a correction.
    pub applied: bool,
    /// The record for `turns.stt_fix`; `None` when correction is off.
    pub record: Option<FixRecord>,
}

impl Outcome {
    fn heard(heard: &str, record: Option<FixRecord>) -> Self {
        Self {
            send: heard.to_owned(),
            applied: false,
            record,
        }
    }
}

/// The result of [`prepare`].
#[derive(Debug)]
pub enum Prepared {
    /// Decided without Home Assistant: off, a follow-up, or not ready.
    Done(Box<Outcome>),
    /// Ask Home Assistant's matcher: [`run`].
    Check(Box<Check>),
}

fn mode(settings: &TranscriptSettings) -> Option<RecordMode> {
    match settings.transcript_fix {
        TranscriptFixMode::Off => None,
        TranscriptFixMode::Shadow => Some(RecordMode::Shadow),
        TranscriptFixMode::On => Some(RecordMode::On),
    }
}

/// Prepares a turn's correction under the speaker's current `settings`: synchronous and
/// bounded by the proposer's work limit.
///
/// Decided at once: `off` (the heard text, no record); a follow-up (`contextual`); no
/// published tables, preferences compiled for other tables or other settings, or an
/// unknown language (`not_ready`). Otherwise the proposal is made and the matcher must be
/// asked about the heard text and the candidates ([`Check`]).
pub fn prepare(
    settings: &TranscriptSettings,
    turn: &TurnText,
    snapshot: Option<&Snapshot>,
    ha: &HaContext,
) -> Prepared {
    let Some(mode) = mode(settings) else {
        return Prepared::Done(Box::new(Outcome::heard(&turn.heard, None)));
    };
    let not_ready = |error: &str| {
        let mut record = FixRecord::bare(mode, Reason::NotReady, &turn.heard);
        record.error = Some(error.to_owned());
        Prepared::Done(Box::new(Outcome::heard(&turn.heard, Some(record))))
    };
    if turn.contextual {
        let record = FixRecord::bare(mode, Reason::Contextual, &turn.heard);
        return Prepared::Done(Box::new(Outcome::heard(&turn.heard, Some(record))));
    }
    let Some(snapshot) = snapshot else {
        return not_ready("tables not ready");
    };
    if snapshot.preferences.tables() != snapshot.tables.id() {
        return not_ready("preferences were compiled for other tables");
    }
    let config = ConfigDigest::of(settings);
    if *snapshot.preferences.config() != config {
        return not_ready("preferences were compiled for other settings");
    }
    let Some(language) = ha.language.clone() else {
        return not_ready("the pipeline's language is not known");
    };
    let started = std::time::Instant::now();
    let proposal = propose(
        &snapshot.tables,
        &snapshot.preferences,
        &turn.heard,
        turn.wake_phrase.as_deref(),
    );
    let compute = started.elapsed();
    Prepared::Check(Box::new(Check {
        mode,
        heard: turn.heard.clone(),
        context: FixContext {
            connection: ha.connection,
            tables: snapshot.tables.id().clone(),
            config,
            ha_device: ha.ha_device.clone(),
            pipeline: ha.pipeline.clone(),
            language,
            wake_phrase: turn.wake_phrase.clone(),
        },
        proposal,
        compute,
    }))
}

/// A prepared correction waiting for Home Assistant's matcher.
#[derive(Debug)]
pub struct Check {
    mode: RecordMode,
    heard: String,
    context: FixContext,
    proposal: Proposal,
    compute: Duration,
}

impl Check {
    /// The context it was prepared in.
    pub fn context(&self) -> &FixContext {
        &self.context
    }

    /// The longest the check may take: [`FINAL_DEADLINE`] when the turn waits for it
    /// ([`Check::can_change_text`]), otherwise [`SHADOW_LIMIT`].
    fn limit(&self) -> Duration {
        if self.can_change_text() {
            FINAL_DEADLINE
        } else {
            SHADOW_LIMIT
        }
    }

    /// Whether the matcher's answer can change the text sent: false when there is no
    /// candidate, or in shadow mode. Then the turn may send the heard text at once and
    /// finish the check beside it, only for the record.
    pub fn can_change_text(&self) -> bool {
        self.mode == RecordMode::On && !self.proposal.candidates.is_empty()
    }

    fn validated(&self) -> &[Candidate] {
        let n = self.proposal.candidates.len().min(VALIDATE_LIMIT);
        &self.proposal.candidates[..n]
    }

    /// The sentences to ask about: the heard text first, then the candidates to validate.
    pub(crate) fn sentences(&self) -> Vec<String> {
        std::iter::once(self.heard.clone())
            .chain(self.validated().iter().map(|c| c.text.clone()))
            .collect()
    }

    /// The matcher request: one call for every sentence, for the speaker's HA device and in
    /// the pipeline's language.
    fn request(&self, scope: WorkScope) -> RecognizeRequest {
        RecognizeRequest {
            scope,
            ha_device: self.context.ha_device.clone(),
            sentences: self.sentences(),
            language: Some(self.context.language.clone()),
        }
    }

    fn base_record(&self, reason: Reason, latency: Duration) -> FixRecord {
        let mut record = FixRecord::bare(self.mode, reason, &self.heard);
        record.context = Some(self.context.record());
        record.compute_ms = Some(self.compute.as_millis() as u64);
        record.latency_ms = Some(latency.as_millis() as u64);
        record.deadline_met = Some(latency <= FINAL_DEADLINE);
        record.work = Some(WorkRecord::new(self.proposal.work, self.proposal.truncated));
        record.skipped = self.proposal.skipped;
        record
    }

    /// The check did not finish within its limit: the heard text is sent.
    fn timed_out(self, latency: Duration) -> Outcome {
        let mut record = self.base_record(Reason::Timeout, latency);
        record.deadline_met = Some(false);
        Outcome::heard(&self.heard, Some(record))
    }

    /// Decides from the matcher's answer, `latency` after the final transcript, given the
    /// context `current` at that moment.
    ///
    /// The heard text is sent unless HA's matcher did not understand it, a candidate kept
    /// its meaning and passed, no other interpretation came close, the context still holds,
    /// and, in `on` mode, the answer came within [`FINAL_DEADLINE`]. In shadow mode the
    /// heard text is always sent and the record says what `on` would have done.
    pub(crate) fn finish(
        self,
        result: Result<Vec<Option<SentenceRecognition>>, HaError>,
        current: &FixContext,
        latency: Duration,
    ) -> Outcome {
        if *current != self.context {
            let record = self.base_record(Reason::Stale, latency);
            return Outcome::heard(&self.heard, Some(record));
        }
        let recognitions = match result {
            Ok(r) if r.len() == self.validated().len() + 1 => r,
            Ok(r) => {
                let mut record = self.base_record(Reason::Error, latency);
                record.error = Some(format!(
                    "the matcher answered {} sentences of {}",
                    r.len(),
                    self.validated().len() + 1
                ));
                return Outcome::heard(&self.heard, Some(record));
            }
            Err(error) => {
                let reason = match error {
                    HaError::NotDispatched(NotDispatched::Unavailable) => Reason::NotReady,
                    _ => Reason::Error,
                };
                let mut record = self.base_record(reason, latency);
                record.error = Some(error.to_string());
                return Outcome::heard(&self.heard, Some(record));
            }
        };
        if self.can_change_text() && latency > FINAL_DEADLINE {
            return self.timed_out(latency);
        }
        let mut recognitions = recognitions.into_iter();
        let original = recognitions.next().flatten();
        let original_verdict = original.as_ref().map(Verdict::of);
        if original.as_ref().is_some_and(|r| r.matched) {
            // Text HA already understands is never changed.
            let mut record = self.base_record(Reason::OriginalMatched, latency);
            record.verdict = Some(Verdicts {
                original: original_verdict,
                proposed: None,
            });
            record.candidates = Some(Vec::new());
            return Outcome::heard(&self.heard, Some(record));
        }
        let checked: Vec<Checked> = self
            .validated()
            .iter()
            .cloned()
            .zip(recognitions)
            .map(|(candidate, recognition)| {
                let (status, detail) = judge(&candidate, recognition.as_ref(), original.as_ref());
                Checked {
                    candidate,
                    status,
                    detail,
                    recognition,
                }
            })
            .collect();
        let held = &self.proposal.candidates[self.validated().len()..];
        let choice = choose(&checked, held);
        let mut reason = choice.reason;
        let mut record_candidates = Vec::new();
        let mut sorted: Vec<&Checked> = checked.iter().collect();
        sorted.sort_by(|a, b| order(&a.candidate, &b.candidate));
        for c in sorted {
            record_candidates.push(candidate_record(
                &c.candidate,
                c.status,
                c.detail.clone(),
                c.recognition.as_ref(),
            ));
        }
        for c in self.proposal.rejected.iter() {
            record_candidates.push(candidate_record(
                c,
                Status::NotPreserved,
                c.not_preserved.map(str::to_owned),
                None,
            ));
        }
        for c in held {
            record_candidates.push(candidate_record(c, Status::NotValidated, None, None));
        }
        record_candidates.truncate(MAX_RECORDED_CANDIDATES);

        let proposed = choice.proposed.map(|i| &checked[i]);
        let apply = reason == Reason::Applied && self.mode == RecordMode::On;
        if reason == Reason::Applied && self.mode == RecordMode::Shadow {
            reason = Reason::WouldApply;
        }
        if reason == Reason::NoCandidate && !self.proposal.rejected.is_empty() {
            // Candidates existed but none kept what was said.
            reason = Reason::NoValidCandidate;
        }
        let mut record = self.base_record(reason, latency);
        record.verdict = Some(Verdicts {
            original: original_verdict,
            proposed: proposed.and_then(|p| p.recognition.as_ref().map(Verdict::of)),
        });
        record.candidates = Some(record_candidates);
        if let Some(p) = proposed {
            record.proposed = Some(p.candidate.text.clone());
            record.rule = Some(p.candidate.rule);
            record.source = Some(p.candidate.source);
            record.raw = Some(p.candidate.raw);
            record.coverage = Some(p.candidate.coverage);
            record.preference = Some(p.candidate.preference);
            record.rank = Some(p.candidate.rank);
            record.status = Some(p.status);
            record.preference_changed = Some(choice.preference_changed);
        } else {
            record.preference_changed = Some(false);
        }
        if apply {
            let text = proposed
                .map(|p| p.candidate.text.clone())
                .unwrap_or_default();
            record.sent = text.clone();
            record.applied = true;
            return Outcome {
                send: text,
                applied: true,
                record: Some(record),
            };
        }
        Outcome::heard(&self.heard, Some(record))
    }
}

fn candidate_record(
    candidate: &Candidate,
    status: Status,
    detail: Option<String>,
    recognition: Option<&SentenceRecognition>,
) -> CandidateRecord {
    let matched = recognition.filter(|r| r.matched);
    CandidateRecord {
        text: candidate.text.clone(),
        rule: candidate.rule,
        source: candidate.source,
        raw: candidate.raw,
        coverage: candidate.coverage,
        preference: candidate.preference,
        rank: candidate.rank,
        status,
        detail,
        matched: matched.map(|r| r.template.clone()),
        intent: matched.and_then(|r| r.intent.clone()),
    }
}

/// Runs a prepared check against Home Assistant: one matcher call, cut off at
/// `started + check.limit()`, where `started` is when the final transcript arrived. The
/// context is read again with `current` when the answer comes, so a result computed under
/// tables, settings or a connection that changed meanwhile is recorded `stale` and not used.
pub async fn run<H: HomeAssistant>(
    ha: &H,
    check: Check,
    scope: WorkScope,
    started: Instant,
    current: impl FnOnce() -> FixContext,
) -> Outcome {
    let deadline = started + check.limit();
    let request = check.request(scope);
    match tokio::time::timeout_at(deadline, ha.recognize(request)).await {
        Ok(result) => {
            let latency = started.elapsed();
            check.finish(result, &current(), latency)
        }
        Err(_) => {
            let latency = started.elapsed();
            check.timed_out(latency)
        }
    }
}
