//! The reference scoring worker's state: per subscription, the reference wake scorer on the
//! device's wake graph (spec §5.1).
//!
//! The device's own output is scored with the same graph its microphone wake runs, on the
//! reference's hop grid, so a wake candidate the device itself caused can be recognised
//! (`echomuse_echo::ReferenceWakeScorer`). A judgement request waits until the windows it
//! reads are scored, or until the reference ends.
//!
//! **No usable graph.** When the device's graph cannot be used (the job names none, or it
//! fails to load), no window is ever scored. Every judgement is then answered at once as
//! [`ReferenceScore::Unavailable`] with reason [`ReferenceUnavailable::InvalidWindow`],
//! the echo crate's reason for "a window the candidate needs failed to score", which is
//! exactly what happened; a load failure is also reported in `failure`. Unavailable never
//! refuses a wake on its own.

use std::collections::HashMap;
use std::sync::Arc;

use echomuse_core::types::SessionId;
use echomuse_echo::{
    Alignment, EchoPolicy, ReferenceJudgement, ReferenceUnavailable, ReferenceWakePolicy,
    ReferenceWakeScorer, WakeWindowScorer,
};
use echomuse_speech::{SpeechError, WindowScore};
use echomuse_turn::{ReferenceScore, SampleSpan};

use super::models::{BoxedWakeScorer, ModelSet};
use super::{ReferenceJob, ReferenceOutput, StreamKey};

/// A boxed wake scorer as the echo crate's scorer type.
struct Scorer(BoxedWakeScorer);

impl WakeWindowScorer for Scorer {
    type Error = SpeechError;

    fn score_window(&mut self, pcm: &[i16]) -> Result<WindowScore, SpeechError> {
        self.0.score_window(pcm)
    }
}

struct ReferenceStream {
    /// `None` when the device's graph cannot be used.
    scorer: Option<ReferenceWakeScorer<Scorer>>,
    pending: Vec<(String, SampleSpan, Alignment)>,
}

/// The reference scoring worker's state for every subscription it scores.
pub struct ReferenceScoring {
    models: Arc<dyn ModelSet>,
    streams: HashMap<StreamKey, ReferenceStream>,
}

/// The judgement for a candidate when no window can be scored.
fn unscorable() -> ReferenceJudgement {
    ReferenceJudgement {
        score: ReferenceScore::Unavailable,
        unavailable: Some(ReferenceUnavailable::InvalidWindow),
        candidate: None,
        windows: None,
    }
}

impl ReferenceScoring {
    pub fn new(models: Arc<dyn ModelSet>) -> Self {
        Self {
            models,
            streams: HashMap::new(),
        }
    }

    /// Applies one job and returns what it produced.
    pub fn run(&mut self, job: ReferenceJob) -> ReferenceOutput {
        let mut failures = Vec::new();
        let key = match job {
            ReferenceJob::Reset => {
                self.streams.clear();
                return output(None, Vec::new(), failures);
            }
            ReferenceJob::Close { key } => {
                self.streams.remove(&key);
                return output(Some(key), Vec::new(), failures);
            }
            ReferenceJob::Open { key, graph } => {
                self.forget_other_sessions(&key.session);
                let scorer = match graph {
                    None => None,
                    Some(files) => match self.models.wake_scorer(&files) {
                        Ok(s) => Some(ReferenceWakeScorer::new(
                            Scorer(s),
                            ReferenceWakePolicy::default(),
                        )),
                        Err(e) => {
                            failures.push(format!("the wake graph could not be loaded: {e}"));
                            None
                        }
                    },
                };
                self.streams.insert(
                    key.clone(),
                    ReferenceStream {
                        scorer,
                        pending: Vec::new(),
                    },
                );
                return output(Some(key), Vec::new(), failures);
            }
            ReferenceJob::Pcm { ref key, .. }
            | ReferenceJob::Silence { ref key, .. }
            | ReferenceJob::Missing { ref key, .. }
            | ReferenceJob::Ended { ref key }
            | ReferenceJob::Judge { ref key, .. } => key.clone(),
        };
        let Some(stream) = self.streams.get_mut(&key) else {
            // A job for a subscription that is not open is stale.
            return output(Some(key), Vec::new(), failures);
        };
        match (job, &mut stream.scorer) {
            (
                ReferenceJob::Judge {
                    candidate,
                    support,
                    alignment,
                    ..
                },
                _,
            ) => stream.pending.push((candidate, support, alignment)),
            (
                ReferenceJob::Pcm {
                    reference_epoch,
                    first,
                    pcm,
                    ..
                },
                Some(s),
            ) => s.push_pcm(reference_epoch, first, pcm.samples()),
            (
                ReferenceJob::Silence {
                    reference_epoch,
                    first,
                    frames,
                    ..
                },
                Some(s),
            ) => s.push_silence(reference_epoch, first, frames),
            (
                ReferenceJob::Missing {
                    reference_epoch,
                    first,
                    end,
                    ..
                },
                Some(s),
            ) => s.push_missing(reference_epoch, first, end),
            (ReferenceJob::Ended { .. }, Some(s)) => s.reference_ended(),
            _ => {}
        }
        if let Some(s) = &mut stream.scorer {
            advance(s, &mut failures);
        }

        let mut judgements = Vec::new();
        let scorer = &stream.scorer;
        let echo = EchoPolicy::default();
        stream.pending.retain(|(candidate, support, alignment)| {
            let judgement = match scorer {
                None => Some(unscorable()),
                Some(s) => s.judge(*support, alignment, &echo),
            };
            match judgement {
                Some(j) => {
                    judgements.push((candidate.clone(), j));
                    false
                }
                None => true,
            }
        });
        output(Some(key), judgements, failures)
    }

    fn forget_other_sessions(&mut self, session: &SessionId) {
        self.streams.retain(|k, _| &k.session == session);
    }
}

/// Scores every hop whose window has arrived. A failed inference marks its hop invalid
/// (judgements reading it are unavailable) and scoring continues with the next hop.
fn advance(scorer: &mut ReferenceWakeScorer<Scorer>, failures: &mut Vec<String>) {
    let mut failed = 0usize;
    let mut first = None;
    while let Err(e) = scorer.advance() {
        failed += 1;
        first.get_or_insert_with(|| e.to_string());
    }
    if let Some(e) = first {
        failures.push(if failed == 1 {
            format!("the wake graph failed on the reference: {e}")
        } else {
            format!("the wake graph failed on {failed} reference windows, first: {e}")
        });
    }
}

fn output(
    key: Option<StreamKey>,
    judgements: Vec<(String, ReferenceJudgement)>,
    failures: Vec<String>,
) -> ReferenceOutput {
    ReferenceOutput {
        key,
        judgements,
        failure: (!failures.is_empty()).then(|| failures.join("; ")),
    }
}

#[cfg(test)]
mod tests {
    use echomuse_echo::ClockFit;
    use echomuse_speech::{BcresnetModelFiles, PinnedFile, Sha256Digest};
    use echomuse_turn::{Epoch, Sample};
    use echomuse_uplink::SubscriptionId;

    use super::*;
    use crate::speech::testing::{ToneModels, pcm16, silence, tone};

    fn r_epoch() -> Epoch {
        Epoch::new(7).unwrap()
    }

    fn key(session: u64) -> StreamKey {
        StreamKey {
            session: SessionId(session),
            subscription: SubscriptionId::generate(),
            epoch: Epoch::new(1).unwrap(),
        }
    }

    fn graph() -> BcresnetModelFiles {
        BcresnetModelFiles {
            graph: PinnedFile::new("graph.onnx", Sha256Digest::of_bytes(b"graph")),
            sidecar: PinnedFile::new("graph.json", Sha256Digest::of_bytes(b"sidecar")),
        }
    }

    /// Microphone sample `m` and reference sample `m` play at the same device time.
    fn alignment() -> Alignment {
        let fit = ClockFit {
            ns_ref: 0,
            position_at_ref: 0.0,
            samples_per_ns: 1.0 / 62_500.0,
        };
        Alignment {
            mic_epoch: Epoch::new(1).unwrap(),
            reference_epoch: r_epoch(),
            mic: fit,
            reference: fit,
            lag: None,
        }
    }

    /// 100,000 reference samples with the wake word's tone at 60,000..69,600.
    fn reference() -> Vec<i16> {
        let mut pcm = silence(3_750);
        pcm.extend(tone(0, 600, 8_000));
        pcm.extend(silence(1_900));
        pcm
    }

    fn judge(k: &StreamKey, candidate: &str) -> ReferenceJob {
        ReferenceJob::Judge {
            key: k.clone(),
            candidate: candidate.into(),
            support: SampleSpan::new(Sample(60_000), Sample(70_000)),
            alignment: alignment(),
        }
    }

    fn pcm(k: &StreamKey, first: u64, samples: &[i16]) -> ReferenceJob {
        ReferenceJob::Pcm {
            key: k.clone(),
            reference_epoch: r_epoch(),
            first,
            pcm: pcm16(samples),
        }
    }

    fn open(
        w: &mut ReferenceScoring,
        k: &StreamKey,
        graph: Option<BcresnetModelFiles>,
    ) -> ReferenceOutput {
        w.run(ReferenceJob::Open {
            key: k.clone(),
            graph,
        })
    }

    #[test]
    fn a_judgement_waits_for_its_windows_then_finds_the_overlap() {
        let mut w = ReferenceScoring::new(Arc::new(ToneModels::new(&["ophelia", "turn"])));
        let k = key(1);
        assert!(open(&mut w, &k, Some(graph())).failure.is_none());
        let r = reference();
        w.run(pcm(&k, 0, &r[..50_000]));
        let asked = w.run(judge(&k, "c1"));
        assert!(
            asked.judgements.is_empty(),
            "its windows are not scored yet"
        );
        let out = w.run(pcm(&k, 50_000, &r[50_000..]));
        assert_eq!(out.judgements.len(), 1);
        let (candidate, j) = &out.judgements[0];
        assert_eq!(candidate, "c1");
        assert_eq!(j.score, ReferenceScore::Overlap);
        assert!(j.unavailable.is_none());
    }

    #[test]
    fn silent_reference_is_no_overlap() {
        let mut w = ReferenceScoring::new(Arc::new(ToneModels::new(&["ophelia"])));
        let k = key(1);
        open(&mut w, &k, Some(graph()));
        w.run(pcm(&k, 0, &silence(6_250)));
        let out = w.run(judge(&k, "c1"));
        assert_eq!(out.judgements[0].1.score, ReferenceScore::NoOverlap);
    }

    #[test]
    fn a_reference_that_ends_answers_a_waiting_judgement() {
        let mut w = ReferenceScoring::new(Arc::new(ToneModels::new(&["ophelia"])));
        let k = key(1);
        open(&mut w, &k, Some(graph()));
        w.run(pcm(&k, 0, &silence(3_000)));
        assert!(w.run(judge(&k, "c1")).judgements.is_empty());
        let out = w.run(ReferenceJob::Ended { key: k.clone() });
        let j = out.judgements[0].1;
        assert_eq!(j.score, ReferenceScore::Unavailable);
        assert_eq!(j.unavailable, Some(ReferenceUnavailable::Ended));
    }

    #[test]
    fn without_a_usable_graph_every_judgement_is_unavailable_at_once() {
        let mut w = ReferenceScoring::new(Arc::new(ToneModels::new(&["ophelia"])));
        let k = key(1);
        assert!(open(&mut w, &k, None).failure.is_none());
        let out = w.run(judge(&k, "c1"));
        assert_eq!(out.judgements[0].1, unscorable());

        let mut w =
            ReferenceScoring::new(Arc::new(ToneModels::new(&["ophelia"]).failing_wake_graph()));
        let opened = open(&mut w, &k, Some(graph()));
        assert!(opened.failure.unwrap().contains("could not be loaded"));
        w.run(pcm(&k, 0, &reference()));
        let out = w.run(judge(&k, "c2"));
        assert_eq!(out.judgements[0].1.score, ReferenceScore::Unavailable);
        assert_eq!(
            out.judgements[0].1.unavailable,
            Some(ReferenceUnavailable::InvalidWindow)
        );
    }

    #[test]
    fn reset_and_newer_sessions_forget_streams() {
        let mut w = ReferenceScoring::new(Arc::new(ToneModels::new(&["ophelia"])));
        let old = key(1);
        open(&mut w, &old, Some(graph()));
        open(&mut w, &key(2), Some(graph()));
        let stale = w.run(judge(&old, "c1"));
        assert_eq!(stale.key, Some(old));
        assert!(stale.judgements.is_empty());

        let k = key(2);
        open(&mut w, &k, None);
        assert!(w.run(ReferenceJob::Reset).key.is_none());
        assert!(w.run(judge(&k, "c1")).judgements.is_empty());
    }
}
