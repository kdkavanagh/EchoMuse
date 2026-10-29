//! The wake verification worker: a fresh Kroko decode of one candidate's verification
//! window (spec §4.1, §5.1), on the evidence copy followed by 1.5 s of zeros like every
//! fresh decode. The wake decision compares the text with the wake phrase; a failed decode
//! is [`VerifierResult::Error`], which refuses the candidate (it is never accepted
//! unverified).

use std::sync::Arc;

use echomuse_turn::VerifierResult;

use super::models::{ModelSet, evidence_copy, fresh_decode};
use super::{VerifyJob, VerifyOutput};

/// The wake verification worker's state: only its models; every decode starts fresh.
pub struct WakeVerification {
    models: Arc<dyn ModelSet>,
}

impl WakeVerification {
    pub fn new(models: Arc<dyn ModelSet>) -> Self {
        Self { models }
    }

    /// Decodes the job's window.
    pub fn run(&mut self, job: VerifyJob) -> VerifyOutput {
        let result = match fresh_decode(&*self.models, &evidence_copy(&job.pcm)) {
            Ok(r) => VerifierResult::Text(r.text),
            Err(error) => {
                tracing::warn!(candidate = %job.candidate, %error, "wake verification decode failed");
                VerifierResult::Error
            }
        };
        VerifyOutput {
            candidate: job.candidate,
            result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech::testing::{ToneModels, silence, utterance};

    const WORDS: [&str; 3] = ["ophelia", "turn", "on"];

    fn window(m: &ToneModels) -> Arc<[i16]> {
        let mut pcm = silence(300);
        pcm.extend(utterance(m, &["ophelia", "turn"], 300, 120, 1_000));
        pcm.into()
    }

    #[test]
    fn the_window_is_decoded_fresh_with_its_trailing_word() {
        let m = ToneModels::new(&WORDS);
        let mut w = WakeVerification::new(Arc::new(m.clone()));
        // "turn" ends 120 ms before the window does: only the flush brings it out.
        let out = w.run(VerifyJob {
            candidate: "c1".into(),
            pcm: window(&m),
        });
        assert_eq!(out.candidate, "c1");
        assert_eq!(out.result, VerifierResult::Text("ophelia turn".into()));
    }

    #[test]
    fn a_failed_decode_is_a_verifier_error() {
        let m = ToneModels::new(&WORDS).failing_asr();
        let mut w = WakeVerification::new(Arc::new(m.clone()));
        let out = w.run(VerifyJob {
            candidate: "c1".into(),
            pcm: window(&m),
        });
        assert_eq!(out.result, VerifierResult::Error);
    }
}
