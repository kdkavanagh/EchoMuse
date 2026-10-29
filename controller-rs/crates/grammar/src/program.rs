//! Compiling template sets into one word automaton.
//!
//! A template is first built into an automaton over pieces of text and spaces, so that text
//! written against a group without a space joins into one word, as in hassil (`minute[s]`).
//! That automaton is then rewritten over whole words: every path of pieces between two spaces
//! becomes one word edge, its word normalized exactly as transcripts are
//! ([`crate::normalize`]). Lists become either inline alternatives (value lists, whose values
//! are templates themselves) or slot edges matched at run time (number ranges, ordinals,
//! wildcards and the vocabulary lists `name`, `area` and `floor`).

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt,
    sync::Arc,
};

use crate::{
    normalize::normalize,
    template::{self, Expr, TemplateError},
};

pub(crate) type StateId = u32;
pub(crate) type Sym = u32;
pub(crate) type SlotId = u32;
pub(crate) type CapId = u32;
pub(crate) type SentenceId = u32;

/// An edge of the word automaton.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Edge {
    Eps(StateId),
    Word(Sym, StateId),
    Slot(SlotId, StateId),
    Begin(CapId, StateId),
    End(CapId, StateId),
    Accept(SentenceId),
}

/// The vocabulary lists Home Assistant fills from its entity, area and floor registries.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum VocabList {
    Name,
    Area,
    Floor,
}

/// How a slot edge matches words.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SlotKind {
    /// One number token in `from..=to` on `step`; `halves` also takes `n.5`.
    Range {
        from: i64,
        to: i64,
        step: i64,
        halves: bool,
    },
    /// One ordinal token (`3rd`) in `from..=to`.
    Ordinal { from: i64, to: i64 },
    /// One or more words of any kind.
    Wildcard,
    /// A phrase of a vocabulary list; any words when the vocabulary is not known.
    Vocabulary(VocabList),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SlotDef {
    pub name: Arc<str>,
    pub kind: SlotKind,
}

/// A value-list capture: the slot and the value's `out`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CaptureDef {
    pub slot: Arc<str>,
    pub value: Option<Arc<str>>,
}

/// Where a sentence comes from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Origin {
    /// Home Assistant's intent data.
    Ha { intent: Arc<str> },
    /// This crate's own templates.
    Local {
        intent: crate::local::LocalIntent,
        flag: crate::local::Flag,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sentence {
    pub origin: Origin,
    pub template: Arc<str>,
    pub start: StateId,
    pub group: Group,
}

/// Which sentences a run starts from.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Group {
    /// Home Assistant's sentences, except its timer intents.
    Ha,
    /// Home Assistant's timer intents (`HassStartTimer`, `HassCancelTimer`, …): the one
    /// family of its sentences the local grammar judges for completeness (§4.6).
    HaTimers,
    /// The controller's own commands.
    Commands,
    /// Answers to the controller's questions.
    Replies,
}

/// A list as a template set defines it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ListDef {
    Values(Vec<ValueDef>),
    Range {
        from: i64,
        to: i64,
        step: i64,
        halves: bool,
    },
    Ordinal {
        from: i64,
        to: i64,
    },
    Wildcard,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ValueDef {
    pub template: String,
    pub out: Option<String>,
}

/// One set of templates with its own rules and lists.
#[derive(Clone, Debug, Default)]
pub(crate) struct TemplateSet {
    pub rules: BTreeMap<String, String>,
    pub lists: BTreeMap<String, ListDef>,
    pub sentences: Vec<(String, Origin, Group)>,
    pub skip_words: Vec<String>,
}

/// The vendored data or a local template could not be compiled.
#[derive(Clone, Debug, PartialEq)]
pub enum CompileError {
    /// A template did not parse.
    Template(TemplateError),
    /// A template names a rule no set defines.
    UnknownRule(String),
    /// A template names a list no set defines.
    UnknownList(String),
    /// Rules refer to each other in a loop.
    RuleLoop(String),
    /// The intent data file is not what this crate reads.
    Data(String),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::Template(err) => err.fmt(f),
            CompileError::UnknownRule(name) => write!(f, "unknown expansion rule <{name}>"),
            CompileError::UnknownList(name) => write!(f, "unknown list {{{name}}}"),
            CompileError::RuleLoop(name) => write!(f, "expansion rule <{name}> refers to itself"),
            CompileError::Data(reason) => write!(f, "intent data: {reason}"),
        }
    }
}

impl std::error::Error for CompileError {}

impl From<TemplateError> for CompileError {
    fn from(err: TemplateError) -> Self {
        CompileError::Template(err)
    }
}

/// The compiled automaton.
#[derive(Debug)]
pub(crate) struct Program {
    pub states: Vec<Box<[Edge]>>,
    pub words: HashMap<String, Sym>,
    pub slots: Vec<SlotDef>,
    pub captures: Vec<CaptureDef>,
    pub sentences: Vec<Sentence>,
    /// Skip words, normalized, longest first.
    pub skip_words: Vec<Vec<String>>,
    /// The sentence each state belongs to: every sentence is compiled into states of its
    /// own, so a reading's state says which sentence it follows.
    pub owner: Vec<SentenceId>,
}

impl Program {
    pub fn sym(&self, word: &str) -> Option<Sym> {
        self.words.get(word).copied()
    }

    /// Compiles the sets into one automaton.
    pub fn compile(sets: &[TemplateSet]) -> Result<Self, CompileError> {
        let mut builder = Builder::default();
        let mut skip: Vec<Vec<String>> = Vec::new();
        for set in sets {
            builder.add_set(set)?;
            for word in &set.skip_words {
                let tokens = normalize(word).into_tokens();
                if !tokens.is_empty() && !skip.contains(&tokens) {
                    skip.push(tokens);
                }
            }
        }
        skip.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        let mut program = builder.into_words();
        program.skip_words = skip;
        program.owner = owners(&program);
        Ok(program)
    }
}

/// The sentence each state is reached from (states are never shared between sentences).
fn owners(program: &Program) -> Vec<SentenceId> {
    let mut owner = vec![SentenceId::MAX; program.states.len()];
    for (id, sentence) in program.sentences.iter().enumerate() {
        let mut stack = vec![sentence.start];
        while let Some(state) = stack.pop() {
            let slot = &mut owner[state as usize];
            if *slot != SentenceId::MAX {
                continue;
            }
            *slot = id as SentenceId;
            for edge in &program.states[state as usize] {
                match *edge {
                    Edge::Eps(to)
                    | Edge::Word(_, to)
                    | Edge::Slot(_, to)
                    | Edge::Begin(_, to)
                    | Edge::End(_, to) => stack.push(to),
                    Edge::Accept(_) => {}
                }
            }
        }
    }
    owner
}

#[derive(Clone, Debug)]
enum PEdge {
    Eps(StateId),
    Piece(String, StateId),
    Space(StateId),
    Slot(SlotId, StateId),
    Begin(CapId, StateId),
    End(CapId, StateId),
    Accept(SentenceId),
}

/// The value captures of one list reference.
#[derive(Clone, Debug)]
enum CompiledList {
    Values(Vec<(Expr, CapId)>),
    Slot(SlotId),
}

#[derive(Default)]
struct Builder {
    pieces: Vec<Vec<PEdge>>,
    slots: Vec<SlotDef>,
    captures: Vec<CaptureDef>,
    sentences: Vec<(Origin, Arc<str>, StateId, Group)>,
}

/// A value of a value list: its template and its `out`.
type ParsedValue = (Expr, Option<Arc<str>>);

struct SetContext<'a> {
    set: &'a TemplateSet,
    rules: HashMap<&'a str, Expr>,
    values: HashMap<&'a str, Vec<ParsedValue>>,
    lists: HashMap<(String, String), CompiledList>,
}

impl Builder {
    fn state(&mut self) -> StateId {
        self.pieces.push(Vec::new());
        (self.pieces.len() - 1) as StateId
    }

    fn edge(&mut self, from: StateId, edge: PEdge) {
        self.pieces[from as usize].push(edge);
    }

    fn add_set(&mut self, set: &TemplateSet) -> Result<(), CompileError> {
        let mut rules = HashMap::new();
        for (name, text) in &set.rules {
            rules.insert(name.as_str(), template::parse(text)?);
        }
        let mut values = HashMap::new();
        for (name, list) in &set.lists {
            if let ListDef::Values(items) = list {
                let mut parsed = Vec::with_capacity(items.len());
                for item in items {
                    parsed.push((
                        template::parse(&item.template)?,
                        item.out.as_deref().map(Arc::from),
                    ));
                }
                values.insert(name.as_str(), parsed);
            }
        }
        let mut ctx = SetContext {
            set,
            rules,
            values,
            lists: HashMap::new(),
        };
        for (text, origin, group) in &set.sentences {
            let expr = template::parse(text)?;
            let start = self.state();
            let end = self.build(&expr, start, &mut ctx, &mut Vec::new())?;
            let id = self.sentences.len() as SentenceId;
            self.edge(end, PEdge::Accept(id));
            self.sentences
                .push((origin.clone(), Arc::from(text.as_str()), start, *group));
        }
        Ok(())
    }

    fn build(
        &mut self,
        expr: &Expr,
        from: StateId,
        ctx: &mut SetContext<'_>,
        rule_stack: &mut Vec<String>,
    ) -> Result<StateId, CompileError> {
        match expr {
            Expr::Text(text) => Ok(self.text(text, from)),
            Expr::Seq(items) => {
                let mut cur = from;
                for item in items {
                    cur = self.build(item, cur, ctx, rule_stack)?;
                }
                Ok(cur)
            }
            Expr::Alt(items) => {
                let end = self.state();
                for item in items {
                    let start = self.state();
                    self.edge(from, PEdge::Eps(start));
                    let item_end = self.build(item, start, ctx, rule_stack)?;
                    self.edge(item_end, PEdge::Eps(end));
                }
                Ok(end)
            }
            Expr::Perm(items) => {
                let orders = permutations(items.len());
                let alt = Expr::Alt(
                    orders
                        .into_iter()
                        .map(|order| {
                            Expr::Seq(order.into_iter().map(|i| items[i].clone()).collect())
                        })
                        .collect(),
                );
                self.build(&alt, from, ctx, rule_stack)
            }
            Expr::Rule(name) => {
                if rule_stack.iter().any(|r| r == name) {
                    return Err(CompileError::RuleLoop(name.clone()));
                }
                let rule = ctx
                    .rules
                    .get(name.as_str())
                    .cloned()
                    .ok_or_else(|| CompileError::UnknownRule(name.clone()))?;
                rule_stack.push(name.clone());
                let end = self.build(&rule, from, ctx, rule_stack)?;
                rule_stack.pop();
                Ok(end)
            }
            Expr::List { list, slot } => {
                let compiled = self.list(list, slot, ctx)?;
                match compiled {
                    CompiledList::Slot(id) => {
                        let end = self.state();
                        self.edge(from, PEdge::Slot(id, end));
                        Ok(end)
                    }
                    CompiledList::Values(values) => {
                        let end = self.state();
                        for (value, cap) in values {
                            let start = self.state();
                            self.edge(from, PEdge::Begin(cap, start));
                            let value_end = self.build(&value, start, ctx, rule_stack)?;
                            let closed = self.state();
                            self.edge(value_end, PEdge::End(cap, closed));
                            self.edge(closed, PEdge::Eps(end));
                        }
                        Ok(end)
                    }
                }
            }
        }
    }

    fn list(
        &mut self,
        list: &str,
        slot: &str,
        ctx: &mut SetContext<'_>,
    ) -> Result<CompiledList, CompileError> {
        let key = (list.to_owned(), slot.to_owned());
        if let Some(found) = ctx.lists.get(&key) {
            return Ok(found.clone());
        }
        let slot_name: Arc<str> = Arc::from(slot);
        let kind = match ctx.set.lists.get(list) {
            Some(ListDef::Values(_)) => None,
            Some(ListDef::Range {
                from,
                to,
                step,
                halves,
            }) => Some(SlotKind::Range {
                from: *from,
                to: *to,
                step: *step,
                halves: *halves,
            }),
            Some(ListDef::Ordinal { from, to }) => Some(SlotKind::Ordinal {
                from: *from,
                to: *to,
            }),
            Some(ListDef::Wildcard) => Some(SlotKind::Wildcard),
            None => match list {
                "name" => Some(SlotKind::Vocabulary(VocabList::Name)),
                "area" => Some(SlotKind::Vocabulary(VocabList::Area)),
                "floor" => Some(SlotKind::Vocabulary(VocabList::Floor)),
                _ => return Err(CompileError::UnknownList(list.to_owned())),
            },
        };
        let compiled = match kind {
            Some(kind) => {
                self.slots.push(SlotDef {
                    name: slot_name,
                    kind,
                });
                CompiledList::Slot((self.slots.len() - 1) as SlotId)
            }
            None => {
                let values = ctx.values.get(list).cloned().unwrap_or_default();
                let mut out = Vec::with_capacity(values.len());
                for (expr, value) in values {
                    self.captures.push(CaptureDef {
                        slot: slot_name.clone(),
                        value,
                    });
                    out.push((expr, (self.captures.len() - 1) as CapId));
                }
                CompiledList::Values(out)
            }
        };
        ctx.lists.insert(key, compiled.clone());
        Ok(compiled)
    }

    /// Literal text: pieces between whitespace, and a space edge for each run of
    /// whitespace. A hyphen separates words, as it does in transcripts.
    fn text(&mut self, text: &str, from: StateId) -> StateId {
        let mut cur = from;
        let mut piece = String::new();
        for c in text.chars() {
            if c.is_whitespace() || c == '-' {
                if !piece.is_empty() {
                    let next = self.state();
                    self.edge(cur, PEdge::Piece(std::mem::take(&mut piece), next));
                    cur = next;
                }
                let next = self.state();
                self.edge(cur, PEdge::Space(next));
                cur = next;
            } else {
                piece.push(c);
            }
        }
        if !piece.is_empty() {
            let next = self.state();
            self.edge(cur, PEdge::Piece(piece, next));
            cur = next;
        }
        cur
    }

    /// Rewrites the piece automaton over whole words.
    fn into_words(self) -> Program {
        let mut conv = WordBuilder {
            pieces: &self.pieces,
            map: vec![None; self.pieces.len()],
            states: Vec::new(),
            words: HashMap::new(),
            queue: VecDeque::new(),
            emitted: HashSet::new(),
        };
        let mut sentences = Vec::with_capacity(self.sentences.len());
        for (origin, template, start, group) in self.sentences {
            let start = conv.word_state(start);
            sentences.push(Sentence {
                origin,
                template,
                start,
                group,
            });
        }
        while let Some(piece_state) = conv.queue.pop_front() {
            conv.process(piece_state);
        }
        Program {
            states: conv.states.into_iter().map(Vec::into_boxed_slice).collect(),
            words: conv.words,
            slots: self.slots,
            captures: self.captures,
            sentences,
            skip_words: Vec::new(),
            owner: Vec::new(),
        }
    }
}

fn permutations(n: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return vec![Vec::new()];
    }
    let mut out = Vec::new();
    for rest in permutations(n - 1) {
        for pos in 0..=rest.len() {
            let mut order = rest.clone();
            order.insert(pos, n - 1);
            out.push(order);
        }
    }
    out.sort();
    out
}

struct WordBuilder<'a> {
    pieces: &'a [Vec<PEdge>],
    map: Vec<Option<StateId>>,
    states: Vec<Vec<Edge>>,
    words: HashMap<String, Sym>,
    queue: VecDeque<StateId>,
    emitted: HashSet<(StateId, String, StateId)>,
}

impl WordBuilder<'_> {
    /// The word state of a piece state where a word may start, queued for processing the
    /// first time it is seen.
    fn word_state(&mut self, piece_state: StateId) -> StateId {
        if let Some(id) = self.map[piece_state as usize] {
            return id;
        }
        let id = self.new_state();
        self.map[piece_state as usize] = Some(id);
        self.queue.push_back(piece_state);
        id
    }

    fn new_state(&mut self) -> StateId {
        self.states.push(Vec::new());
        (self.states.len() - 1) as StateId
    }

    fn push(&mut self, from: StateId, edge: Edge) {
        let edges = &mut self.states[from as usize];
        if !edges.contains(&edge) {
            edges.push(edge);
        }
    }

    fn sym(&mut self, word: &str) -> Sym {
        if let Some(&sym) = self.words.get(word) {
            return sym;
        }
        let sym = self.words.len() as Sym;
        self.words.insert(word.to_owned(), sym);
        sym
    }

    fn process(&mut self, piece_state: StateId) {
        let from = self.map[piece_state as usize].expect("queued states are mapped");
        let pieces = self.pieces;
        for edge in &pieces[piece_state as usize] {
            match edge {
                PEdge::Eps(t) | PEdge::Space(t) => {
                    let to = self.word_state(*t);
                    self.push(from, Edge::Eps(to));
                }
                PEdge::Slot(slot, t) => {
                    let to = self.word_state(*t);
                    self.push(from, Edge::Slot(*slot, to));
                }
                PEdge::Begin(cap, t) => {
                    let to = self.word_state(*t);
                    self.push(from, Edge::Begin(*cap, to));
                }
                PEdge::End(cap, t) => {
                    let to = self.word_state(*t);
                    self.push(from, Edge::End(*cap, to));
                }
                PEdge::Accept(id) => self.push(from, Edge::Accept(*id)),
                PEdge::Piece(piece, t) => self.word_paths(from, *t, piece.clone()),
            }
        }
    }

    /// Follows every path of pieces from `state` until the word ends, emitting a word edge
    /// from `from` for each.
    fn word_paths(&mut self, from: StateId, state: StateId, word: String) {
        let mut stack = vec![(state, word)];
        let mut seen: HashSet<(StateId, String)> = HashSet::new();
        let pieces = self.pieces;
        while let Some((state, word)) = stack.pop() {
            if !seen.insert((state, word.clone())) {
                continue;
            }
            let mut boundary = false;
            for edge in &pieces[state as usize] {
                match edge {
                    PEdge::Piece(piece, t) => stack.push((*t, format!("{word}{piece}"))),
                    PEdge::Eps(t) => stack.push((*t, word.clone())),
                    PEdge::Space(t) => {
                        let to = self.word_state(*t);
                        self.emit(from, &word, to);
                    }
                    PEdge::Slot(..) | PEdge::Begin(..) | PEdge::End(..) | PEdge::Accept(_) => {
                        boundary = true;
                    }
                }
            }
            if boundary {
                let to = self.word_state(state);
                self.emit(from, &word, to);
            }
        }
    }

    fn emit(&mut self, from: StateId, word: &str, to: StateId) {
        if !self.emitted.insert((from, word.to_owned(), to)) {
            return;
        }
        let tokens = normalize(word).into_tokens();
        let Some((last, init)) = tokens.split_last() else {
            self.push(from, Edge::Eps(to));
            return;
        };
        let mut cur = from;
        for token in init {
            let next = self.new_state();
            let sym = self.sym(token);
            self.push(cur, Edge::Word(sym, next));
            cur = next;
        }
        let sym = self.sym(last);
        self.push(cur, Edge::Word(sym, to));
    }
}

#[cfg(test)]
mod tests {
    use super::permutations;

    #[test]
    fn permutations_are_complete() {
        assert_eq!(permutations(1), vec![vec![0]]);
        assert_eq!(permutations(3).len(), 6);
        let mut p = permutations(3);
        p.dedup();
        assert_eq!(p.len(), 6);
    }
}
