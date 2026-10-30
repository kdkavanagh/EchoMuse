//! Home Assistant's English intent data, vendored from home-assistant-intents 2026.7.30 (the
//! release HA 2026.8.1 pins; Apache-2.0, see `data/SOURCE`).
//!
//! The file holds every intent's sentence templates, the lists and expansion rules they use,
//! and the skip words. The lists `name`, `area` and `floor` are not in it: HA fills them from
//! its registries, so they are vocabulary lists here ([`crate::Vocabulary`]).

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::program::{CompileError, Group, ListDef, Origin, TemplateSet, ValueDef};

/// The vendored `en.json`.
const EN_JSON: &str = include_str!("../data/home-assistant-intents/en.json");

/// Timer requests the deployed controller's timer family judges for completeness although
/// Home Assistant's templates lack them, compiled with HA's own rules and lists: `(intent,
/// template)`. They are judged like HA's timer templates; nothing here routes them.
const TIMER_FORMS: &[(&str, &str)] = &[
    (
        "HassStartTimer",
        "<timer_set> [a|an|the|my] {timer_name:name} timer for {timer_hours:hours}( |-)hour[s]",
    ),
    (
        "HassStartTimer",
        "<timer_set> [a|an|the|my] {timer_name:name} timer for {timer_minutes:minutes}( |-)minute[s]",
    ),
    (
        "HassStartTimer",
        "<timer_set> [a|an|the|my] {timer_name:name} timer for {timer_seconds:seconds}( |-)second[s]",
    ),
    ("HassTimerStatus", "how much time [is] left"),
];

#[derive(Deserialize)]
struct IntentFile {
    language: String,
    #[serde(default)]
    skip_words: Vec<String>,
    intents: BTreeMap<String, IntentDef>,
    #[serde(default)]
    lists: BTreeMap<String, ListJson>,
    #[serde(default)]
    expansion_rules: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct IntentDef {
    data: Vec<DataBlock>,
}

#[derive(Deserialize)]
struct DataBlock {
    sentences: Vec<String>,
}

#[derive(Deserialize)]
struct ListJson {
    values: Option<Vec<ValueJson>>,
    range: Option<RangeJson>,
    #[serde(default)]
    wildcard: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ValueJson {
    Plain(String),
    Mapped {
        #[serde(rename = "in")]
        input: String,
        out: Option<serde_json::Value>,
    },
}

#[derive(Deserialize)]
struct RangeJson {
    from: i64,
    to: i64,
    #[serde(default = "one")]
    step: i64,
    fractions: Option<String>,
}

fn one() -> i64 {
    1
}

/// Reads the intent data into a template set.
pub(crate) fn home_assistant() -> Result<TemplateSet, CompileError> {
    let file: IntentFile =
        serde_json::from_str(EN_JSON).map_err(|err| CompileError::Data(err.to_string()))?;
    if file.language != "en" {
        return Err(CompileError::Data(format!(
            "language {:?}, expected \"en\"",
            file.language
        )));
    }
    let mut lists = BTreeMap::new();
    for (name, list) in file.lists {
        let def = if let Some(values) = list.values {
            ListDef::Values(
                values
                    .into_iter()
                    .map(|value| match value {
                        ValueJson::Plain(text) => ValueDef {
                            out: Some(text.clone()),
                            template: text,
                        },
                        ValueJson::Mapped { input, out } => ValueDef {
                            template: input,
                            out: out.map(|v| match v {
                                serde_json::Value::String(s) => s,
                                other => other.to_string(),
                            }),
                        },
                    })
                    .collect(),
            )
        } else if let Some(range) = list.range {
            if range.step <= 0 || range.from > range.to {
                return Err(CompileError::Data(format!("list {name}: bad range")));
            }
            ListDef::Range {
                from: range.from,
                to: range.to,
                step: range.step,
                halves: range.fractions.as_deref() == Some("halves"),
            }
        } else if list.wildcard {
            ListDef::Wildcard
        } else {
            return Err(CompileError::Data(format!("list {name}: unknown kind")));
        };
        lists.insert(name, def);
    }
    let mut sentences = Vec::new();
    for (intent, def) in file.intents {
        let group = if intent.contains("Timer") {
            Group::HaTimers
        } else {
            Group::Ha
        };
        let intent: std::sync::Arc<str> = std::sync::Arc::from(intent.as_str());
        for block in def.data {
            for sentence in block.sentences {
                sentences.push((
                    sentence,
                    Origin::Ha {
                        intent: intent.clone(),
                    },
                    group,
                ));
            }
        }
    }
    for (intent, template) in TIMER_FORMS {
        sentences.push((
            (*template).to_owned(),
            Origin::Ha {
                intent: std::sync::Arc::from(*intent),
            },
            Group::HaTimers,
        ));
    }
    Ok(TemplateSet {
        rules: file.expansion_rules,
        lists,
        sentences,
        skip_words: file.skip_words,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vendored_data_reads() {
        let set = home_assistant().unwrap();
        assert!(set.sentences.len() > 800, "{}", set.sentences.len());
        assert!(set.rules.contains_key("timer_cancel"));
        assert!(matches!(
            set.lists.get("timer_name"),
            Some(ListDef::Wildcard)
        ));
        assert!(matches!(
            set.lists.get("temperature"),
            Some(ListDef::Range { halves: true, .. })
        ));
        assert!(set.skip_words.iter().any(|w| w == "please"));
        // The timer intents, and only they, are the timer family.
        for (_, origin, group) in &set.sentences {
            let Origin::Ha { intent } = origin else {
                panic!("{origin:?}");
            };
            let timer = [
                "HassStartTimer",
                "HassCancelTimer",
                "HassCancelAllTimers",
                "HassIncreaseTimer",
                "HassDecreaseTimer",
                "HassPauseTimer",
                "HassUnpauseTimer",
                "HassTimerStatus",
            ]
            .contains(&&**intent);
            assert_eq!(*group == Group::HaTimers, timer, "{intent}");
        }
    }
}
