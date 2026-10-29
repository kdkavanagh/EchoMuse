//! Running the word automaton over a transcript.
//!
//! The matcher follows every template at once, word by word, keeping for each position in the
//! automaton the reading that explains the most words literally. A word *anchors* a reading
//! when a template word, a closed list value, a number or a known vocabulary phrase matched
//! it; words taken by a wildcard (or by a vocabulary list that is not known) do not. Like
//! hassil, which ranks matches by the literal text they explain, the readings with the most
//! anchors decide.

use std::{collections::BTreeMap, collections::HashMap, rc::Rc, sync::Arc};

use crate::{
    program::{CapId, Edge, Group, Program, SentenceId, SlotId, SlotKind, StateId},
    vocabulary::{PhraseTrie, VocabIndex},
};

/// The longest name a vocabulary list that is not known stands for, in words.
const OPEN_NAME_WORDS: usize = 3;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Loc {
    /// At a state.
    At(StateId),
    /// Inside the open slot edge `edge` of `state`, filled from token `start`.
    Open {
        state: StateId,
        edge: u16,
        start: u16,
    },
    /// Inside the vocabulary slot edge `edge` of `state`, at trie `node`.
    Phrase {
        state: StateId,
        edge: u16,
        start: u16,
        node: u32,
    },
}

#[derive(Clone, Copy, Debug)]
enum CapEvent {
    Begin(CapId, u16),
    End(CapId, u16),
    Slot { slot: SlotId, start: u16, end: u16 },
}

#[derive(Debug)]
struct CapNode {
    event: CapEvent,
    prev: Caps,
}

type Caps = Option<Rc<CapNode>>;

fn push(caps: &Caps, event: CapEvent) -> Caps {
    Some(Rc::new(CapNode {
        event,
        prev: caps.clone(),
    }))
}

#[derive(Clone, Debug)]
pub(crate) struct Thread {
    pub loc: Loc,
    pub anchors: u16,
    /// Words given to a vocabulary list that is not known: guesses, not evidence.
    pub guessed: u16,
    /// Whether the last word went into a wildcard or an unknown name.
    pub tail_open: bool,
    caps: Caps,
}

impl Thread {
    /// More anchors is better; among equals, fewer guesses.
    fn score(&self) -> (u16, std::cmp::Reverse<u16>) {
        (self.anchors, std::cmp::Reverse(self.guessed))
    }

    /// Whether more of its words are guesses than evidence.
    pub fn is_guesswork(&self) -> bool {
        self.guessed > self.anchors
    }
}

/// A slot value of an accepted reading.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Capture {
    pub slot: Arc<str>,
    /// The list value's `out`, when the list has one.
    pub value: Option<Arc<str>>,
    /// The words it took.
    pub text: String,
}

impl Capture {
    /// The value, or the words when there is none.
    pub fn value_or_text(&self) -> &str {
        self.value.as_deref().unwrap_or(&self.text)
    }
}

/// A sentence that explains the whole transcript.
#[derive(Clone, Debug)]
pub(crate) struct Accepted {
    pub sentence: SentenceId,
    pub anchors: u16,
    pub guessed: u16,
    caps: Caps,
}

impl Accepted {
    /// Whether more of its words are guesses than evidence.
    pub fn is_guesswork(&self) -> bool {
        self.guessed > self.anchors
    }
}

/// The state after the last word.
pub(crate) struct RunResult<'p> {
    pub program: &'p Program,
    pub tokens: Vec<String>,
    /// Every live reading, closed over empty moves.
    pub threads: Vec<Thread>,
    /// The best reading of each sentence that explains every word.
    pub accepts: Vec<Accepted>,
}

impl RunResult<'_> {
    /// The slot values of an accepted reading, in the order they were said.
    pub fn captures(&self, accepted: &Accepted) -> Vec<Capture> {
        let mut events = Vec::new();
        let mut node = accepted.caps.clone();
        while let Some(n) = node {
            events.push(n.event);
            node = n.prev.clone();
        }
        events.reverse();
        let mut open: Vec<(CapId, u16)> = Vec::new();
        let mut out = Vec::new();
        for event in events {
            match event {
                CapEvent::Begin(cap, pos) => open.push((cap, pos)),
                CapEvent::End(cap, end) => {
                    if let Some(i) = open.iter().rposition(|(c, _)| *c == cap) {
                        let (_, start) = open.remove(i);
                        let def = &self.program.captures[cap as usize];
                        out.push(Capture {
                            slot: def.slot.clone(),
                            value: def.value.clone(),
                            text: self.text(start, end),
                        });
                    }
                }
                CapEvent::Slot { slot, start, end } => {
                    let def = &self.program.slots[slot as usize];
                    out.push(Capture {
                        slot: def.name.clone(),
                        value: None,
                        text: self.text(start, end),
                    });
                }
            }
        }
        out
    }

    fn text(&self, start: u16, end: u16) -> String {
        self.tokens[start as usize..end as usize].join(" ")
    }

    /// Whether every value an accepted reading put into a wildcard starts and ends with
    /// a word that can start or end a name: not a number, and not one of `open_words`
    /// ("the", "for", …). "set a timer for 5 minutes for" is no timer named "for".
    pub fn accepted_wildcards_bounded(&self, accepted: &Accepted, open_words: &[&str]) -> bool {
        self.wildcards_bounded(&accepted.caps, open_words)
    }

    /// [`Self::accepted_wildcards_bounded`] for a reading still under way.
    pub fn wildcards_bounded_so_far(&self, thread: &Thread, open_words: &[&str]) -> bool {
        self.wildcards_bounded(&thread.caps, open_words)
    }

    fn wildcards_bounded(&self, caps: &Caps, open_words: &[&str]) -> bool {
        let bounding =
            |token: &String| integer(token).is_none() && !open_words.contains(&token.as_str());
        let mut node = caps.as_deref();
        while let Some(n) = node {
            if let CapEvent::Slot { slot, start, end } = n.event
                && matches!(self.program.slots[slot as usize].kind, SlotKind::Wildcard)
                && !(bounding(&self.tokens[start as usize])
                    && bounding(&self.tokens[end as usize - 1]))
            {
                return false;
            }
            node = n.prev.as_deref();
        }
        true
    }

    /// Whether a reading that has not finished its sentence waits for a value: its next
    /// edge is a slot (a number, a name, any words), or it has just put a number into a
    /// slot and the unit follows ("set a timer for 5").
    pub fn waits_for_value(&self, thread: &Thread) -> bool {
        match thread.loc {
            Loc::At(state) => {
                self.program.states[state as usize]
                    .iter()
                    .any(|e| matches!(e, Edge::Slot(..)))
                    || self.just_took_number(thread)
            }
            // Inside a wildcard its way out is a reading of its own; part way through a
            // known name is no value yet.
            Loc::Open { .. } | Loc::Phrase { .. } => false,
        }
    }

    /// Whether the reading's last word went into a number slot.
    fn just_took_number(&self, thread: &Thread) -> bool {
        let last = self.tokens.len() as u16;
        let mut node = thread.caps.as_deref();
        while let Some(n) = node {
            match n.event {
                CapEvent::Slot { slot, end, .. } => {
                    return end == last
                        && matches!(
                            self.program.slots[slot as usize].kind,
                            SlotKind::Range { .. } | SlotKind::Ordinal { .. }
                        );
                }
                CapEvent::Begin(_, at) | CapEvent::End(_, at) if at == last => {
                    node = n.prev.as_deref();
                }
                CapEvent::Begin(..) | CapEvent::End(..) => return false,
            }
        }
        false
    }

    /// The sentence a reading follows.
    pub fn sentence(&self, thread: &Thread) -> SentenceId {
        let state = match thread.loc {
            Loc::At(state) | Loc::Open { state, .. } | Loc::Phrase { state, .. } => state,
        };
        self.program.owner[state as usize]
    }
}

fn phrase_trie<'v>(
    program: &Program,
    vocab: &'v VocabIndex,
    state: StateId,
    edge: u16,
) -> Option<&'v PhraseTrie> {
    match program.states[state as usize][edge as usize] {
        Edge::Slot(slot, _) => match program.slots[slot as usize].kind {
            SlotKind::Vocabulary(list) => vocab.trie(list),
            _ => None,
        },
        _ => None,
    }
}

/// Removes skip words (longest first, wherever they occur as whole words).
pub(crate) fn remove_skip_words(program: &Program, tokens: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0;
    'outer: while i < tokens.len() {
        for skip in &program.skip_words {
            if tokens[i..].starts_with(skip) {
                i += skip.len();
                continue 'outer;
            }
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    out
}

/// Runs the sentences of `groups` over `tokens` (already normalized, skip words removed).
pub(crate) fn run<'p>(
    program: &'p Program,
    vocab: &'p VocabIndex,
    tokens: Vec<String>,
    groups: &[Group],
) -> RunResult<'p> {
    let mut runner = Runner {
        program,
        vocab,
        tokens: &tokens,
    };
    let seeds: Vec<Thread> = program
        .sentences
        .iter()
        .filter(|s| groups.contains(&s.group))
        .map(|s| Thread {
            loc: Loc::At(s.start),
            anchors: 0,
            guessed: 0,
            tail_open: false,
            caps: None,
        })
        .collect();
    let mut accepts = BTreeMap::new();
    let last = tokens.len();
    let mut threads = runner.closure(seeds, 0, (last == 0).then_some(&mut accepts));
    for pos in 0..last {
        let stepped = runner.step(&threads, pos);
        if stepped.is_empty() {
            threads = Vec::new();
            break;
        }
        let at_end = pos + 1 == last;
        threads = runner.closure(stepped, pos + 1, at_end.then_some(&mut accepts));
    }
    RunResult {
        program,
        tokens,
        threads,
        accepts: accepts.into_values().collect(),
    }
}

struct Runner<'a> {
    program: &'a Program,
    vocab: &'a VocabIndex,
    tokens: &'a [String],
}

impl Runner<'_> {
    fn closure(
        &mut self,
        seeds: Vec<Thread>,
        pos: usize,
        mut accepts: Option<&mut BTreeMap<SentenceId, Accepted>>,
    ) -> Vec<Thread> {
        let pos16 = pos as u16;
        let mut index: HashMap<Loc, usize> = HashMap::with_capacity(seeds.len() * 2);
        let mut out: Vec<Thread> = Vec::with_capacity(seeds.len() * 2);
        let mut stack = seeds;
        stack.reverse();
        while let Some(thread) = stack.pop() {
            match index.get(&thread.loc) {
                Some(&i) if out[i].score() >= thread.score() => continue,
                Some(&i) => out[i] = thread.clone(),
                None => {
                    index.insert(thread.loc, out.len());
                    out.push(thread.clone());
                }
            }
            match thread.loc {
                Loc::At(state) => {
                    for edge in self.program.states[state as usize].iter().rev() {
                        match *edge {
                            Edge::Eps(to) => stack.push(Thread {
                                loc: Loc::At(to),
                                ..thread.clone()
                            }),
                            Edge::Begin(cap, to) => stack.push(Thread {
                                loc: Loc::At(to),
                                anchors: thread.anchors,
                                guessed: thread.guessed,
                                tail_open: thread.tail_open,
                                caps: push(&thread.caps, CapEvent::Begin(cap, pos16)),
                            }),
                            Edge::End(cap, to) => stack.push(Thread {
                                loc: Loc::At(to),
                                anchors: thread.anchors,
                                guessed: thread.guessed,
                                tail_open: thread.tail_open,
                                caps: push(&thread.caps, CapEvent::End(cap, pos16)),
                            }),
                            Edge::Accept(sentence) => {
                                if let Some(accepts) = accepts.as_deref_mut() {
                                    let better = accepts.get(&sentence).is_none_or(|a| {
                                        (a.anchors, std::cmp::Reverse(a.guessed)) < thread.score()
                                    });
                                    if better {
                                        accepts.insert(
                                            sentence,
                                            Accepted {
                                                sentence,
                                                anchors: thread.anchors,
                                                guessed: thread.guessed,
                                                caps: thread.caps.clone(),
                                            },
                                        );
                                    }
                                }
                            }
                            Edge::Word(..) | Edge::Slot(..) => {}
                        }
                    }
                }
                Loc::Open { state, edge, start } => {
                    if let Some(exit) = self.slot_exit(state, edge, start, pos16, &thread) {
                        stack.push(exit);
                    }
                }
                Loc::Phrase {
                    state,
                    edge,
                    start,
                    node,
                } => {
                    let terminal = phrase_trie(self.program, self.vocab, state, edge)
                        .is_some_and(|trie| trie.terminal(node));
                    if terminal
                        && let Some(exit) = self.slot_exit(state, edge, start, pos16, &thread)
                    {
                        stack.push(exit);
                    }
                }
            }
        }
        out
    }

    fn slot_exit(
        &self,
        state: StateId,
        edge: u16,
        start: u16,
        pos: u16,
        thread: &Thread,
    ) -> Option<Thread> {
        match self.program.states[state as usize][edge as usize] {
            Edge::Slot(slot, to) if pos > start => Some(Thread {
                loc: Loc::At(to),
                anchors: thread.anchors,
                guessed: thread.guessed,
                tail_open: thread.tail_open,
                caps: push(
                    &thread.caps,
                    CapEvent::Slot {
                        slot,
                        start,
                        end: pos,
                    },
                ),
            }),
            _ => None,
        }
    }

    fn step(&self, threads: &[Thread], pos: usize) -> Vec<Thread> {
        let token = self.tokens[pos].as_str();
        let sym = self.program.sym(token);
        let pos16 = pos as u16;
        let mut out = Vec::new();
        for thread in threads {
            match thread.loc {
                Loc::At(state) => {
                    for (i, edge) in self.program.states[state as usize].iter().enumerate() {
                        match *edge {
                            Edge::Word(word, to) if Some(word) == sym => out.push(Thread {
                                loc: Loc::At(to),
                                anchors: thread.anchors + 1,
                                guessed: thread.guessed,
                                tail_open: false,
                                caps: thread.caps.clone(),
                            }),
                            Edge::Slot(slot, to) => {
                                if let Some(next) =
                                    self.enter_slot(thread, state, i as u16, slot, to, token, pos16)
                                {
                                    out.push(next);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Loc::Open { state, edge, start } => {
                    // A vocabulary list that is not known stands for names of at most
                    // `OPEN_NAME_WORDS` words; a wildcard takes any number.
                    let vocabulary = matches!(
                        self.program.states[state as usize][edge as usize],
                        Edge::Slot(slot, _)
                            if matches!(self.program.slots[slot as usize].kind, SlotKind::Vocabulary(_))
                    );
                    if !vocabulary {
                        out.push(thread.clone());
                    } else if pos - (start as usize) < OPEN_NAME_WORDS {
                        out.push(Thread {
                            guessed: thread.guessed + 1,
                            tail_open: true,
                            ..thread.clone()
                        });
                    }
                }
                Loc::Phrase {
                    state,
                    edge,
                    start,
                    node,
                } => {
                    if let Some(next) = phrase_trie(self.program, self.vocab, state, edge)
                        .and_then(|trie| trie.child(node, token))
                    {
                        out.push(Thread {
                            loc: Loc::Phrase {
                                state,
                                edge,
                                start,
                                node: next,
                            },
                            anchors: thread.anchors + 1,
                            guessed: thread.guessed,
                            tail_open: false,
                            caps: thread.caps.clone(),
                        });
                    }
                }
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn enter_slot(
        &self,
        thread: &Thread,
        state: StateId,
        edge: u16,
        slot: SlotId,
        to: StateId,
        token: &str,
        pos: u16,
    ) -> Option<Thread> {
        let whole_token = |matched: bool| {
            matched.then(|| Thread {
                loc: Loc::At(to),
                anchors: thread.anchors + 1,
                guessed: thread.guessed,
                tail_open: false,
                caps: push(
                    &thread.caps,
                    CapEvent::Slot {
                        slot,
                        start: pos,
                        end: pos + 1,
                    },
                ),
            })
        };
        let open = |guess: u16| Thread {
            loc: Loc::Open {
                state,
                edge,
                start: pos,
            },
            anchors: thread.anchors,
            guessed: thread.guessed + guess,
            tail_open: true,
            caps: thread.caps.clone(),
        };
        match &self.program.slots[slot as usize].kind {
            SlotKind::Range {
                from,
                to: last,
                step,
                halves,
            } => whole_token(number_in_range(token, *from, *last, *step, *halves)),
            SlotKind::Ordinal { from, to: last } => {
                whole_token(ordinal_value(token).is_some_and(|n| n >= *from && n <= *last))
            }
            SlotKind::Wildcard => Some(open(0)),
            SlotKind::Vocabulary(list) => match self.vocab.trie(*list) {
                None => Some(open(1)),
                Some(trie) => trie.child(PhraseTrie::ROOT, token).map(|node| Thread {
                    loc: Loc::Phrase {
                        state,
                        edge,
                        start: pos,
                        node,
                    },
                    anchors: thread.anchors + 1,
                    guessed: thread.guessed,
                    tail_open: false,
                    caps: thread.caps.clone(),
                }),
            },
        }
    }
}

/// The value of a token of digits (leading zeros allowed).
pub(crate) fn integer(token: &str) -> Option<i64> {
    if token.is_empty() || token.len() > 9 || !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    token.parse().ok()
}

fn number_in_range(token: &str, from: i64, to: i64, step: i64, halves: bool) -> bool {
    if let Some(n) = integer(token) {
        return n >= from && n <= to && (n - from) % step == 0;
    }
    if halves
        && let Some(whole) = token.strip_suffix(".5")
        && let Some(n) = integer(whole)
    {
        return n >= from && n < to;
    }
    false
}

/// The value of an ordinal token such as `3rd`.
pub(crate) fn ordinal_value(token: &str) -> Option<i64> {
    let digits = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|suffix| token.strip_suffix(suffix))?;
    integer(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_ordinals() {
        assert!(number_in_range("5", 1, 100, 1, false));
        assert!(number_in_range("05", 0, 59, 1, false));
        assert!(!number_in_range("0", 1, 100, 1, false));
        assert!(number_in_range("2700", 1000, 10000, 100, false));
        assert!(!number_in_range("2750", 1000, 10000, 100, false));
        assert!(number_in_range("21.5", 0, 100, 1, true));
        assert!(!number_in_range("21.5", 0, 100, 1, false));
        assert_eq!(ordinal_value("23rd"), Some(23));
        assert_eq!(ordinal_value("rd"), None);
        assert_eq!(ordinal_value("23"), None);
    }
}
