//! The hassil sentence template language, as Home Assistant's intent data writes it
//! (hassil 3.11.0, Apache-2.0; see `data/SOURCE`):
//!
//! - `word word` — literal text; whitespace separates words, and text written against a
//!   group without a space joins it into one word (`minute[s]` is `minute` or `minutes`);
//! - `(a|b)` — one of the alternatives;
//! - `[a]` — optional, the same as `(a|)`;
//! - `(a;b;c)` — a permutation: the items in any order, separated by spaces;
//! - `{list}` / `{list:slot}` — a value of a list, captured under the slot name (the list
//!   name when none is given);
//! - `<rule>` — an expansion rule, replaced by its template;
//! - `\x` — the character `x` taken literally.

use std::fmt;

/// A parsed template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Expr {
    /// Literal text, whitespace included.
    Text(String),
    /// The items one after another.
    Seq(Vec<Expr>),
    /// One of the items; an optional is an alternative with an empty sequence.
    Alt(Vec<Expr>),
    /// The items in any order.
    Perm(Vec<Expr>),
    /// A list reference.
    List {
        /// The list's name.
        list: String,
        /// The slot its value fills.
        slot: String,
    },
    /// An expansion rule reference.
    Rule(String),
}

/// A template the parser could not read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateError {
    /// The template.
    pub template: String,
    /// What was wrong.
    pub reason: &'static str,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "template {:?}: {}", self.template, self.reason)
    }
}

impl std::error::Error for TemplateError {}

/// Parses one template.
pub(crate) fn parse(template: &str) -> Result<Expr, TemplateError> {
    let mut parser = Parser {
        chars: template.chars().collect(),
        pos: 0,
    };
    let expr = parser.group(None).map_err(|reason| TemplateError {
        template: template.to_owned(),
        reason,
    })?;
    Ok(expr)
}

/// The plain sentences a template says, at most `limit` of them, each with its
/// whitespace collapsed. A sentence that needs a list value (`{name}`) or an expansion
/// rule (`<rule>`) has no fixed text and is left out. `Ok(None)` when the template says
/// more than `limit` sentences: a caller wanting all of them must not take a part as the
/// whole. Permutations give every order of their items.
pub fn expand(template: &str, limit: usize) -> Result<Option<Vec<String>>, TemplateError> {
    let expr = parse(template)?;
    let Some(variants) = sentences(&expr, limit) else {
        return Ok(None);
    };
    let mut out: Vec<String> = Vec::new();
    for variant in variants.into_iter().flatten() {
        let sentence = variant.split_whitespace().collect::<Vec<_>>().join(" ");
        if !sentence.is_empty() && !out.contains(&sentence) {
            out.push(sentence);
        }
    }
    Ok(Some(out))
}

/// Every text `expr` may read as (`None` where a list or rule makes it open), or `None`
/// past `limit` variants.
fn sentences(expr: &Expr, limit: usize) -> Option<Vec<Option<String>>> {
    let product = |parts: &[Vec<Option<String>>]| -> Option<Vec<Option<String>>> {
        let mut acc: Vec<Option<String>> = vec![Some(String::new())];
        for part in parts {
            let mut next = Vec::with_capacity(acc.len() * part.len());
            for a in &acc {
                for b in part {
                    next.push(match (a, b) {
                        (Some(a), Some(b)) => Some(format!("{a}{b}")),
                        _ => None,
                    });
                    if next.len() > limit {
                        return None;
                    }
                }
            }
            acc = next;
        }
        Some(acc)
    };
    match expr {
        Expr::Text(text) => Some(vec![Some(text.clone())]),
        Expr::List { .. } | Expr::Rule(_) => Some(vec![None]),
        Expr::Seq(items) => {
            let parts = items
                .iter()
                .map(|item| sentences(item, limit))
                .collect::<Option<Vec<_>>>()?;
            product(&parts)
        }
        Expr::Alt(items) => {
            let mut all = Vec::new();
            for item in items {
                all.extend(sentences(item, limit)?);
                if all.len() > limit {
                    return None;
                }
            }
            Some(all)
        }
        Expr::Perm(items) => {
            let parts = items
                .iter()
                .map(|item| sentences(item, limit))
                .collect::<Option<Vec<_>>>()?;
            let mut all = Vec::new();
            let mut order: Vec<usize> = (0..parts.len()).collect();
            permutations(&mut order, 0, &mut |order| {
                let mut ordered = Vec::with_capacity(order.len() * 2);
                for (n, &i) in order.iter().enumerate() {
                    if n > 0 {
                        ordered.push(vec![Some(" ".to_owned())]);
                    }
                    ordered.push(parts[i].clone());
                }
                match product(&ordered) {
                    Some(found) if all.len() + found.len() <= limit => {
                        all.extend(found);
                        true
                    }
                    _ => false,
                }
            })
            .then_some(all)
        }
    }
}

/// Calls `each` with every ordering of `items[from..]`; stops (false) when it does.
fn permutations(items: &mut [usize], from: usize, each: &mut dyn FnMut(&[usize]) -> bool) -> bool {
    if from + 1 >= items.len() {
        return each(items);
    }
    for i in from..items.len() {
        items.swap(from, i);
        let go_on = permutations(items, from + 1, each);
        items.swap(from, i);
        if !go_on {
            return false;
        }
    }
    true
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum GroupKind {
    Seq,
    Alt,
    Perm,
}

impl Parser {
    /// Reads items up to `close` (or the end, for the whole template).
    fn group(&mut self, close: Option<char>) -> Result<Expr, &'static str> {
        let mut kind = GroupKind::Seq;
        let mut branches: Vec<Vec<Expr>> = vec![Vec::new()];
        let mut text = String::new();
        loop {
            let Some(c) = self.chars.get(self.pos).copied() else {
                if close.is_some() {
                    return Err("unclosed group");
                }
                break;
            };
            self.pos += 1;
            if Some(c) == close {
                break;
            }
            let item = match c {
                '\\' => {
                    let escaped = self
                        .chars
                        .get(self.pos)
                        .copied()
                        .ok_or("escape at the end")?;
                    self.pos += 1;
                    text.push(escaped);
                    None
                }
                '(' => Some(self.group(Some(')'))?),
                '[' => {
                    let inner = self.group(Some(']'))?;
                    Some(Expr::Alt(vec![inner, Expr::Seq(Vec::new())]))
                }
                '{' => {
                    let name = self.name('}')?;
                    let (list, slot) = match name.split_once(':') {
                        Some((list, slot)) => (list.to_owned(), slot.to_owned()),
                        None => (name.clone(), name),
                    };
                    if list.is_empty() || slot.is_empty() {
                        return Err("empty list name");
                    }
                    Some(Expr::List { list, slot })
                }
                '<' => {
                    let name = self.name('>')?;
                    if name.is_empty() {
                        return Err("empty rule name");
                    }
                    Some(Expr::Rule(name))
                }
                '|' | ';' => {
                    let new_kind = if c == '|' {
                        GroupKind::Alt
                    } else {
                        GroupKind::Perm
                    };
                    if kind != GroupKind::Seq && kind != new_kind {
                        return Err("alternatives and permutation mixed in one group");
                    }
                    kind = new_kind;
                    flush(&mut text, branches.last_mut());
                    branches.push(Vec::new());
                    None
                }
                ')' | ']' | '}' | '>' => return Err("unbalanced delimiter"),
                other => {
                    text.push(other);
                    None
                }
            };
            if let Some(item) = item {
                flush(&mut text, branches.last_mut());
                if let Some(branch) = branches.last_mut() {
                    branch.push(item);
                }
            }
        }
        flush(&mut text, branches.last_mut());
        let mut seqs = branches.into_iter().map(Expr::Seq);
        Ok(match kind {
            GroupKind::Seq => seqs.next().unwrap_or(Expr::Seq(Vec::new())),
            GroupKind::Alt => Expr::Alt(seqs.collect()),
            GroupKind::Perm => Expr::Perm(
                seqs.map(|item| {
                    Expr::Seq(vec![
                        Expr::Text(" ".to_owned()),
                        item,
                        Expr::Text(" ".to_owned()),
                    ])
                })
                .collect(),
            ),
        })
    }

    fn name(&mut self, close: char) -> Result<String, &'static str> {
        let mut name = String::new();
        loop {
            let c = self
                .chars
                .get(self.pos)
                .copied()
                .ok_or("unclosed reference")?;
            self.pos += 1;
            if c == close {
                return Ok(name.trim().to_owned());
            }
            name.push(c);
        }
    }
}

fn flush(text: &mut String, branch: Option<&mut Vec<Expr>>) {
    if !text.is_empty()
        && let Some(branch) = branch
    {
        branch.push(Expr::Text(std::mem::take(text)));
    }
}

#[cfg(test)]
mod tests {
    use super::{Expr, expand, parse};

    fn text(s: &str) -> Expr {
        Expr::Text(s.to_owned())
    }

    #[test]
    fn reads_every_construct() {
        let expr = parse("<turn> on [the] {name:target} (a|b)").unwrap();
        assert_eq!(
            expr,
            Expr::Seq(vec![
                Expr::Rule("turn".to_owned()),
                text(" on "),
                Expr::Alt(vec![Expr::Seq(vec![text("the")]), Expr::Seq(vec![])]),
                text(" "),
                Expr::List {
                    list: "name".to_owned(),
                    slot: "target".to_owned()
                },
                text(" "),
                Expr::Alt(vec![Expr::Seq(vec![text("a")]), Expr::Seq(vec![text("b")])]),
            ])
        );
    }

    #[test]
    fn permutation_items_get_spaces() {
        let expr = parse("(on;here)").unwrap();
        let Expr::Seq(items) = expr else { panic!() };
        let Expr::Perm(perm) = &items[0] else {
            panic!()
        };
        assert_eq!(perm.len(), 2);
        assert_eq!(
            perm[0],
            Expr::Seq(vec![text(" "), Expr::Seq(vec![text("on")]), text(" ")])
        );
    }

    #[test]
    fn escapes_and_errors() {
        assert_eq!(parse(r"a\(b").unwrap(), Expr::Seq(vec![text("a(b")]));
        assert!(parse("(a|b").is_err());
        assert!(parse("a]").is_err());
        assert!(parse("(a|b;c)").is_err());
        assert!(parse("{}").is_err());
    }

    #[test]
    fn a_list_without_slot_uses_its_name() {
        assert_eq!(
            parse("{area}").unwrap(),
            Expr::Seq(vec![Expr::List {
                list: "area".to_owned(),
                slot: "area".to_owned()
            }])
        );
    }

    #[test]
    fn expansion_gives_every_fixed_sentence() {
        let found = expand("(arm|set) [the] alarm[s] (now;please)", 100)
            .unwrap()
            .unwrap();
        assert!(found.contains(&"arm the alarm now please".to_owned()));
        assert!(found.contains(&"set alarms please now".to_owned()));
        assert!(found.contains(&"arm alarm now please".to_owned()));
        assert_eq!(found.len(), 2 * 2 * 2 * 2);
    }

    #[test]
    fn sentences_needing_a_list_value_are_left_out() {
        let found = expand("(good night|turn off {area})", 10).unwrap().unwrap();
        assert_eq!(found, ["good night"]);
    }

    #[test]
    fn a_template_past_the_limit_gives_none() {
        assert_eq!(expand("(a|b|c) (d|e|f)", 8).unwrap(), None);
        assert_eq!(expand("(a|b|c) (d|e|f)", 9).unwrap().unwrap().len(), 9);
        assert!(expand("(unclosed", 9).is_err());
    }
}
