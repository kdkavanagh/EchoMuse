//! Streaming text as endpoint evidence (post-AFE architecture §16.6 "Text stability" and
//! "Finalize at the pause"; Rust controller evaluation §4.7 for the pause server).
//!
//! The streaming recognizer only appends tokens, at its 1.28 s chunk edges, so its latest result
//! *is* the stable prefix: a repeated result adds no evidence. Two samples follow the prefix:
//! `prefix_sample`, the covered-through sample of the result in which it last changed (none while
//! empty), and `progress_sample`, the same sample whenever it changes (the trigger until the
//! first non-empty prefix).
//!
//! 320 ms into each pause the worker makes a fresh decode of the utterance so far
//! ([`PauseFinalizer`] says when). That finalized result stands in for the live one while the
//! live tokens are a prefix of it; the first live result that adds or changes a token replaces
//! it. While it stands, its trailing blanks are the real audio since the latest speech-positive
//! cell, not the live decoder's count, which stops at its last chunk edge.
//!
//! While a pause decode stands, the pause server's transcript of the same audio may stand in
//! for its words: the judged text is then the server's, with the wake phrase removed as from
//! HA's final transcript near where Kroko's words placed it. Kroko's tokens still time the
//! words. The server's words go when the pause decode does.

use crate::evidence::{AsrKind, AsrObservation, Token};
use crate::pause::TextSource;
use crate::text::{TimedTranscript, cut_streaming, locate_in_streaming, normalize, strip_final};
use crate::time::{CellIndex, Sample, Samples};

/// Samples in one decoder blank frame (40 ms).
const BLANK_FRAME_SAMPLES: u64 = 640;
/// The finalize runs once VAD has analysed this many cells past the last speech-positive one.
const FINALIZE_AFTER_CELLS: u64 = 10;

/// Where to cut a wake turn's streaming text: through the wake phrase found by step 2 among the
/// words emitted by the end of the verification lookahead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WakeCut {
    pub lookahead_end: Sample,
    pub phrase: String,
}

/// How much quiet the decoder evidence shows after the last word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrailingQuiet {
    /// The live decoder's blank frames.
    Blanks(u32),
    /// A finalized result stands: the real audio since the latest speech-positive cell.
    Audio(Samples),
}

impl TrailingQuiet {
    pub fn samples(self) -> Samples {
        match self {
            Self::Blanks(n) => Samples(u64::from(n) * BLANK_FRAME_SAMPLES),
            Self::Audio(s) => s,
        }
    }
}

/// The finalized pause decode while it stands.
#[derive(Debug)]
struct Standing {
    /// The end of the audio it decoded.
    through: Sample,
    tokens: Vec<Token>,
    /// The pause server's transcript of the same audio, once it stands in for these words.
    server: Option<String>,
}

/// Tracks the streaming text of one utterance.
#[derive(Debug)]
pub(crate) struct TextTracker {
    wake: Option<WakeCut>,
    /// The tokens the current text was built from.
    current: Vec<Token>,
    standing: Option<Standing>,
    /// The tokens or the server's words changed since the prefix was last worked out.
    unsettled: bool,
    live_blanks: u32,
    streaming_text: String,
    prefix: String,
    prefix_sample: Option<Sample>,
    progress_sample: Sample,
    trigger: Sample,
    /// When each command word's last token was emitted: words after the wake phrase (for a wake
    /// turn), emitted after the trigger, in order.
    command_words: Vec<Sample>,
}

fn token_texts_prefix(short: &[Token], long: &[Token]) -> bool {
    short.len() <= long.len() && short.iter().zip(long).all(|(a, b)| a.text == b.text)
}

impl TextTracker {
    pub fn new(trigger: Sample, wake: Option<WakeCut>) -> Self {
        Self {
            wake,
            current: Vec::new(),
            standing: None,
            unsettled: false,
            live_blanks: 0,
            streaming_text: String::new(),
            prefix: String::new(),
            prefix_sample: None,
            progress_sample: trigger,
            trigger,
            command_words: Vec::new(),
        }
    }

    /// Applies a block's result and returns whether it ended a standing pause decode (a
    /// live result that adds or changes a token). Call [`TextTracker::settle`] after.
    pub fn take(&mut self, obs: AsrObservation) -> bool {
        match obs.kind {
            AsrKind::Finalized => {
                self.standing = Some(Standing {
                    through: obs.through,
                    tokens: obs.tokens.clone(),
                    server: None,
                });
                self.unsettled = true;
                self.set_tokens(obs.tokens);
                false
            }
            AsrKind::Live => {
                self.live_blanks = obs.trailing_blanks;
                match &self.standing {
                    Some(st) if token_texts_prefix(&obs.tokens, &st.tokens) => false,
                    Some(_) => {
                        self.standing = None;
                        self.unsettled = true;
                        self.set_tokens(obs.tokens);
                        true
                    }
                    None => {
                        self.set_tokens(obs.tokens);
                        false
                    }
                }
            }
        }
    }

    fn set_tokens(&mut self, tokens: Vec<Token>) {
        if tokens != self.current {
            self.current = tokens;
            self.unsettled = true;
        }
    }

    /// The pause server's words (trimmed, not empty) for the pause decode through `pause`
    /// stand in for Kroko's, when that decode still stands. Returns whether they do.
    pub fn stand_server(&mut self, pause: Sample, words: &str) -> bool {
        match self.standing.as_mut() {
            Some(st) if st.through == pause => {
                st.server = Some(words.to_owned());
                self.unsettled = true;
                true
            }
            _ => false,
        }
    }

    /// Works out the prefix after the block covering through `through`; returns whether it
    /// changed.
    pub fn settle(&mut self, through: Sample) -> bool {
        if !std::mem::take(&mut self.unsettled) {
            return false;
        }
        let transcript =
            TimedTranscript::from_tokens(self.current.iter().map(|t| (t.text.as_str(), t.emitted)));
        let server = self.standing.as_ref().and_then(|st| st.server.as_deref());
        let (prefix, first_command_word) = match &self.wake {
            Some(cut) => {
                let found = locate_in_streaming(&transcript, cut.lookahead_end, &cut.phrase);
                let prefix = match server {
                    Some(words) => normalize(strip_final(words, &found, &cut.phrase).text),
                    None => cut_streaming(&transcript, &found),
                };
                (prefix, found.window.map_or(0, |w| w.end))
            }
            None => (normalize(server.unwrap_or(transcript.text())), 0),
        };
        self.command_words.clear();
        self.command_words.extend(
            transcript.last_emitted()[first_command_word..]
                .iter()
                .filter(|&&e| e > self.trigger),
        );
        self.streaming_text = transcript.text().trim().to_owned();
        if prefix == self.prefix {
            return false;
        }
        self.prefix = prefix;
        self.progress_sample = through;
        self.prefix_sample = (!self.prefix.is_empty()).then_some(through);
        true
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn streaming_text(&self) -> &str {
        &self.streaming_text
    }

    /// The judged words as transcribed: the server's while they stand, else Kroko's.
    pub fn heard(&self) -> &str {
        self.standing
            .as_ref()
            .and_then(|st| st.server.as_deref())
            .unwrap_or(&self.streaming_text)
    }

    /// Whose words the prefix was judged on.
    pub fn source(&self) -> TextSource {
        match &self.standing {
            Some(Standing {
                server: Some(_), ..
            }) => TextSource::Server,
            Some(_) => TextSource::Kroko,
            None => TextSource::Streaming,
        }
    }

    pub fn prefix_sample(&self) -> Option<Sample> {
        self.prefix_sample
    }

    pub fn progress_sample(&self) -> Sample {
        self.progress_sample
    }

    /// The emission of the last command word emitted at or before `by`.
    pub fn last_command_word_by(&self, by: Sample) -> Option<Sample> {
        self.command_words.iter().rev().find(|&&e| e <= by).copied()
    }

    pub fn finalized_standing(&self) -> bool {
        self.standing.is_some()
    }

    /// The quiet after the last word at a block covering through `through`.
    pub fn trailing_quiet(
        &self,
        through: Sample,
        last_speech_positive_end: Sample,
    ) -> TrailingQuiet {
        if self.standing.is_some() {
            TrailingQuiet::Audio(through.since(last_speech_positive_end))
        } else {
            TrailingQuiet::Blanks(self.live_blanks)
        }
    }
}

/// Tells the speech worker when to make the pause decode: once VAD has analysed 10 cells
/// (320 ms) past the end of the last speech-positive cell seen since the utterance opened. It
/// fires once per pause; a later speech-positive cell re-arms it.
#[derive(Clone, Debug)]
pub struct PauseFinalizer {
    speech_positive_min: f32,
    last_speech_end: Option<Sample>,
    analysed_through: Option<Sample>,
    armed: bool,
}

impl PauseFinalizer {
    /// A finalizer that counts VAD at or above `speech_positive_min` as speech-positive.
    pub fn new(speech_positive_min: f32) -> Self {
        Self {
            speech_positive_min,
            last_speech_end: None,
            analysed_through: None,
            armed: false,
        }
    }

    /// Records a cell's VAD result; `None` for a gap or an unknown probability.
    pub fn observe_cell(&mut self, cell: CellIndex, vad: Option<f32>) {
        self.analysed_through = Some(cell.end());
        if vad.is_some_and(|p| p >= self.speech_positive_min) {
            self.last_speech_end = Some(cell.end());
            self.armed = true;
        }
    }

    /// Whether the block just analysed should carry a finalized result. True at most once per
    /// pause.
    pub fn due(&mut self) -> bool {
        let (Some(end), Some(through)) = (self.last_speech_end, self.analysed_through) else {
            return false;
        };
        if self.armed && through.since(end).whole_cells() >= FINALIZE_AFTER_CELLS {
            self.armed = false;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl TextTracker {
        /// One block's result, settled.
        fn apply(&mut self, obs: AsrObservation) -> bool {
            let through = obs.through;
            self.take(obs);
            self.settle(through)
        }
    }

    fn ophelia() -> WakeCut {
        WakeCut {
            lookahead_end: Sample(10_000),
            phrase: "ophelia".into(),
        }
    }

    /// The server's words, wake phrase removed near where Kroko's words placed it, stand in
    /// for Kroko's while the pause decode stands.
    #[test]
    fn the_servers_words_stand_in_while_the_pause_decode_stands() {
        let mut t = TextTracker::new(Sample(5_000), Some(ophelia()));
        let kroko = [(" Ophilia", 7_000), (" time", 15_000)];
        assert!(t.apply(obs(20_480, &kroko, 0, AsrKind::Finalized)));
        assert_eq!(t.prefix(), "time");
        assert_eq!(t.source(), TextSource::Kroko);
        // Words for another pause decode do not stand.
        assert!(!t.stand_server(Sample(19_200), "Ophelia, what time is it?"));
        assert!(t.stand_server(Sample(20_480), "Ophelia, what time is it?"));
        assert!(t.settle(Sample(21_760)));
        assert_eq!(t.prefix(), "what time is it");
        assert_eq!(t.prefix_sample(), Some(Sample(21_760)));
        assert_eq!(t.heard(), "Ophelia, what time is it?");
        assert_eq!(t.source(), TextSource::Server);
        // A live result that is still a prefix of the pause decode keeps them standing.
        assert!(!t.apply(obs(23_040, &kroko[..1], 0, AsrKind::Live)));
        assert_eq!(t.prefix(), "what time is it");
        // Kroko's timings still place the last command word.
        assert_eq!(t.last_command_word_by(Sample(30_000)), Some(Sample(15_000)));
        // New speech ends the pause decode, and the server's words with it.
        let more = [(" Ophilia", 7_000), (" time", 15_000), (" now", 22_000)];
        assert!(t.take(obs(24_320, &more, 0, AsrKind::Live)));
        assert!(t.settle(Sample(24_320)));
        assert_eq!(t.prefix(), "time now");
        assert_eq!(t.source(), TextSource::Streaming);
        assert_eq!(t.heard(), "Ophilia time now");
    }

    #[test]
    fn the_next_pause_decode_puts_krokos_words_back() {
        let mut t = TextTracker::new(Sample(0), None);
        let words = [(" turn", 5_000), (" off", 9_000)];
        t.apply(obs(20_480, &words, 0, AsrKind::Finalized));
        assert!(t.stand_server(Sample(20_480), "Turn off."));
        // Same normalized words: no change of the prefix.
        assert!(!t.settle(Sample(21_760)));
        assert_eq!(t.source(), TextSource::Server);
        // A later pause decode with the same tokens is Kroko's again.
        assert!(!t.apply(obs(30_720, &words, 0, AsrKind::Finalized)));
        assert_eq!(t.source(), TextSource::Kroko);
        assert_eq!(t.heard(), "turn off");
    }

    fn obs(through: u64, words: &[(&str, u64)], blanks: u32, kind: AsrKind) -> AsrObservation {
        AsrObservation {
            through: Sample(through),
            tokens: words
                .iter()
                .map(|(t, e)| Token {
                    text: (*t).to_owned(),
                    emitted: Sample(*e),
                })
                .collect(),
            trailing_blanks: blanks,
            kind,
        }
    }

    #[test]
    fn prefix_and_progress_samples_follow_changes() {
        let mut t = TextTracker::new(Sample(100), None);
        assert_eq!(t.progress_sample(), Sample(100));
        assert!(!t.apply(obs(1_000, &[], 3, AsrKind::Live)));
        assert_eq!(t.prefix_sample(), None);
        assert!(t.apply(obs(2_000, &[(" Turn", 1_500)], 0, AsrKind::Live)));
        assert_eq!(t.prefix(), "turn");
        assert_eq!(t.prefix_sample(), Some(Sample(2_000)));
        assert!(!t.apply(obs(3_000, &[(" Turn", 1_500)], 5, AsrKind::Live)));
        assert_eq!(t.progress_sample(), Sample(2_000));
        // Punctuation alone does not change the normalized prefix.
        assert!(!t.apply(obs(
            4_000,
            &[(" Turn", 1_500), (".", 3_900)],
            0,
            AsrKind::Live
        )));
        assert_eq!(t.streaming_text(), "Turn.");
    }

    #[test]
    fn a_wake_turn_judges_only_text_after_the_wake_phrase() {
        let cut = WakeCut {
            lookahead_end: Sample(10_000),
            phrase: "ophelia".into(),
        };
        let mut t = TextTracker::new(Sample(5_000), Some(cut));
        assert!(!t.apply(obs(
            8_000,
            &[(" Oph", 6_000), ("ilia", 7_000)],
            0,
            AsrKind::Live
        )));
        assert_eq!(t.prefix(), "");
        assert_eq!(t.prefix_sample(), None);
        assert!(t.apply(obs(
            20_000,
            &[(" Oph", 6_000), ("ilia", 7_000), (" stop", 15_000)],
            0,
            AsrKind::Live
        )));
        assert_eq!(t.prefix(), "stop");
    }

    #[test]
    fn a_finalized_result_stands_until_live_tokens_diverge() {
        let mut t = TextTracker::new(Sample(0), None);
        t.apply(obs(20_480, &[(" turn", 5_000)], 2, AsrKind::Live));
        assert!(t.apply(obs(
            25_600,
            &[(" turn", 5_000), (" off", 9_000)],
            0,
            AsrKind::Finalized
        )));
        assert!(t.finalized_standing());
        // Real audio since the last speech-positive cell, not the live count.
        assert_eq!(
            t.trailing_quiet(Sample(26_880), Sample(20_000)),
            TrailingQuiet::Audio(Samples(6_880))
        );
        // A live result that is still a prefix keeps it standing.
        assert!(!t.apply(obs(26_880, &[(" turn", 5_000)], 1, AsrKind::Live)));
        assert_eq!(t.prefix(), "turn off");
        // One that matches it fully keeps it standing too.
        assert!(!t.apply(obs(
            28_160,
            &[(" turn", 5_000), (" off", 9_100)],
            1,
            AsrKind::Live
        )));
        assert!(t.finalized_standing());
        // One that adds a token replaces it, and the live blank count is back.
        assert!(t.apply(obs(
            29_440,
            &[(" turn", 5_000), (" off", 9_100), (" the", 28_000)],
            1,
            AsrKind::Live
        )));
        assert!(!t.finalized_standing());
        assert_eq!(
            t.trailing_quiet(Sample(29_440), Sample(20_000)),
            TrailingQuiet::Blanks(1)
        );
        assert_eq!(TrailingQuiet::Blanks(10).samples(), Samples::from_ms(400));
    }

    #[test]
    fn the_finalizer_fires_once_per_pause() {
        let mut f = PauseFinalizer::new(0.5);
        assert!(!f.due());
        f.observe_cell(CellIndex(0), Some(0.9));
        for i in 1..10 {
            f.observe_cell(CellIndex(i), Some(0.1));
            assert!(!f.due(), "cell {i}");
        }
        f.observe_cell(CellIndex(10), None);
        assert!(f.due(), "10 cells past the speech");
        f.observe_cell(CellIndex(11), Some(0.1));
        assert!(!f.due(), "once per pause");
        f.observe_cell(CellIndex(12), Some(0.5));
        for i in 13..22 {
            f.observe_cell(CellIndex(i), Some(0.49));
        }
        assert!(!f.due());
        f.observe_cell(CellIndex(22), Some(0.0));
        assert!(f.due(), "re-armed by later speech");
    }
}
