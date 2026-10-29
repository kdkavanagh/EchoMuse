//! Replays recorded transcripts through the proposer, offline, and prints what it would
//! propose. Home Assistant's matcher is not asked, so this measures proposals and the
//! word-level meaning checks, not acceptance.
//!
//! ```sh
//! cargo run --release -p echomuse-correction --example evaluate -- TURNS.jsonl VOCABULARY.json
//! ```
//!
//! `TURNS.jsonl`: one turn per line, oldest first:
//! `{"text": "...", "wake_phrase": "ophelia" | null, "answered": true | false}`. A turn
//! HA answered joins the remembered requests for the turns after it.
//! `VOCABULARY.json`: `{"names": [...], "areas": [...], "floors": [...]}`.
//! Keep both files outside the repository: they hold real household data.

use std::{collections::BTreeMap, sync::Arc, time::Instant};

use echomuse_correction::{
    KnownPhrase, Lexicon, PhraseSource, Preferences, TableInputs, Tables, propose,
};
use echomuse_grammar::{Grammar, Vocabulary};
use echomuse_settings::settings::TranscriptSettings;
use serde_json::Value;

fn strings(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, turns_path, vocabulary_path] = args.as_slice() else {
        return Err("usage: evaluate TURNS.jsonl VOCABULARY.json".into());
    };
    let vocabulary: Value = serde_json::from_str(&std::fs::read_to_string(vocabulary_path)?)?;
    let vocabulary = Vocabulary {
        names: strings(&vocabulary["names"]),
        areas: strings(&vocabulary["areas"]),
        floors: strings(&vocabulary["floors"]),
    };
    let grammar = Grammar::new()?;
    let lexicon = Arc::new(Lexicon::cmu());
    let settings = TranscriptSettings::default();

    let mut history: Vec<KnownPhrase> = Vec::new();
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut by_rule: BTreeMap<String, usize> = BTreeMap::new();
    let mut slowest = 0f64;
    let mut total_ms = 0f64;
    let mut most_cells = 0u64;
    let mut turns = 0usize;
    for (revision, line) in std::fs::read_to_string(turns_path)?.lines().enumerate() {
        let turn: Value = serde_json::from_str(line)?;
        let Some(text) = turn["text"].as_str() else {
            continue;
        };
        let wake = turn["wake_phrase"].as_str();
        let inputs = TableInputs::new(&grammar, vocabulary.clone(), history.clone());
        let tables = Tables::build(revision as u64, &inputs, lexicon.clone());
        let preferences = Preferences::compile(&tables, &settings);
        let started = Instant::now();
        let proposal = propose(&tables, &preferences, text, wake);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        turns += 1;
        total_ms += ms;
        slowest = slowest.max(ms);
        most_cells = most_cells.max(proposal.work.cells);
        *counts.entry("turns").or_default() += 1;
        if proposal.skipped.is_some() {
            *counts.entry("skipped (too short or long)").or_default() += 1;
        }
        if !proposal.candidates.is_empty() {
            *counts
                .entry("with a meaning-preserving candidate")
                .or_default() += 1;
        }
        if !proposal.rejected.is_empty() {
            *counts
                .entry("with a candidate refused for meaning")
                .or_default() += 1;
        }
        if proposal.truncated {
            *counts.entry("cut short by the work limit").or_default() += 1;
        }
        if let Some(best) = proposal.candidates.first() {
            *by_rule
                .entry(format!("{:?}/{:?}", best.rule, best.source))
                .or_default() += 1;
            println!(
                "{text:?} -> {:?} [{:?} from {:?}, raw {:.2}{}] ({} candidates, {} refused)",
                best.text,
                best.rule,
                best.source,
                best.raw,
                best.coverage
                    .map(|c| format!(", coverage {c:.2}"))
                    .unwrap_or_default(),
                proposal.candidates.len(),
                proposal.rejected.len(),
            );
        }
        for refused in proposal.rejected.iter().take(2) {
            println!(
                "    refused {:?} [{:?}]: {}",
                refused.text,
                refused.rule,
                refused.not_preserved.unwrap_or_default()
            );
        }
        if turn["answered"].as_bool() == Some(true) {
            history.push(KnownPhrase {
                text: text.to_owned(),
                source: PhraseSource::History,
            });
        }
    }
    println!();
    for (k, v) in &counts {
        println!("{k}: {v}");
    }
    println!("best candidate by rule/source: {by_rule:?}");
    println!(
        "proposal time: mean {:.2} ms, slowest {slowest:.2} ms; most edit-distance cells {most_cells}",
        total_ms / turns.max(1) as f64
    );
    Ok(())
}
