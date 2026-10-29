//! The names Home Assistant matches entities, areas and floors by.

use crate::{normalize::normalize, program::VocabList};

/// The phrases of Home Assistant's `name`, `area` and `floor` lists: exposed entities' names
/// and aliases, area names and aliases, floor names and aliases. A list that is `None` is not
/// known, and any words fill it; `Some` of an empty list matches nothing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Vocabulary {
    /// Entity names and aliases.
    pub names: Option<Vec<String>>,
    /// Area names and aliases.
    pub areas: Option<Vec<String>>,
    /// Floor names and aliases.
    pub floors: Option<Vec<String>>,
}

/// A prefix tree of normalized phrases.
#[derive(Clone, Debug, Default)]
pub(crate) struct PhraseTrie {
    nodes: Vec<TrieNode>,
}

#[derive(Clone, Debug, Default)]
struct TrieNode {
    children: Vec<(String, u32)>,
    terminal: bool,
}

impl PhraseTrie {
    pub const ROOT: u32 = 0;

    fn new<'a>(phrases: impl IntoIterator<Item = &'a String>) -> Self {
        let mut trie = Self {
            nodes: vec![TrieNode::default()],
        };
        for phrase in phrases {
            let tokens = normalize(phrase).into_tokens();
            if tokens.is_empty() {
                continue;
            }
            let mut node = Self::ROOT;
            for token in tokens {
                node = match trie.child(node, &token) {
                    Some(next) => next,
                    None => {
                        trie.nodes.push(TrieNode::default());
                        let next = (trie.nodes.len() - 1) as u32;
                        trie.nodes[node as usize].children.push((token, next));
                        next
                    }
                };
            }
            trie.nodes[node as usize].terminal = true;
        }
        trie
    }

    pub fn child(&self, node: u32, token: &str) -> Option<u32> {
        self.nodes[node as usize]
            .children
            .iter()
            .find(|(t, _)| t == token)
            .map(|(_, n)| *n)
    }

    pub fn terminal(&self, node: u32) -> bool {
        self.nodes[node as usize].terminal
    }

    pub fn has_children(&self, node: u32) -> bool {
        !self.nodes[node as usize].children.is_empty()
    }
}

/// The vocabulary as the matcher reads it.
#[derive(Clone, Debug, Default)]
pub(crate) struct VocabIndex {
    names: Option<PhraseTrie>,
    areas: Option<PhraseTrie>,
    floors: Option<PhraseTrie>,
    /// Every name, area and floor together: what "turn on …" may target. `None` when no
    /// list is known.
    targets: Option<PhraseTrie>,
}

/// How a run of words stands to the known targets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TargetPrefix {
    /// The words are a whole target.
    pub whole: bool,
    /// A longer target starts with them.
    pub longer: bool,
}

impl VocabIndex {
    pub fn new(vocabulary: &Vocabulary) -> Self {
        let lists = [&vocabulary.names, &vocabulary.areas, &vocabulary.floors];
        Self {
            names: vocabulary.names.as_ref().map(PhraseTrie::new),
            areas: vocabulary.areas.as_ref().map(PhraseTrie::new),
            floors: vocabulary.floors.as_ref().map(PhraseTrie::new),
            targets: lists
                .iter()
                .any(|l| l.is_some())
                .then(|| PhraseTrie::new(lists.into_iter().flatten().flatten())),
        }
    }

    /// The list's phrases; `None` when the list is not known.
    pub fn trie(&self, list: VocabList) -> Option<&PhraseTrie> {
        match list {
            VocabList::Name => self.names.as_ref(),
            VocabList::Area => self.areas.as_ref(),
            VocabList::Floor => self.floors.as_ref(),
        }
    }

    /// Whether any target is known at all.
    pub fn has_targets(&self) -> bool {
        self.targets
            .as_ref()
            .is_some_and(|t| t.has_children(PhraseTrie::ROOT))
    }

    /// How `words` stand to the known targets; `None` when no target starts with them
    /// (or none is known, or `words` is empty).
    pub fn target_prefix(&self, words: &[String]) -> Option<TargetPrefix> {
        let trie = self.targets.as_ref()?;
        if words.is_empty() {
            return None;
        }
        let mut node = PhraseTrie::ROOT;
        for word in words {
            node = trie.child(node, word)?;
        }
        Some(TargetPrefix {
            whole: trie.terminal(node),
            longer: trie.has_children(node),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrases_are_normalized_and_prefixes_kept() {
        let phrases = vec![
            "Kitchen".to_owned(),
            "kitchen lights".to_owned(),
            "TV-2".to_owned(),
        ];
        let trie = PhraseTrie::new(&phrases);
        let kitchen = trie.child(PhraseTrie::ROOT, "kitchen").unwrap();
        assert!(trie.terminal(kitchen));
        assert!(trie.has_children(kitchen));
        let tv = trie.child(PhraseTrie::ROOT, "tv").unwrap();
        assert!(!trie.terminal(tv));
        assert!(trie.terminal(trie.child(tv, "2").unwrap()));
    }
}
