//! Classifies transcripts read from standard input, one per line, and prints each with its
//! completeness and route, tab-separated. For evaluating the grammar on recorded turns
//! without putting them in the repository:
//!
//! ```sh
//! sqlite3 copy.db "select stt_text from turns" \
//!   | cargo run -p echomuse-grammar --example classify -- [NAMES AREAS FLOORS]
//! ```
//!
//! The optional arguments are files of Home Assistant's exposed names, areas and floors, one
//! per line; without them those lists are open.

use std::io::{self, BufRead, Write};

use echomuse_grammar::{Grammar, Route, Vocabulary};

fn lines(path: &str) -> io::Result<Vec<String>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut grammar = Grammar::new()?;
    if let [names, areas, floors] = args.as_slice() {
        grammar = grammar.with_vocabulary(&Vocabulary {
            names: Some(lines(names)?),
            areas: Some(lines(areas)?),
            floors: Some(lines(floors)?),
        });
    }
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in io::stdin().lock().lines() {
        let line = line?;
        let route = match grammar.route(&line) {
            Route::Repeat => "repeat".to_owned(),
            Route::SetAlarm(r) => format!("set_alarm {:?} {:?}", r.time, r.day),
            Route::AskMeridiem(p) => format!("ask_meridiem {}:{:02}", p.hour, p.minute),
            Route::CancelAlarms(c) => format!("cancel_alarms {c:?}"),
            Route::AlarmQuery(q) => format!("alarm_query {q:?}"),
            Route::CancelTimers(t) => format!("cancel_timers {t:?}"),
            Route::SendToHa => "ha".to_owned(),
        };
        writeln!(out, "{line}\t{:?}\t{route}", grammar.classify(&line))?;
    }
    Ok(())
}
