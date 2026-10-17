"""Fuzzy search over what transcript correction knows (SPEC §16.8), for the dashboard.

Pure. Searches a device's saved preferred phrases and terms and the published correction
tables: HA's trigger sentences (a template's expansions collapse into one result), the
trigger templates left to HA's matcher (a {list} or {wildcard}), earlier requests HA
answered, and HA's entity, area and floor names.

Matching is a fuzzy finder's: each query word must appear, as a word prefix, a substring,
or failing those as an in-order subsequence of letters, in any order. Word prefixes rank
above substrings above subsequences, tight subsequences above loose ones, and a text that
begins with the whole query above the rest. This ranks for a person typing; it is not the
similarity correction uses.
"""

from __future__ import annotations

import re
from collections import Counter
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass, field
from typing import Final

from em_text_fix import NameIndex, PhraseLayer, Source, Tables, TargetKind

MAX_RESULTS: Final = 100
MAX_QUERY_CHARS: Final = 120

# Ties after the score: shorter text first, then this order.
_SOURCE_ORDER: Final = {Source.USER_PHRASE: 0, Source.USER_TERM: 1, Source.TRIGGER: 2, Source.HISTORY: 3,
                        Source.ENTITY: 4, Source.AREA: 5, Source.FLOOR: 6}


@dataclass(frozen=True, slots=True)
class Entry:
    text: str                        # as shown
    key: str                         # what is matched: `text` lowercased
    source: Source
    group: str | None = None         # trigger expansions of one template collapse into one result
    template: str | None = None      # a trigger's sentence template
    wildcard: bool = False           # a template HA's matcher handles itself ({list}, {wildcard})
    position: int | None = None      # a user entry's place in its list, 0 = highest preference
    turn_id: int | None = None       # history: the newest turn that sent it
    ts: float | None = None
    ids: tuple[str, ...] = ()        # names: the entity, area or floor ids

    @property
    def kind(self) -> str:
        """The source as the dashboard groups it: a wildcard template is its own kind."""
        return "wildcard" if self.wildcard else str(self.source)


def _entry(text: str, source: Source, *, group: str | None = None, template: str | None = None,
           wildcard: bool = False, position: int | None = None, turn_id: int | None = None,
           ts: float | None = None, ids: tuple[str, ...] = ()) -> Entry:
    return Entry(text, text.lower(), source, group, template, wildcard, position, turn_id, ts, ids)


@dataclass(frozen=True, slots=True)
class Match:
    entry: Entry
    score: float
    spans: tuple[tuple[int, int], ...]   # [start, end) ranges of `entry.text` matched; () when unknown
    variants: int = 1                    # matching expansions of the same trigger template


@dataclass(frozen=True)
class SearchResult:
    matches: tuple[Match, ...]
    total: int                           # matching results before the limit (templates collapsed)
    counts: dict[str, int] = field(default_factory=dict)   # searchable entries by Entry.kind


class _Token:
    """One query word and how it is matched."""
    __slots__ = ("text", "boundary", "subsequence")

    def __init__(self, text: str) -> None:
        self.text = text
        self.boundary = " " + text
        self.subsequence = re.compile(".*?".join(re.escape(c) for c in text))

    def score(self, key: str) -> float | None:
        """0 a word prefix, 1 a substring, 2+ a subsequence (0.05 more for each letter
        skipped inside it); None: absent."""
        if key.startswith(self.text) or self.boundary in key:
            return 0.0
        if self.text in key:
            return 1.0
        m = self.subsequence.search(key)
        if m is None:
            return None
        return 2.0 + 0.05 * (m.end() - m.start() - len(self.text))

    def spans(self, key: str) -> list[tuple[int, int]]:
        """Where it matched, for highlighting."""
        if key.startswith(self.text):
            return [(0, len(self.text))]
        i = key.find(self.boundary)
        if i >= 0:
            return [(i + 1, i + 1 + len(self.text))]
        i = key.find(self.text)
        if i >= 0:
            return [(i, i + len(self.text))]
        m = self.subsequence.search(key)
        if m is None:
            return []
        # The leftmost match, tightened: each letter as late as still fits before the next.
        positions: list[int] = []
        j = m.end()
        for c in reversed(self.text):
            j = key.rfind(c, m.start(), j)
            positions.append(j)
        return [(p, p + 1) for p in reversed(positions)]


def _merge(spans: Iterable[tuple[int, int]]) -> tuple[tuple[int, int], ...]:
    out: list[tuple[int, int]] = []
    for start, end in sorted(spans):
        if out and start <= out[-1][1]:
            out[-1] = (out[-1][0], max(out[-1][1], end))
        else:
            out.append((start, end))
    return tuple(out)


def _layer_entries(layer: PhraseLayer, source: Source) -> tuple[Entry, ...]:
    return tuple(_entry(p.text, source, group=p.template, template=p.template, turn_id=p.turn_id, ts=p.ts)
                 for p in layer.phrases)


def _name_entries(names: NameIndex) -> tuple[Entry, ...]:
    """One entry per name and kind: a name several entities share lists them all."""
    source = {TargetKind.ENTITY: Source.ENTITY, TargetKind.AREA: Source.AREA, TargetKind.FLOOR: Source.FLOOR}
    merged: dict[tuple[TargetKind, str], tuple[str, set[str]]] = {}
    for n in names.names:
        if n.target.kind in source:
            merged.setdefault((n.target.kind, n.text.casefold()), (n.text, set()))[1].update(n.target.ids)
    return tuple(_entry(text, source[kind], ids=tuple(sorted(ids))) for (kind, _), (text, ids) in merged.items())


class PhraseSearch:
    """Searches the published Tables. Each part's entries are built on first use and kept
    while that part is the one published, so a history update leaves the trigger entries
    alone. A query that only extends the previous one (each word lengthened, words added)
    is matched among the previous query's matches: typing ahead never rescans everything.
    Concurrent calls at worst build a part twice or miss the narrowing."""

    def __init__(self) -> None:
        self._cache: dict[str, tuple[object, tuple[Entry, ...]]] = {}
        self._combined: tuple[tuple[str, ...], tuple[str, ...], tuple[object, ...], tuple[Entry, ...],
                              dict[str, int]] | None = None
        self._last: tuple[tuple[Entry, ...], tuple[str, ...], list[int]] | None = None

    def _part(self, name: str, owner: object, build: Callable[[], tuple[Entry, ...]]) -> tuple[Entry, ...]:
        held = self._cache.get(name)
        if held is not None and held[0] is owner:
            return held[1]
        entries = build()
        self._cache[name] = (owner, entries)
        return entries

    def entries(self, tables: Tables | None) -> tuple[Entry, ...]:
        if tables is None:
            return ()
        return (self._part("triggers", tables.triggers, lambda: _layer_entries(tables.triggers, Source.TRIGGER))
                + self._part("wildcards", tables.skipped_templates, lambda: tuple(
                    _entry(t, Source.TRIGGER, template=t, wildcard=True) for t in tables.skipped_templates))
                + self._part("history", tables.history, lambda: _layer_entries(tables.history, Source.HISTORY))
                + self._part("names", tables.names, lambda: _name_entries(tables.names)))

    def _all(self, tables: Tables | None, terms: Sequence[str],
             phrases: Sequence[str]) -> tuple[tuple[Entry, ...], dict[str, int]]:
        """The user's entries then the tables', and their counts by kind: one tuple while
        neither the lists (by value) nor the tables' parts (by identity) change."""
        user = (tuple(terms), tuple(phrases))
        parts: tuple[object, ...] = () if tables is None else (
            tables.triggers, tables.skipped_templates, tables.history, tables.names)
        held = self._combined
        if (held is not None and held[:2] == user and len(held[2]) == len(parts)
                and all(a is b for a, b in zip(held[2], parts))):
            return held[3], held[4]
        entries = (tuple(_entry(t, Source.USER_PHRASE, position=i) for i, t in enumerate(phrases))
                   + tuple(_entry(t, Source.USER_TERM, position=i) for i, t in enumerate(terms))
                   + self.entries(tables))
        counts = dict(Counter(e.kind for e in entries))
        self._combined = (*user, parts, entries, counts)
        return entries, counts

    def search(self, tables: Tables | None, query: str, terms: Sequence[str], phrases: Sequence[str],
               limit: int = 50) -> SearchResult:
        """The best `limit` results for `query` among the saved lists and `tables` (None:
        not ready, the lists only). An empty query matches nothing but still counts."""
        entries, counts = self._all(tables, terms, phrases)
        words = tuple(dict.fromkeys(query[:MAX_QUERY_CHARS].lower().split()))
        if not words:
            return SearchResult((), 0, counts)
        tokens = [_Token(w) for w in words]
        whole = " ".join(query.lower().split())
        last = self._last
        candidates: Iterable[int] = range(len(entries))
        if (last is not None and last[0] is entries and len(last[1]) <= len(words)
                and all(new.startswith(old) for old, new in zip(last[1], words))):
            candidates = last[2]
        hits: list[int] = []
        loose: list[tuple[tuple[float, int, int, str], int]] = []
        best: dict[str, tuple[tuple[float, int, int, str], int]] = {}   # template → its best expansion
        variants: Counter[str] = Counter()
        for i in candidates:
            e = entries[i]
            total = 0.0
            for token in tokens:
                s = token.score(e.key)
                if s is None:
                    break
                total += s
            else:
                hits.append(i)
                if e.key.startswith(whole):
                    total -= 1.0 if e.key == whole else 0.5
                rank = (total, len(e.key), _SOURCE_ORDER.get(e.source, 9), e.key)
                if e.group is None:
                    loose.append((rank, i))
                    continue
                variants[e.group] += 1
                held = best.get(e.group)
                if held is None or rank < held[0]:
                    best[e.group] = (rank, i)
        self._last = (entries, words, hits)
        order = sorted(loose + list(best.values()))
        matches = []
        for rank, i in order[:max(0, min(limit, MAX_RESULTS))]:
            e = entries[i]
            spans = _merge(s for t in tokens for s in t.spans(e.key)) if len(e.key) == len(e.text) else ()
            matches.append(Match(e, round(rank[0], 3), spans, variants[e.group] if e.group is not None else 1))
        return SearchResult(tuple(matches), len(order), dict(counts))
