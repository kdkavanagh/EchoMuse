//! Cell attribution: what produced the sound in each 32 ms cell of an open utterance
//! (§4.6; post-AFE architecture §8.2 and §16.6).
//!
//! Each cell gets exactly one class, first match wins, from the evidence available for that cell
//! alone plus the run state carried from the cells before it:
//!
//! 1. `gap`: the audio is missing or the device was muted;
//! 2. `self_output`: the per-cell echo comparison says the device's own playback explains it;
//! 3. `non_speech`: VAD ≤ 0.35;
//! 4. `background_speech`: VAD ≥ 0.50 with `F` known and `E ≤ F − 10 dB` (speech clearly quieter
//!    than the command), or a reply's speech already under way when its window opened;
//! 5. `command_speech`: inside a command-speech run;
//! 6. `unknown`: anything else.
//!
//! A run opens on a cell with VAD ≥ 0.70 whose level is in the command range (`r ≥ 0.55`, or `r`
//! unavailable) and continues while VAD > 0.35 and the cell is neither `self_output` nor
//! `background_speech`, so a word's softer syllables do not flicker between classes. Unknown
//! evidence never becomes silence: a cell with no VAD result is `unknown`, and a cell with an
//! unknown echo label is judged on VAD and level like one with no reference.

use serde::{Deserialize, Serialize};

use crate::evidence::{CellAudio, CellEvidence, EchoLabel};
use crate::time::{CELL_SAMPLES, Sample, Samples};

/// A cell's class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellClass {
    /// Unknown audio: missing or muted.
    Gap,
    /// The device's own playback explains the sound.
    SelfOutput,
    /// No speech: fan, hum, a music bed, silence.
    NonSpeech,
    /// Speech that is not the command speaker's.
    BackgroundSpeech,
    /// The person issuing the command, or someone indistinguishable from them.
    CommandSpeech,
    /// Ambiguous; bounded rather than guessed.
    Unknown,
}

/// The evidence that decided a cell's class, recorded in the trace so a wrong endpoint can be
/// explained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassRule {
    /// A missing range of audio.
    Gap,
    /// The device reported the cell muted.
    Muted,
    /// The per-cell echo comparison.
    Echo,
    /// The VAD probability.
    Vad,
    /// No VAD result exists for the cell.
    VadUnknown,
    /// Level against `F` (background) or `r` (a run that may not open).
    Level,
    /// A command-speech run continuing through softer cells.
    Hysteresis,
    /// A reply's run already under way when its window opened.
    UnderWay,
    /// A reply's opening guard: no run opens in its first 320 ms.
    Guard,
}

/// The attribution thresholds of policy `post_afe_10` (§16.6).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttributionPolicy {
    /// VAD at or below this is `non_speech`, and a run ends.
    pub non_speech_max: f32,
    /// VAD at or above this is speech-positive.
    pub speech_positive_min: f32,
    /// VAD at or above this may open a run.
    pub run_open_min: f32,
    /// Speech this far or further below `F` is `background_speech`, in dB.
    pub background_below_db: f64,
    /// A run opens only when `r` is at least this (or `r` is unavailable).
    pub run_open_level_min: f64,
    /// The smallest `F − B` span `r` divides by, in dB.
    pub level_span_floor_db: f64,
    /// A reply run whose first cell starts more than this before the trigger is already under
    /// way.
    pub under_way_lead: Samples,
}

impl Default for AttributionPolicy {
    fn default() -> Self {
        Self {
            non_speech_max: 0.35,
            speech_positive_min: 0.50,
            run_open_min: 0.70,
            background_below_db: 10.0,
            run_open_level_min: 0.55,
            level_span_floor_db: 12.0,
            under_way_lead: Samples::from_ms(480),
        }
    }
}

/// The reply-only attribution rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyAttribution {
    /// The opening guard's end: no run opens on a cell after the trigger that starts before it.
    /// `None` for a reply opened at an early answer, which has no guard.
    pub guard_until: Option<Sample>,
}

/// A classified cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Classified {
    /// The class.
    pub class: CellClass,
    /// The rule that decided it.
    pub rule: ClassRule,
    /// Set when this cell opened a command-speech run.
    pub opened: Option<RunOpen>,
}

/// A command-speech run that opened on a cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunOpen {
    /// Where the run starts. Normally the opening cell's start; for a guarded reply whose speech
    /// began inside the guard, where that speech began, but never before the cell holding the
    /// trigger.
    pub start: Sample,
    /// How many cells after the trigger the start was dated back over. They stay `unknown` but
    /// count toward route A's minimum of command speech.
    pub dated_back_cells: u32,
}

#[derive(Clone, Copy, Debug)]
struct Run {
    /// Start of the run's first cell, for the under-way test.
    first_cell: Sample,
}

/// Classifies one utterance's cells in sample order.
#[derive(Debug)]
pub struct CellClassifier {
    policy: AttributionPolicy,
    trigger: Sample,
    reply: Option<ReplyAttribution>,
    run: Option<Run>,
    /// Start of the current stretch of contiguous cells with VAD above the non-speech bar.
    voiced_since: Option<Sample>,
    /// Whether the first cell after the trigger has been seen (the under-way test runs there).
    past_trigger: bool,
    under_way: bool,
}

impl CellClassifier {
    /// A classifier for an utterance triggered at `trigger`; `reply` adds the reply rules.
    pub fn new(
        policy: AttributionPolicy,
        trigger: Sample,
        reply: Option<ReplyAttribution>,
    ) -> Self {
        Self {
            policy,
            trigger,
            reply,
            run: None,
            voiced_since: None,
            past_trigger: false,
            under_way: false,
        }
    }

    fn end_run(&mut self) {
        self.run = None;
        self.under_way = false;
    }

    /// Classifies the next cell with the `B` and `F` in force before it.
    pub fn classify(
        &mut self,
        cell: &CellEvidence,
        background_db: Option<f64>,
        foreground_db: Option<f64>,
    ) -> Classified {
        let plain = |class, rule| Classified {
            class,
            rule,
            opened: None,
        };
        let p = &self.policy;
        let start = cell.cell.start();
        let after_trigger = cell.cell.end() > self.trigger;
        let (energy, vad, echo) = match cell.audio {
            CellAudio::Gap { muted } => {
                self.end_run();
                self.voiced_since = None;
                let rule = if muted {
                    ClassRule::Muted
                } else {
                    ClassRule::Gap
                };
                return plain(CellClass::Gap, rule);
            }
            CellAudio::Present { echo, .. } => (cell.audio.energy_db(), cell.audio.vad(), echo),
        };

        // Track contiguous voiced cells; only VAD decides what counts as one stretch of speech.
        match vad {
            Some(v) if v > p.non_speech_max => {
                self.voiced_since.get_or_insert(start);
            }
            _ => self.voiced_since = None,
        }

        // The under-way test: a reply run open at the trigger that began more than 480 ms
        // before it is speech that was already going on (a TV, the asker), not the answer.
        if after_trigger && !self.past_trigger {
            self.past_trigger = true;
            if self.reply.is_some()
                && let Some(run) = self.run
                && self.trigger.since(run.first_cell) > p.under_way_lead
            {
                self.under_way = true;
            }
        }

        if echo.label == EchoLabel::EchoOnly {
            self.end_run();
            return plain(CellClass::SelfOutput, ClassRule::Echo);
        }
        let Some(v) = vad else {
            self.end_run();
            return plain(CellClass::Unknown, ClassRule::VadUnknown);
        };
        if v <= p.non_speech_max {
            self.end_run();
            return plain(CellClass::NonSpeech, ClassRule::Vad);
        }
        if self.under_way && self.run.is_some() {
            // Under way: background until the run itself closes on a quiet, echo or gap cell.
            return plain(CellClass::BackgroundSpeech, ClassRule::UnderWay);
        }
        if v >= p.speech_positive_min
            && let (Some(e), Some(f)) = (energy, foreground_db)
            && e <= f - p.background_below_db
        {
            self.end_run();
            return plain(CellClass::BackgroundSpeech, ClassRule::Level);
        }
        if self.run.is_some() {
            return plain(CellClass::CommandSpeech, ClassRule::Hysteresis);
        }
        if v < p.run_open_min {
            return plain(CellClass::Unknown, ClassRule::Vad);
        }
        let r = match (energy, background_db, foreground_db) {
            (Some(e), Some(b), Some(f)) => Some((e - b) / (f - b).max(p.level_span_floor_db)),
            _ => None,
        };
        if r.is_some_and(|r| r < p.run_open_level_min) {
            return plain(CellClass::Unknown, ClassRule::Level);
        }
        let guard_until = self.reply.and_then(|r| r.guard_until);
        if let Some(g) = guard_until
            && after_trigger
            && start < g
        {
            return plain(CellClass::Unknown, ClassRule::Guard);
        }
        // Open a run. A guarded reply whose speech began inside the guard is dated back to where
        // that speech began, never before the cell holding the trigger, so an answer begun the
        // instant the window opened keeps its first words.
        let mut run_start = start;
        if let Some(g) = guard_until
            && let Some(since) = self.voiced_since
            && since < g
        {
            run_start = since.max(self.trigger.round_down(CELL_SAMPLES)).min(start);
        }
        self.run = Some(Run { first_cell: start });
        Classified {
            class: CellClass::CommandSpeech,
            rule: ClassRule::Vad,
            opened: Some(RunOpen {
                start: run_start,
                dated_back_cells: (start.since(run_start).0 / CELL_SAMPLES) as u32,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{EchoCell, ReferenceCoverage, VadInput};
    use crate::time::CellIndex;

    fn present(i: u64, e: i16, vad: Option<f32>, echo: EchoLabel) -> CellEvidence {
        CellEvidence {
            cell: CellIndex(i),
            audio: CellAudio::Present {
                energy_centi: Some(e),
                source_mask: Some(0),
                vad: vad.map_or(VadInput::Unknown, |p| VadInput::probability(p).unwrap()),
                echo: EchoCell {
                    label: echo,
                    coverage: ReferenceCoverage::Full,
                },
            },
        }
    }

    fn speech(i: u64, vad: f32) -> CellEvidence {
        present(i, -5000, Some(vad), EchoLabel::NoReference)
    }

    fn classes(c: &mut CellClassifier, cells: &[CellEvidence], f: Option<f64>) -> Vec<CellClass> {
        cells
            .iter()
            .map(|x| c.classify(x, Some(-70.0), f).class)
            .collect()
    }

    use CellClass::*;

    #[test]
    fn first_match_wins_in_spec_order() {
        let mut c = CellClassifier::new(AttributionPolicy::default(), Sample(0), None);
        let gap = CellEvidence {
            cell: CellIndex(0),
            audio: CellAudio::Gap { muted: false },
        };
        let muted = CellEvidence {
            cell: CellIndex(1),
            audio: CellAudio::Gap { muted: true },
        };
        assert_eq!(c.classify(&gap, None, None).rule, ClassRule::Gap);
        assert_eq!(c.classify(&muted, None, None).rule, ClassRule::Muted);
        // Echo outranks a loud VAD.
        let x = c.classify(
            &present(2, -3000, Some(0.99), EchoLabel::EchoOnly),
            None,
            None,
        );
        assert_eq!((x.class, x.rule), (SelfOutput, ClassRule::Echo));
        let x = c.classify(
            &present(3, -3000, Some(0.35), EchoLabel::Unknown),
            None,
            None,
        );
        assert_eq!((x.class, x.rule), (NonSpeech, ClassRule::Vad));
        let x = c.classify(
            &present(4, -3000, Some(0.69), EchoLabel::Unknown),
            None,
            None,
        );
        assert_eq!((x.class, x.rule), (Unknown, ClassRule::Vad));
        let x = c.classify(
            &present(5, -3000, Some(0.70), EchoLabel::Unknown),
            None,
            None,
        );
        assert_eq!((x.class, x.rule), (CommandSpeech, ClassRule::Vad));
        assert!(x.opened.is_some());
    }

    #[test]
    fn missing_vad_is_unknown_and_ends_a_run() {
        let mut c = CellClassifier::new(AttributionPolicy::default(), Sample(0), None);
        assert_eq!(c.classify(&speech(0, 0.9), None, None).class, CommandSpeech);
        let x = c.classify(&present(1, -5000, None, EchoLabel::NoReference), None, None);
        assert_eq!((x.class, x.rule), (Unknown, ClassRule::VadUnknown));
        // The run ended: 0.5 does not continue it.
        assert_eq!(c.classify(&speech(2, 0.5), None, None).class, Unknown);
    }

    #[test]
    fn runs_continue_above_the_non_speech_bar() {
        let mut c = CellClassifier::new(AttributionPolicy::default(), Sample(0), None);
        let cells = [
            speech(0, 0.9),
            speech(1, 0.4),
            speech(2, 0.36),
            speech(3, 0.35),
            speech(4, 0.6),
        ];
        assert_eq!(
            classes(&mut c, &cells, None),
            [
                CommandSpeech,
                CommandSpeech,
                CommandSpeech,
                NonSpeech,
                Unknown
            ]
        );
    }

    #[test]
    fn quieter_speech_is_background_only_once_f_is_known() {
        let mut c = CellClassifier::new(AttributionPolicy::default(), Sample(0), None);
        let quiet = |i| present(i, -6000, Some(0.9), EchoLabel::NoReference);
        // Without F early speech is command speech.
        assert_eq!(
            c.classify(&quiet(0), Some(-75.0), None).class,
            CommandSpeech
        );
        // With F = -50 dB, E = -60 dB is exactly 10 dB below: background, and the run ends.
        let x = c.classify(&quiet(1), Some(-75.0), Some(-50.0));
        assert_eq!((x.class, x.rule), (BackgroundSpeech, ClassRule::Level));
        // E = -59.99 dB is within 10 dB and VAD 0.9 reopens a run (r ≥ 0.55).
        let near = present(2, -5999, Some(0.9), EchoLabel::NoReference);
        assert_eq!(
            c.classify(&near, Some(-75.0), Some(-50.0)).class,
            CommandSpeech
        );
    }

    #[test]
    fn low_level_cannot_open_a_run() {
        let mut c = CellClassifier::new(AttributionPolicy::default(), Sample(0), None);
        // B = -70, F = -50: r = (E - B) / 20. E = -59.2 gives r = 0.54.
        let low = present(0, -5920, Some(0.9), EchoLabel::NoReference);
        let x = c.classify(&low, Some(-70.0), Some(-50.0));
        assert_eq!((x.class, x.rule), (Unknown, ClassRule::Level));
        // r = 0.55 exactly opens.
        let ok = present(1, -5900, Some(0.9), EchoLabel::NoReference);
        assert_eq!(
            c.classify(&ok, Some(-70.0), Some(-50.0)).class,
            CommandSpeech
        );
        // r uses at least a 12 dB span: F - B = 5 dB still divides by 12.
        let mut c = CellClassifier::new(AttributionPolicy::default(), Sample(0), None);
        let x = present(2, -6400, Some(0.9), EchoLabel::NoReference);
        assert_eq!(
            c.classify(&x, Some(-70.0), Some(-65.0)).rule,
            ClassRule::Level
        );
    }

    #[test]
    fn reply_speech_under_way_at_the_trigger_is_background() {
        // Trigger at cell 20; a run starting at cell 4 began 512 ms before it.
        let trigger = CellIndex(20).start();
        let mut c = CellClassifier::new(
            AttributionPolicy::default(),
            trigger,
            Some(ReplyAttribution { guard_until: None }),
        );
        let mut got = Vec::new();
        for i in 4..24 {
            got.push(c.classify(&speech(i, 0.9), None, None));
        }
        assert!(got[..16].iter().all(|x| x.class == CommandSpeech));
        assert!(got[16..].iter().all(|x| x.rule == ClassRule::UnderWay));
        // The run closes on a quiet cell; the next run is the answer.
        assert_eq!(c.classify(&speech(24, 0.1), None, None).class, NonSpeech);
        assert_eq!(
            c.classify(&speech(25, 0.9), None, None).class,
            CommandSpeech
        );

        // A run that began exactly 480 ms before the trigger is the answer over the question's
        // end, not speech under way.
        let mut c = CellClassifier::new(
            AttributionPolicy::default(),
            trigger,
            Some(ReplyAttribution { guard_until: None }),
        );
        for i in 5..22 {
            assert_eq!(c.classify(&speech(i, 0.9), None, None).class, CommandSpeech);
        }
    }

    #[test]
    fn guard_holds_runs_and_dates_back_their_start() {
        // Trigger 100 samples into cell 10 (sample 5220); the guard ends 320 ms later, at 10340.
        let trigger = Sample(CellIndex(10).start().0 + 100);
        let reply = Some(ReplyAttribution {
            guard_until: Some(trigger + Samples::from_ms(320)),
        });
        let mut c = CellClassifier::new(AttributionPolicy::default(), trigger, reply);
        // Speech begins in the cell holding the trigger and keeps going. Cells 10..=20 start
        // before 10340, so none may open a run.
        for i in 10..=20 {
            let x = c.classify(&speech(i, 0.9), None, None);
            assert_eq!((x.class, x.rule), (Unknown, ClassRule::Guard), "cell {i}");
        }
        // Cell 21 starts at 10752 and opens the run, dated back to where the speech began.
        let x = c.classify(&speech(21, 0.9), None, None);
        let o = x.opened.expect("run opened after the guard");
        assert_eq!(o.start, CellIndex(10).start());
        assert_eq!(o.dated_back_cells, 11);

        // Speech that began before the trigger is dated back only to the cell holding it.
        let mut c = CellClassifier::new(AttributionPolicy::default(), trigger, reply);
        assert_eq!(c.classify(&speech(8, 0.5), None, None).class, Unknown);
        assert_eq!(c.classify(&speech(9, 0.5), None, None).class, Unknown);
        for i in 10..=20 {
            assert_eq!(
                c.classify(&speech(i, 0.9), None, None).rule,
                ClassRule::Guard
            );
        }
        let o = c.classify(&speech(21, 0.9), None, None).opened.unwrap();
        assert_eq!(o.start, CellIndex(10).start());

        // A run already open before the trigger (the answer begun over the question's end) is
        // not held by the guard.
        let mut c = CellClassifier::new(AttributionPolicy::default(), trigger, reply);
        assert_eq!(c.classify(&speech(8, 0.9), None, None).class, CommandSpeech);
        for i in 9..=20 {
            assert_eq!(c.classify(&speech(i, 0.9), None, None).class, CommandSpeech);
        }
    }

    #[test]
    fn speech_ending_inside_the_guard_opens_nothing() {
        let trigger = CellIndex(10).start();
        let reply = Some(ReplyAttribution {
            guard_until: Some(trigger + Samples::from_ms(320)),
        });
        let mut c = CellClassifier::new(AttributionPolicy::default(), trigger, reply);
        for i in 10..14 {
            assert_eq!(
                c.classify(&speech(i, 0.9), None, None).rule,
                ClassRule::Guard
            );
        }
        assert_eq!(c.classify(&speech(14, 0.1), None, None).class, NonSpeech);
        // A new stretch after the guard opens normally, at its own start.
        for i in 15..20 {
            assert_eq!(c.classify(&speech(i, 0.1), None, None).class, NonSpeech);
        }
        let x = c.classify(&speech(20, 0.9), None, None);
        assert_eq!(x.opened.map(|o| o.start), Some(CellIndex(20).start()));
        assert_eq!(x.opened.map(|o| o.dated_back_cells), Some(0));
    }
}
