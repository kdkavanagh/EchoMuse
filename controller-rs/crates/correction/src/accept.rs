//! Acceptance: whether Home Assistant's sentence matcher takes a candidate, and whether
//! what it understood keeps what the heard text had already resolved; then the choice
//! among the accepted.

use std::collections::BTreeMap;

use echomuse_core::services::SentenceRecognition;

use crate::{
    propose::{Candidate, Rule},
    record::{Reason, Status},
    tables::Source,
    text::letters_key,
};

/// Two interpretations whose evidence is closer than this are ambiguous.
pub const AMBIGUITY_MARGIN: f64 = 0.05;

/// Slots whose values HA resolves against fixed vocabularies, registries or number
/// ranges (home-assistant-intents 2026.7.30's lists that are not wildcards).
const STRUCTURED_SLOTS: &[&str] = &[
    "name",
    "area",
    "floor",
    "domain",
    "device_class",
    "state",
    "brightness",
    "color",
    "temperature",
    "position",
    "percentage",
    "volume_level",
    "volume_step",
    "media_class",
    "hours",
    "minutes",
    "seconds",
    "start_hours",
    "start_minutes",
    "start_seconds",
];

/// Intents whose `name` slot is a timer's name, a wildcard in the templates.
const TIMER_NAME_INTENTS: &[&str] = &[
    "HassStartTimer",
    "HassCancelTimer",
    "HassIncreaseTimer",
    "HassDecreaseTimer",
    "HassPauseTimer",
    "HassUnpauseTimer",
    "HassTimerStatus",
];

/// `state` values from the templates' closed state lists; any other `state` of
/// `HassGetState` came from the zone wildcard.
const CLOSED_STATES: &[&str] = &[
    "on",
    "off",
    "open",
    "opened",
    "closed",
    "locked",
    "unlocked",
    "opening",
    "closing",
    "occupied",
    "not occupied",
    "detected",
    "clear",
];

/// Whether a slot of `recognition` was filled by free text.
fn free_slot(recognition: &SentenceRecognition, slot: &str, value: &str) -> bool {
    let intent = recognition.intent.as_deref().unwrap_or("");
    if !STRUCTURED_SLOTS.contains(&slot) {
        return true;
    }
    match slot {
        "name" => TIMER_NAME_INTENTS.contains(&intent),
        "state" => {
            intent == "HassGetState" && !CLOSED_STATES.contains(&value.to_lowercase().as_str())
        }
        _ => false,
    }
}

/// Whether HA matched `recognition` only through free text: a trigger template with a list
/// or wildcard, or any filled slot that is free text.
fn matched_by_free_text(recognition: &SentenceRecognition) -> bool {
    if recognition.source.as_deref() == Some("trigger") {
        return recognition.template.contains('{');
    }
    recognition
        .details
        .iter()
        .any(|(slot, value)| free_slot(recognition, slot, &value.value))
}

/// The slots of the heard text's recognition that a correction must keep: those HA
/// resolved from fixed vocabularies, not free text.
fn resolved_slots(original: &SentenceRecognition) -> BTreeMap<&str, &str> {
    original
        .details
        .iter()
        .filter(|(slot, value)| !free_slot(original, slot, &value.value))
        .map(|(slot, value)| (slot.as_str(), value.value.as_str()))
        .collect()
}

/// Whether HA resolved the name a candidate inserted to what it names.
fn resolved_insert(candidate: &Candidate, recognition: &SentenceRecognition) -> bool {
    let Some(inserted) = &candidate.inserted else {
        return true;
    };
    let key = letters_key(inserted);
    let slot_is = |slot: &str| {
        recognition
            .details
            .get(slot)
            .is_some_and(|v| letters_key(&v.value) == key || letters_key(&v.text) == key)
    };
    match candidate.source {
        Source::Entity => slot_is("name") && !recognition.targets.is_empty(),
        Source::Area => slot_is("area"),
        Source::Floor => slot_is("floor"),
        Source::UserTerm => {
            recognition.details.values().any(|v| {
                letters_key(&v.value).contains(&key) || letters_key(&v.text).contains(&key)
            }) || (recognition.source.as_deref() == Some("trigger")
                && letters_key(&recognition.template).contains(&key))
        }
        _ => true,
    }
}

/// A candidate's status from HA's recognition of it and of the heard text, with a detail
/// when it is not `ok`.
///
/// When HA recognized the heard text (fully or not), every slot it had resolved from a
/// fixed vocabulary (a name, area, floor, domain, device class, state or number) must keep
/// its value, and the entities it would target must stay the same. When HA recognized
/// nothing in the heard text, nothing had been resolved, so only the word-level checks made
/// before HA (negations, numbers, units, on/off words, verbs, names) protect the meaning.
pub(crate) fn judge(
    candidate: &Candidate,
    recognition: Option<&SentenceRecognition>,
    original: Option<&SentenceRecognition>,
) -> (Status, Option<String>) {
    let Some(recognition) = recognition.filter(|r| r.matched) else {
        return (Status::NoMatch, None);
    };
    if matched_by_free_text(recognition) && !candidate.whole_text() {
        return (Status::CatchAll, None);
    }
    if candidate.rule == Rule::Name && !resolved_insert(candidate, recognition) {
        return (
            Status::Unresolved,
            Some("the inserted name did not resolve".to_owned()),
        );
    }
    if let Some(original) = original {
        for (slot, value) in resolved_slots(original) {
            let kept = recognition
                .details
                .get(slot)
                .is_some_and(|v| v.value == value);
            if !kept {
                return (Status::ChangedResolved, Some(format!("changes the {slot}")));
            }
        }
        if !original.targets.is_empty() && original.targets != recognition.targets {
            return (
                Status::ChangedResolved,
                Some("changes the target".to_owned()),
            );
        }
    }
    (Status::Ok, None)
}

/// What HA understood a matched candidate as: candidates with one interpretation are
/// spellings of one choice.
fn interpretation(candidate: &Candidate, recognition: &SentenceRecognition) -> String {
    match &recognition.intent {
        Some(intent) => {
            let slots: Vec<String> = recognition
                .details
                .iter()
                .map(|(k, v)| format!("{k}={}", v.value))
                .collect();
            format!(
                "intent:{intent}|{}|{}",
                slots.join(";"),
                recognition.targets.join(",")
            )
        }
        None if recognition.template.contains('{') => {
            format!(
                "trigger:{}|{}",
                recognition.template,
                letters_key(&candidate.text)
            )
        }
        None => format!("trigger:{}", recognition.template),
    }
}

/// A checked candidate.
#[derive(Clone, Debug)]
pub(crate) struct Checked {
    pub candidate: Candidate,
    pub status: Status,
    pub detail: Option<String>,
    pub recognition: Option<SentenceRecognition>,
}

/// The choice among checked candidates.
#[derive(Clone, Debug)]
pub(crate) struct Choice {
    pub reason: Reason,
    /// Index into the checked list of the proposal, if any.
    pub proposed: Option<usize>,
    /// Whether the user's preference picked the proposal's spelling.
    pub preference_changed: bool,
}

/// Chooses among `checked` (in rank order), with `held` the candidates past the
/// validation limit.
///
/// Matched candidates are grouped by interpretation, and groups rank by their best
/// evidence. A group holding an eligible candidate the user's lists prefer (one that is a
/// preferred phrase or contains a preferred term) ranks above every group without one:
/// the household said what it calls things, and that is what "user terms win" means.
/// Preference never lifts a candidate over a threshold or past a meaning check. Within
/// the same standing, a runner-up group
/// (or a held candidate) within [`AMBIGUITY_MARGIN`] of the leader's evidence makes the
/// choice ambiguous. The leader's eligible member of highest rank is the proposal.
pub(crate) fn choose(checked: &[Checked], held: &[Candidate]) -> Choice {
    struct Group {
        best_raw: f64,
        preferred: bool,
        first: usize,
        members: Vec<usize>,
    }
    let mut groups: Vec<(String, Group)> = Vec::new();
    for (i, c) in checked.iter().enumerate() {
        let Some(recognition) = c
            .recognition
            .as_ref()
            .filter(|_| c.status != Status::NoMatch)
        else {
            continue;
        };
        let key = interpretation(&c.candidate, recognition);
        // Preference lifts a group only through an eligible member: an unsafe spelling of
        // the user's term does not push a safe interpretation aside.
        let preferred = c.candidate.preference > 0.0 && c.status == Status::Ok;
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => {
                g.best_raw = g.best_raw.max(c.candidate.raw);
                g.preferred |= preferred;
                g.members.push(i);
            }
            None => groups.push((
                key,
                Group {
                    best_raw: c.candidate.raw,
                    preferred,
                    first: i,
                    members: vec![i],
                },
            )),
        }
    }
    if groups.is_empty() {
        let reason = if checked.is_empty() {
            Reason::NoCandidate
        } else {
            Reason::NoValidCandidate
        };
        return Choice {
            reason,
            proposed: None,
            preference_changed: false,
        };
    }
    groups.sort_by(|(_, a), (_, b)| {
        b.preferred
            .cmp(&a.preferred)
            .then(b.best_raw.total_cmp(&a.best_raw))
            .then(a.first.cmp(&b.first))
    });
    let leader = &groups[0].1;
    // The leader's best member by rank, eligible or not, is what HA accepted best.
    let best_member = |eligible: bool, by_rank: bool| -> Option<usize> {
        leader
            .members
            .iter()
            .copied()
            .filter(|&i| !eligible || checked[i].status == Status::Ok)
            .min_by(|&a, &b| {
                let (x, y) = (&checked[a].candidate, &checked[b].candidate);
                let primary = if by_rank {
                    y.rank.total_cmp(&x.rank)
                } else {
                    y.raw.total_cmp(&x.raw)
                };
                primary.then(a.cmp(&b))
            })
    };
    let close = |raw: f64, preferred: bool| {
        preferred == leader.preferred && raw >= leader.best_raw - AMBIGUITY_MARGIN
    };
    let ambiguous = groups[1..]
        .iter()
        .any(|(_, g)| close(g.best_raw, g.preferred))
        || held.iter().any(|c| close(c.raw, c.preference > 0.0));
    if ambiguous {
        return Choice {
            reason: Reason::Ambiguous,
            proposed: best_member(false, true),
            preference_changed: false,
        };
    }
    match best_member(true, true) {
        Some(selected) => Choice {
            reason: Reason::Applied,
            proposed: Some(selected),
            preference_changed: best_member(true, false) != Some(selected),
        },
        None => Choice {
            reason: Reason::NoValidCandidate,
            proposed: best_member(false, true),
            preference_changed: false,
        },
    }
}
