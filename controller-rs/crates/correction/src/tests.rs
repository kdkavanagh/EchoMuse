//! Behaviour tests: modes, meaning preservation, preference, table identity and cost.
//! The household, its names and Home Assistant's answers are synthetic.

use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock},
    time::Duration,
};

use echomuse_core::{
    services::{
        DirectIntentRequest, EffectOutcome, HaConnectionGeneration, HaError, HomeAssistant,
        IntentRequest, IntentRun, MediaUrl, NotDispatched, PipelineId, RecognizeRequest,
        SentenceRecognition, SlotValue, SttRequest, TimerStatus, Transcript, TtsRequest,
    },
    types::{DeviceId, RuntimeId, SessionId, WorkIdentity, WorkScope},
};
use echomuse_grammar::{Grammar, Vocabulary};
use echomuse_settings::{
    settings::TranscriptSettings,
    values::{FixEntry, FixList, FixTerms, TranscriptFixMode},
};
use tokio_util::sync::CancellationToken;

use crate::{
    Check, FINAL_DEADLINE, HaContext, KnownPhrase, Lexicon, MAX_WORDS, Outcome, PhraseSource,
    Preferences, Prepared, Reason, Rule, Snapshot, Status, TableInputs, Tables, TurnText,
    WORK_LIMIT, prepare, propose, run,
};

static LEXICON: LazyLock<Arc<Lexicon>> = LazyLock::new(|| Arc::new(Lexicon::cmu()));
static GRAMMAR: LazyLock<Grammar> = LazyLock::new(|| Grammar::new().expect("templates compile"));

fn vocabulary(names: &[&str]) -> Vocabulary {
    Vocabulary {
        names: Some(names.iter().map(|s| s.to_string()).collect()),
        areas: Some(vec![
            "Kitchen".into(),
            "Living Room".into(),
            "Bedroom".into(),
        ]),
        floors: Some(vec!["Upstairs".into()]),
    }
}

fn history(texts: &[&str]) -> Vec<KnownPhrase> {
    texts
        .iter()
        .map(|t| KnownPhrase {
            text: t.to_string(),
            source: PhraseSource::History,
        })
        .collect()
}

fn tables(revision: u64, names: &[&str], phrases: &[&str]) -> Arc<Tables> {
    let inputs = TableInputs::new(&GRAMMAR, vocabulary(names), history(phrases));
    Arc::new(Tables::build(revision, &inputs, LEXICON.clone()))
}

fn settings(mode: TranscriptFixMode, terms: &[&str]) -> TranscriptSettings {
    TranscriptSettings {
        transcript_fix: mode,
        transcript_fix_terms: FixTerms(
            FixList::new(terms.iter().map(|t| FixEntry::new(t).unwrap()).collect()).unwrap(),
        ),
        ..TranscriptSettings::default()
    }
}

fn snapshot(tables: &Arc<Tables>, settings: &TranscriptSettings) -> Snapshot {
    Snapshot {
        tables: tables.clone(),
        preferences: Arc::new(Preferences::compile(tables, settings)),
    }
}

fn ha() -> HaContext {
    HaContext {
        connection: HaConnectionGeneration(1),
        ha_device: None,
        pipeline: Some(PipelineId("pipeline".into())),
        language: Some("en".into()),
    }
}

fn turn(heard: &str) -> TurnText {
    TurnText {
        heard: heard.into(),
        wake_phrase: None,
        contextual: false,
    }
}

fn slots(pairs: &[(&str, &str)]) -> BTreeMap<String, SlotValue> {
    pairs
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                SlotValue {
                    value: v.to_string(),
                    text: v.to_lowercase(),
                },
            )
        })
        .collect()
}

/// A full intent match.
fn intent(name: &str, filled: &[(&str, &str)], targets: &[&str]) -> SentenceRecognition {
    SentenceRecognition {
        matched: true,
        unfilled_slots: false,
        source: Some("builtin".into()),
        template: format!("<{name}>"),
        intent: Some(name.into()),
        details: slots(filled),
        unmatched: BTreeMap::new(),
        targets: targets.iter().map(|s| s.to_string()).collect(),
    }
}

/// An intent HA came closest to without matching.
fn partial(name: &str, filled: &[(&str, &str)], left: &[(&str, &str)]) -> SentenceRecognition {
    SentenceRecognition {
        matched: false,
        unfilled_slots: true,
        source: Some("builtin".into()),
        template: format!("<{name}>"),
        intent: Some(name.into()),
        details: slots(filled),
        unmatched: left
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        targets: Vec::new(),
    }
}

fn key(text: &str) -> String {
    crate::text::letters_key(text)
}

fn check(prepared: Prepared) -> Check {
    match prepared {
        Prepared::Check(check) => *check,
        Prepared::Done(outcome) => panic!("expected a check, got {outcome:?}"),
    }
}

/// Answers the check the way `answer` recognizes each sentence.
fn finish(check: Check, answer: impl Fn(&str) -> Option<SentenceRecognition>) -> Outcome {
    let recognitions = check.sentences().iter().map(|s| answer(s)).collect();
    let current = check.context().clone();
    check.finish(Ok(recognitions), &current, Duration::from_millis(30))
}

/// HA's matcher for the kitchen-light household: "turn on the kitchen light" resolves.
fn kitchen_answer(sentence: &str) -> Option<SentenceRecognition> {
    match key(sentence).as_str() {
        "turnonthekitchenlight" => Some(intent(
            "HassTurnOn",
            &[("name", "Kitchen Light")],
            &["light.kitchen"],
        )),
        "turnonthekitchenlite" => Some(partial("HassTurnOn", &[], &[("name", "kitchen lite")])),
        _ => None,
    }
}

const KITCHEN: &[&str] = &["Kitchen Light", "Apple TV", "Bedroom Lamp"];

#[test]
fn shadow_never_changes_the_text_but_records_the_proposal() {
    let tables = tables(1, KITCHEN, &[]);
    let s = settings(TranscriptFixMode::Shadow, &[]);
    let snap = snapshot(&tables, &s);
    let heard = "turn on the kitchen lite";
    let outcome = finish(
        check(prepare(&s, &turn(heard), Some(&snap), &ha())),
        kitchen_answer,
    );
    assert_eq!(outcome.send, heard);
    assert!(!outcome.applied);
    let record = outcome.record.unwrap();
    assert_eq!(record.reason, Reason::WouldApply);
    assert_eq!(record.sent, heard);
    assert!(!record.applied);
    assert_eq!(
        key(record.proposed.as_deref().unwrap()),
        "turnonthekitchenlight"
    );
    assert_eq!(record.status, Some(Status::Ok));
    let json: serde_json::Value = serde_json::from_str(&record.to_json()).unwrap();
    assert_eq!(json["mode"], "shadow");
    assert_eq!(json["reason"], "would_apply");
    assert_eq!(json["applied"], false);
    assert_eq!(json["verdict"]["original"]["match"], false);
    assert_eq!(json["verdict"]["proposed"]["intent"], "HassTurnOn");
    assert_eq!(json["context"]["tables"], 1);
    assert!(json.get("cache").is_none());
}

#[test]
fn on_sends_a_matcher_accepted_meaning_preserving_correction() {
    let tables = tables(1, KITCHEN, &[]);
    let s = settings(TranscriptFixMode::On, &[]);
    let snap = snapshot(&tables, &s);
    let outcome = finish(
        check(prepare(
            &s,
            &turn("turn on the kitchen lite"),
            Some(&snap),
            &ha(),
        )),
        kitchen_answer,
    );
    assert!(outcome.applied);
    assert_eq!(key(&outcome.send), "turnonthekitchenlight");
    let record = outcome.record.unwrap();
    assert_eq!(record.reason, Reason::Applied);
    assert_eq!(record.sent, outcome.send);
    assert_eq!(record.original, "turn on the kitchen lite");
}

#[test]
fn on_refuses_what_the_matcher_does_not_accept() {
    let tables = tables(1, KITCHEN, &[]);
    let s = settings(TranscriptFixMode::On, &[]);
    let snap = snapshot(&tables, &s);
    let outcome = finish(
        check(prepare(
            &s,
            &turn("turn on the kitchen lite"),
            Some(&snap),
            &ha(),
        )),
        |_| None,
    );
    assert!(!outcome.applied);
    assert_eq!(outcome.send, "turn on the kitchen lite");
    assert_eq!(outcome.record.unwrap().reason, Reason::NoValidCandidate);
}

#[test]
fn text_home_assistant_understands_is_never_changed() {
    let tables = tables(1, KITCHEN, &[]);
    let s = settings(TranscriptFixMode::On, &[]);
    let snap = snapshot(&tables, &s);
    let outcome = finish(
        check(prepare(
            &s,
            &turn("turn on the kitchen lite"),
            Some(&snap),
            &ha(),
        )),
        |s| Some(intent("HassTurnOn", &[("name", s)], &["light.x"])),
    );
    assert!(!outcome.applied);
    let record = outcome.record.unwrap();
    assert_eq!(record.reason, Reason::OriginalMatched);
    assert_eq!(record.candidates, Some(Vec::new()));
}

/// A candidate refused before HA and never sent to the matcher.
fn assert_refused(names: &[&str], phrases: &[&str], heard: &str, expected: &str) {
    let tables = tables(1, names, phrases);
    let s = settings(TranscriptFixMode::On, &[]);
    let snap = snapshot(&tables, &s);
    let check = check(prepare(&s, &turn(heard), Some(&snap), &ha()));
    assert!(check.sentences().iter().all(|s| key(s) != key(expected)));
    // Even if HA would match it, it is not sent.
    let outcome = finish(check, |s| {
        (key(s) == key(expected)).then(|| intent("X", &[], &[]))
    });
    assert!(!outcome.applied);
    assert_eq!(outcome.send, heard);
    let record = outcome.record.unwrap();
    let listed = record.candidates.unwrap();
    let refused = listed
        .iter()
        .find(|c| key(&c.text) == key(expected))
        .unwrap_or_else(|| panic!("{expected:?} not recorded: {listed:?}"));
    assert_eq!(refused.status, Status::NotPreserved);
}

#[test]
fn a_negation_flip_is_refused() {
    assert_refused(
        &[],
        &["never turn on the heater"],
        "ever turn on the heater",
        "never turn on the heater",
    );
}

#[test]
fn a_number_change_is_refused() {
    assert_refused(
        &[],
        &["set a timer for fifty minutes"],
        "set a timer for fifteen minutes",
        "set a timer for fifty minutes",
    );
}

#[test]
fn a_unit_change_is_refused() {
    assert_refused(
        &[],
        &["add two pints of milk"],
        "add two points of milk",
        "add two pints of milk",
    );
}

/// "turn on the bedroom lump": HA resolved the bedroom area for the heard text.
fn bedroom_answer(original_resolved: bool) -> impl Fn(&str) -> Option<SentenceRecognition> {
    move |sentence| match key(sentence).as_str() {
        "turnonthebedroomlump" => original_resolved
            .then(|| partial("HassTurnOn", &[("area", "Bedroom")], &[("name", "lump")])),
        // HA's answer for the candidate puts the lamp in another area.
        "turnonthebedroomlamp" => Some(intent(
            "HassTurnOn",
            &[("name", "Bedroom Lamp"), ("area", "Kitchen")],
            &["light.lamp"],
        )),
        _ => None,
    }
}

#[test]
fn a_target_change_is_refused_when_the_original_resolved() {
    let tables = tables(1, KITCHEN, &[]);
    let s = settings(TranscriptFixMode::On, &[]);
    let snap = snapshot(&tables, &s);
    let heard = "turn on the bedroom lump";
    let outcome = finish(
        check(prepare(&s, &turn(heard), Some(&snap), &ha())),
        bedroom_answer(true),
    );
    assert!(!outcome.applied);
    assert_eq!(outcome.send, heard);
    let record = outcome.record.unwrap();
    assert_eq!(record.reason, Reason::NoValidCandidate);
    assert_eq!(record.status, Some(Status::ChangedResolved));
    let first = &record.candidates.unwrap()[0];
    assert_eq!(first.detail.as_deref(), Some("changes the area"));

    // With nothing resolved for the heard text there is no target to keep: the word
    // checks alone protect the meaning, and the correction applies.
    let outcome = finish(
        check(prepare(&s, &turn(heard), Some(&snap), &ha())),
        bedroom_answer(false),
    );
    assert!(outcome.applied);
    assert_eq!(key(&outcome.send), "turnonthebedroomlamp");
}

/// HA resolves both "watch Flecks" and "watch Plex", as different media players.
fn watch_answer(sentence: &str) -> Option<SentenceRecognition> {
    match key(sentence).as_str() {
        "watchflecks" => Some(intent(
            "HassMediaUnpause",
            &[("name", "Flecks")],
            &["media_player.flecks"],
        )),
        "watchplex" => Some(intent(
            "HassMediaUnpause",
            &[("name", "Plex")],
            &["media_player.plex"],
        )),
        _ => None,
    }
}

#[test]
fn user_terms_win() {
    let tables = tables(1, &["Flecks"], &[]);
    // Without a preferred term the name that sounds most alike wins.
    let plain = settings(TranscriptFixMode::On, &[]);
    let outcome = finish(
        check(prepare(
            &plain,
            &turn("watch flex"),
            Some(&snapshot(&tables, &plain)),
            &ha(),
        )),
        watch_answer,
    );
    assert_eq!(key(&outcome.send), "watchflecks");
    // The household's term wins over it.
    let preferred = settings(TranscriptFixMode::On, &["Plex"]);
    let outcome = finish(
        check(prepare(
            &preferred,
            &turn("watch flex"),
            Some(&snapshot(&tables, &preferred)),
            &ha(),
        )),
        watch_answer,
    );
    assert!(outcome.applied);
    assert_eq!(outcome.send, "watch Plex");
    let record = outcome.record.unwrap();
    assert_eq!(record.source, Some(crate::Source::UserTerm));
    assert!(record.preference.unwrap() > 0.0);
}

#[test]
fn a_table_identity_mismatch_is_refused() {
    let old = tables(1, KITCHEN, &[]);
    let new = tables(2, KITCHEN, &[]);
    let s = settings(TranscriptFixMode::On, &[]);
    // Preferences compiled against the old tables, used with the new ones.
    let mixed = Snapshot {
        tables: new.clone(),
        preferences: Arc::new(Preferences::compile(&old, &s)),
    };
    let Prepared::Done(outcome) =
        prepare(&s, &turn("turn on the kitchen lite"), Some(&mixed), &ha())
    else {
        panic!("a mismatched snapshot must not be searched");
    };
    assert!(!outcome.applied);
    let record = outcome.record.unwrap();
    assert_eq!(record.reason, Reason::NotReady);
    assert_eq!(
        record.error.as_deref(),
        Some("preferences were compiled for other tables")
    );

    // A result computed on tables that were replaced while HA answered is stale.
    let check = check(prepare(
        &s,
        &turn("turn on the kitchen lite"),
        Some(&snapshot(&old, &s)),
        &ha(),
    ));
    let mut current = check.context().clone();
    current.tables = new.id().clone();
    let answers = check
        .sentences()
        .iter()
        .map(|s| kitchen_answer(s))
        .collect();
    let outcome = check.finish(Ok(answers), &current, Duration::from_millis(20));
    assert!(!outcome.applied);
    assert_eq!(outcome.record.unwrap().reason, Reason::Stale);

    // So are settings changed since the preferences were compiled.
    let changed = settings(TranscriptFixMode::On, &["Plex"]);
    let Prepared::Done(outcome) = prepare(
        &changed,
        &turn("turn on the kitchen lite"),
        Some(&snapshot(&old, &s)),
        &ha(),
    ) else {
        panic!("preferences for other settings must not be used");
    };
    assert_eq!(outcome.record.unwrap().reason, Reason::NotReady);
}

#[test]
fn off_and_follow_ups_are_not_corrected() {
    let tables = tables(1, KITCHEN, &[]);
    let off = settings(TranscriptFixMode::Off, &[]);
    let Prepared::Done(outcome) = prepare(
        &off,
        &turn("turn on the kitchen lite"),
        Some(&snapshot(&tables, &off)),
        &ha(),
    ) else {
        panic!("off computes nothing");
    };
    assert_eq!(outcome.record, None);
    assert_eq!(outcome.send, "turn on the kitchen lite");

    let on = settings(TranscriptFixMode::On, &[]);
    let mut follow_up = turn("turn on the kitchen lite");
    follow_up.contextual = true;
    let Prepared::Done(outcome) = prepare(&on, &follow_up, Some(&snapshot(&tables, &on)), &ha())
    else {
        panic!("a follow-up is bypassed");
    };
    assert_eq!(outcome.record.unwrap().reason, Reason::Contextual);

    let mut unknown_language = ha();
    unknown_language.language = None;
    let Prepared::Done(outcome) = prepare(
        &on,
        &turn("turn on the kitchen lite"),
        Some(&snapshot(&tables, &on)),
        &unknown_language,
    ) else {
        panic!("no language, no correction");
    };
    assert_eq!(outcome.record.unwrap().reason, Reason::NotReady);
}

#[test]
fn proposals_from_each_rule() {
    let tables = tables(
        1,
        KITCHEN,
        &["play cherub rock", "what time is it", "close the blinds"],
    );
    let s = settings(TranscriptFixMode::Shadow, &[]);
    let snap = snapshot(&tables, &s);
    let texts = |heard: &str, wake: Option<&str>| -> Vec<(Rule, String)> {
        propose(&snap.tables, &snap.preferences, heard, wake)
            .candidates
            .into_iter()
            .map(|c| (c.rule, c.text))
            .collect()
    };
    assert!(texts("plate cherub rock", None).contains(&(Rule::Phrase, "play cherub rock".into())));
    assert!(texts("what time is", None).contains(&(Rule::Cutoff, "what time is it".into())));
    assert!(
        texts("set a twenty minute timer", None)
            .contains(&(Rule::Numbers, "set a 20 minute timer".into()))
    );
    assert!(
        texts("lia what's the weather", Some("ophelia"))
            .contains(&(Rule::LeadIn, "what's the weather".into()))
    );
    assert!(
        texts("turn on the kitchen lite", None)
            .iter()
            .any(|(r, t)| *r == Rule::Name && key(t) == "turnonthekitchenlight")
    );
    // Ordinary words that sound like template words are not replaced unless the grammar
    // then reads a whole request.
    assert!(
        texts("is orange juice good for you", None)
            .iter()
            .all(|(r, _)| *r != Rule::Word)
    );
    // Too short to propose for.
    assert_eq!(
        propose(&snap.tables, &snap.preferences, "st", None).skipped,
        Some(crate::Skipped::TooShort)
    );
}

#[test]
fn cost_stays_bounded_on_long_inputs() {
    // A large household: thousands of names and remembered requests built from real
    // dictionary words.
    let words = [
        "kitchen", "bedroom", "garage", "porch", "lamp", "light", "fan", "speaker", "heater",
        "window", "door", "blind", "office", "patio", "garden", "hall", "stair", "closet",
        "basement", "attic", "desk", "floor", "ceiling", "table", "strip", "plug", "switch",
        "outlet", "sensor", "camera",
    ];
    let mut names = Vec::new();
    for a in words {
        for b in words {
            for c in ["one", "two", "left", "right"] {
                names.push(format!("{a} {b} {c}"));
            }
        }
    }
    let mut phrases = Vec::new();
    for a in words {
        for b in words {
            phrases.push(format!("turn on the {a} {b} please"));
            phrases.push(format!("what is the {a} {b} doing"));
        }
    }
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let phrases: Vec<&str> = phrases.iter().map(String::as_str).collect();
    let tables = tables(1, &names, &phrases);
    let s = settings(TranscriptFixMode::On, &[]);
    let snap = snapshot(&tables, &s);
    let long: Vec<&str> = words.iter().copied().cycle().take(MAX_WORDS).collect();
    let heard = long.join(" ");
    let proposal = propose(&snap.tables, &snap.preferences, &heard, Some("hey ophelia"));
    // At most one comparison (96 × 96 segments) past the limit.
    assert!(
        proposal.work.cells <= WORK_LIMIT.cells + 96 * 96,
        "{:?}",
        proposal.work
    );
    assert!(proposal.work.rows <= WORK_LIMIT.rows, "{:?}", proposal.work);
    assert!(proposal.candidates.len() <= crate::propose::MAX_CANDIDATES);

    // Longer than a command: nothing is searched.
    let longer = format!("{heard} again");
    let proposal = propose(&snap.tables, &snap.preferences, &longer, None);
    assert_eq!(proposal.skipped, Some(crate::Skipped::TooLong));
    assert_eq!(proposal.work, crate::Work::default());
}

/// Home Assistant with a matcher that answers after `delay`; nothing else is available.
struct FakeHa {
    delay: Duration,
}

impl HomeAssistant for FakeHa {
    async fn pipeline_for(
        &self,
        _scope: WorkScope,
        _ha_device: Option<echomuse_core::services::HaDeviceId>,
    ) -> Result<PipelineId, HaError> {
        Err(HaError::NotDispatched(NotDispatched::Unavailable))
    }

    async fn run_stt(&self, _request: SttRequest) -> Result<Transcript, HaError> {
        Err(HaError::NotDispatched(NotDispatched::Unavailable))
    }

    async fn run_intent_tts(&self, _request: IntentRequest) -> Result<IntentRun, NotDispatched> {
        Err(NotDispatched::Unavailable)
    }

    async fn run_tts(&self, _request: TtsRequest) -> Result<MediaUrl, HaError> {
        Err(HaError::NotDispatched(NotDispatched::Unavailable))
    }

    async fn recognize(
        &self,
        request: RecognizeRequest,
    ) -> Result<Vec<Option<SentenceRecognition>>, HaError> {
        tokio::time::sleep(self.delay).await;
        Ok(request
            .sentences
            .iter()
            .map(|s| kitchen_answer(s))
            .collect())
    }

    async fn timer_status(
        &self,
        _scope: WorkScope,
        _ha_device: Option<echomuse_core::services::HaDeviceId>,
    ) -> Result<TimerStatus, HaError> {
        Err(HaError::NotDispatched(NotDispatched::Unavailable))
    }

    async fn call_intent(&self, _request: DirectIntentRequest) -> EffectOutcome<()> {
        EffectOutcome::NotDispatched(NotDispatched::Unavailable)
    }
}

fn scope() -> WorkScope {
    WorkScope::new(
        WorkIdentity {
            device: DeviceId("dot".into()),
            runtime: RuntimeId(1),
            session: SessionId(1),
            turn: None,
        },
        CancellationToken::new(),
    )
}

#[tokio::test(start_paused = true)]
async fn on_mode_waits_at_most_the_deadline() {
    let tables = tables(1, KITCHEN, &[]);
    let s = settings(TranscriptFixMode::On, &[]);
    let snap = snapshot(&tables, &s);
    let heard = "turn on the kitchen lite";

    let fast = FakeHa {
        delay: Duration::from_millis(40),
    };
    let started = tokio::time::Instant::now();
    let pending = check(prepare(&s, &turn(heard), Some(&snap), &ha()));
    assert!(pending.can_change_text());
    let context = pending.context().clone();
    let outcome = run(&fast, pending, scope(), started, || context).await;
    assert!(outcome.applied);
    assert_eq!(outcome.record.unwrap().deadline_met, Some(true));

    let slow = FakeHa {
        delay: Duration::from_millis(500),
    };
    let started = tokio::time::Instant::now();
    let pending = check(prepare(&s, &turn(heard), Some(&snap), &ha()));
    let context = pending.context().clone();
    let outcome = run(&slow, pending, scope(), started, || context).await;
    assert_eq!(started.elapsed(), FINAL_DEADLINE);
    assert!(!outcome.applied);
    assert_eq!(outcome.send, heard);
    let record = outcome.record.unwrap();
    assert_eq!(record.reason, Reason::Timeout);
    assert_eq!(record.deadline_met, Some(false));
}

#[tokio::test(start_paused = true)]
async fn shadow_waits_beside_the_turn_and_reports_the_deadline() {
    let tables = tables(1, KITCHEN, &[]);
    let s = settings(TranscriptFixMode::Shadow, &[]);
    let snap = snapshot(&tables, &s);
    let slow = FakeHa {
        delay: Duration::from_millis(500),
    };
    let started = tokio::time::Instant::now();
    let check = check(prepare(
        &s,
        &turn("turn on the kitchen lite"),
        Some(&snap),
        &ha(),
    ));
    assert!(!check.can_change_text());
    let context = check.context().clone();
    let outcome = run(&slow, check, scope(), started, || context).await;
    assert!(!outcome.applied);
    let record = outcome.record.unwrap();
    assert_eq!(record.reason, Reason::WouldApply);
    assert_eq!(record.deadline_met, Some(false));
}

#[tokio::test]
async fn the_builder_publishes_tables_and_compiles_preferences() {
    let (builder, mut published) = crate::TableBuilder::spawn_with(|| {
        Lexicon::parse("kitchen K IH1 CH AH0 N\nlight L AY1 T\n")
    })
    .unwrap();
    assert!(published.borrow().is_none());
    let revision = builder.rebuild(TableInputs::new(
        &GRAMMAR,
        vocabulary(&["Kitchen Light"]),
        Vec::new(),
    ));
    published.changed().await.unwrap();
    let tables = published.borrow().clone().unwrap();
    assert_eq!(tables.id().revision, revision);
    let s = settings(TranscriptFixMode::On, &["Plex"]);
    let prefs = builder
        .compile_preferences(tables.clone(), s)
        .await
        .unwrap();
    assert_eq!(prefs.tables(), tables.id());
    builder.withdraw();
    assert!(published.borrow().is_none());
    builder.shutdown().unwrap();
}
