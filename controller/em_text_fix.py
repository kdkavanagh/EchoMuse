"""Transcript correction (SPEC §16.8): repair a misheard command before HA sees it.

Pure: candidate tables, the candidate rules, semantic preservation, ambiguity, and user
preference. No I/O; the HA calls and the table lifecycle are `em_text_fix_service`'s, the
child process every computation here runs in `em_text_fix_worker`'s, the turn plumbing
`em_session`'s.

A correction is proposed only for text HA's sentence matcher does not already understand,
from six rules (`Rule`), and is applied only when HA's matcher fully accepts it under the
turn's own context, it preserves everything the original said explicitly (negation,
polarity, actions, numbers, units, named targets, and every slot HA had resolved), and
no competing interpretation comes within AMBIGUITY_MARGIN of it.

The rules that ask what a misheard text was meant to say (slot, name, lead-in, phrase)
compare pronunciations (em_phonetics): CMUdict and the user's pronunciation guides, by
phoneme edit distance weighted by articulatory features. Text with a word that has no
pronunciation is never compared by sound. Similarity is a retrieval and scoring
heuristic, not a probability that the correction is what was meant.

Every rule reads only the canonical text (`fix_key`): lowercase, edge punctuation trimmed,
whitespace collapsed, exactly what the endpoint's stable prefix is. So a correction
computed at a pause holds for a final transcript with the same key.
"""

from __future__ import annotations

import bisect
import hashlib
import itertools
import json
import re
from collections import Counter
from collections.abc import Iterable, Iterator, Mapping, Sequence
from dataclasses import dataclass, field, replace
from enum import StrEnum
from functools import lru_cache
from typing import Final

import numpy as np
import numpy.typing as npt

from em_ha_client import SentenceRecognition, Vocabulary
from em_phonetics import Lexicon, PhoneticLayer, Pronounced, best_similarity, parse_guides, tokens
from em_wake_phrase import normalize_command

# ── Configuration ────────────────────────────────────────────────────────────

MODE_KEY: Final = "transcriptFix"
TERMS_KEY: Final = "transcriptFixTerms"
PHRASES_KEY: Final = "transcriptFixPhrases"
PRONUNCIATIONS_KEY: Final = "transcriptFixPronunciations"
MAX_LIST_ENTRIES: Final = 200
MAX_ENTRY_CHARS: Final = 120


class TextFixMode(StrEnum):
    """`transcriptFix`. `off` does no work; `shadow` records what `on` would have done and
    sends the original without waiting; `on` sends an eligible, validated correction."""
    OFF = "off"
    SHADOW = "shadow"
    ON = "on"


def parse_mode(raw: object) -> TextFixMode:
    """The `transcriptFix` value; ValueError names what is wrong."""
    if not isinstance(raw, str) or raw not in {m.value for m in TextFixMode}:
        raise ValueError(f"must be one of {', '.join(m.value for m in TextFixMode)}")
    return TextFixMode(raw)


def match_key(entry: str) -> str:
    """The matching normalization of a vocabulary entry: its letters and digits, casefolded.
    Two entries with the same key are duplicates ("YouTube TV", "youtube t.v.")."""
    return letters(entry)


def parse_list(raw: object) -> tuple[str, ...]:
    """A `transcriptFixTerms`/`transcriptFixPhrases` value: an ordered list of entries,
    highest preference first. Display case is kept, surrounding whitespace is not.
    ValueError names what is wrong."""
    if not isinstance(raw, list):
        raise ValueError("must be a list of strings")
    if len(raw) > MAX_LIST_ENTRIES:
        raise ValueError(f"at most {MAX_LIST_ENTRIES} entries")
    out: list[str] = []
    seen: dict[str, str] = {}
    for item in raw:
        if not isinstance(item, str):
            raise ValueError("must be a list of strings")
        entry = " ".join(item.split())
        if not match_key(entry):
            raise ValueError("entries must contain a letter or digit")
        if len(entry) > MAX_ENTRY_CHARS:
            raise ValueError(f"entries are at most {MAX_ENTRY_CHARS} characters")
        key = match_key(entry)
        if key in seen:
            raise ValueError(f"{entry!r} duplicates {seen[key]!r}")
        seen[key] = entry
        out.append(entry)
    return tuple(out)


@dataclass(frozen=True, slots=True)
class FixConfig:
    """One device's effective correction settings. `pronunciations`: (word, ARPAbet) guides."""
    mode: TextFixMode = TextFixMode.OFF
    terms: tuple[str, ...] = ()
    phrases: tuple[str, ...] = ()
    pronunciations: tuple[tuple[str, str], ...] = ()
    lexicon: Lexicon = field(init=False, compare=False, repr=False)

    def __post_init__(self) -> None:
        object.__setattr__(self, "lexicon", Lexicon(self.pronunciations))

    @property
    def digest(self) -> str:
        """Identifies these settings in a result's context and its record."""
        blob = json.dumps([self.mode, self.terms, self.phrases, self.pronunciations], separators=(",", ":"))
        return hashlib.sha256(blob.encode()).hexdigest()[:12]


def fix_config(cfg: Mapping[str, object]) -> FixConfig:
    """The settings in an effective device config. The API validates every write, so a bad
    stored value means a hand-edited database: ValueError, and the caller turns it off."""
    raw_mode = cfg.get(MODE_KEY, TextFixMode.OFF.value)
    return FixConfig(parse_mode(raw_mode), parse_list(cfg.get(TERMS_KEY, [])),
                     parse_list(cfg.get(PHRASES_KEY, [])), parse_guides(cfg.get(PRONUNCIATIONS_KEY, {})))


# ── Text features ────────────────────────────────────────────────────────────

_NOT_LETTER = re.compile(r"[^a-z0-9]")


def fix_key(text: str) -> str:
    """The canonical text every rule reads (normalize_command), and the key a result is
    reused by: lowercase, edge punctuation trimmed, whitespace collapsed."""
    return normalize_command(text)


def letters(text: str) -> str:
    return _NOT_LETTER.sub("", text.casefold())


# ── Protected content ────────────────────────────────────────────────────────

NEGATIONS: Final = frozenset({
    "not", "no", "never", "don't", "dont", "doesn't", "doesnt", "didn't", "didnt", "won't", "wont",
    "can't", "cant", "cannot", "isn't", "isnt", "aren't", "arent", "shouldn't", "shouldnt", "without",
    "nothing", "nobody", "none", "neither", "nor", "nowhere",
})
# Direction and state words: never added, never dropped.
POLARITY: Final = frozenset({
    "on", "off", "up", "down", "out", "higher", "louder", "quieter", "softer", "brighter", "dimmer",
    "warmer", "cooler", "colder", "hotter", "more", "less", "next", "previous", "back", "forward",
    "opened", "closed", "locked", "unlocked", "armed", "disarmed", "enabled", "disabled", "muted", "unmuted",
})
# Command verbs: an original's verbs are all kept; a full phrase may supply one only where
# the original has none at all (a garbled command word).
VERBS: Final = frozenset({
    "turn", "switch", "set", "play", "pause", "stop", "start", "resume", "unpause", "skip", "watch", "open",
    "close", "lock", "unlock", "arm", "disarm", "increase", "decrease", "raise", "lower", "dim", "brighten",
    "mute", "unmute", "cancel", "add", "remove", "delete", "clear", "say", "tell", "repeat", "show",
    "enable", "disable", "activate", "deactivate", "toggle", "run", "put", "change", "make", "call", "find",
    "read", "snooze", "dismiss", "shuffle", "rewind", "listen", "announce", "broadcast", "ask", "give",
    "get", "check", "send", "remind", "wake", "go", "launch", "begin", "end", "finish", "kill", "shut",
})
UNITS: Final = frozenset({
    "%", "percent", "degree", "degrees", "minute", "minutes", "min", "mins", "hour", "hours", "hr", "hrs",
    "second", "seconds", "sec", "secs", "day", "days", "week", "weeks", "month", "months", "celsius",
    "fahrenheit", "kelvin", "am", "pm", "times",
})
_NUMBER_WORDS: Final = {
    "zero": 0, "one": 1, "two": 2, "three": 3, "four": 4, "five": 5, "six": 6, "seven": 7, "eight": 8,
    "nine": 9, "ten": 10, "eleven": 11, "twelve": 12, "thirteen": 13, "fourteen": 14, "fifteen": 15,
    "sixteen": 16, "seventeen": 17, "eighteen": 18, "nineteen": 19, "twenty": 20, "thirty": 30,
    "forty": 40, "fifty": 50, "sixty": 60, "seventy": 70, "eighty": 80, "ninety": 90,
    "first": 1, "second": 2, "third": 3, "fourth": 4, "fifth": 5, "sixth": 6, "seventh": 7, "eighth": 8,
    "ninth": 9, "tenth": 10, "half": 50, "quarter": 25, "hundred": 100, "thousand": 1000,
}
_TENS = {"twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"}
_ORDINAL = re.compile(r"(\d+)(?:st|nd|rd|th)")
STOPWORDS: Final = frozenset({
    "the", "a", "an", "to", "is", "it", "of", "in", "on", "at", "for", "and", "my", "me", "be", "are", "what",
    "what's", "whats", "i", "you", "this", "that", "please", "can", "could", "would", "all", "some",
})
# HA intent slots that name something structured; any other slot (search_query, item,
# message, …) takes whatever words are left: a catch-all.
STRUCTURED_SLOTS: Final = frozenset({
    "name", "area", "floor", "domain", "device_class", "state", "brightness", "temperature",
    "temperature_unit", "position", "volume", "volume_level", "percentage", "color", "hours", "minutes",
    "seconds", "start_hours", "start_minutes", "start_seconds",
})
# Intents whose name slot is itself free text.
CATCH_ALL_INTENTS: Final = frozenset({
    "HassStartTimer", "HassBroadcast", "HassShoppingListAddItem", "HassListAddItem", "HassListCompleteItem",
    "HassMediaSearchAndPlay", "HassRespond",
})
# Slots an original's recognition may have resolved, which a correction must keep.
RESOLVED_SLOTS: Final = ("name", "area", "floor", "domain", "device_class")


def _numbers(words: Sequence[str]) -> Counter[int]:
    out: Counter[int] = Counter()
    i = 0
    while i < len(words):
        w = words[i]
        if w.isdigit():
            out[int(w)] += 1
        elif (m := _ORDINAL.fullmatch(w)) is not None:
            out[int(m.group(1))] += 1
        elif w in _NUMBER_WORDS:
            value = _NUMBER_WORDS[w]
            if w in _TENS and i + 1 < len(words) and _NUMBER_WORDS.get(words[i + 1], 10) < 10:
                value += _NUMBER_WORDS[words[i + 1]]
                i += 1
            out[value] += 1
        i += 1
    return out


def _verb(word: str) -> str | None:
    """The command verb a word is a form of ("watched", "plays", "stopping"), or None."""
    if word in VERBS:
        return word
    for suffix in ("ing", "ed", "es", "s", "d"):
        if word.endswith(suffix) and len(word) > len(suffix) + 1:
            stem = word[:-len(suffix)]
            for base in (stem, stem[:-1] if len(stem) > 2 and stem[-1] == stem[-2] else stem, stem + "e"):
                if base in VERBS:
                    return base
    return None


@dataclass(frozen=True, slots=True)
class _Protected:
    negations: Counter[str]
    verbs: Counter[str]           # by base form
    polarity: Counter[str]
    numbers: Counter[int]
    units: Counter[str]

    @classmethod
    def of(cls, words: Sequence[str]) -> _Protected:
        verbs = (_verb(w) for w in words)
        return cls(Counter(w for w in words if w in NEGATIONS), Counter(v for v in verbs if v is not None),
                   Counter(w for w in words if w in POLARITY), _numbers(words),
                   Counter(w for w in words if w in UNITS))

    def any(self) -> bool:
        return bool(self.negations or self.verbs or self.polarity or self.numbers or self.units)


# ── Rules, sources, statuses ─────────────────────────────────────────────────

class Rule(StrEnum):
    SLOT = "slot"          # an unmatched {name}/{area}/{floor} → an HA name
    NAME = "name"          # a 1–4 word span → a known name or user term
    LEAD_IN = "lead_in"    # leading residual wake material removed
    PHRASE = "phrase"      # the whole command → a known full phrase
    CUTOFF = "cutoff"      # a known phrase's head or tail completed
    NUMBERS = "numbers"    # number words → digits of the same value


class Source(StrEnum):
    TRIGGER = "trigger"
    HISTORY = "history"
    USER_PHRASE = "user_phrase"
    USER_TERM = "user_term"
    ENTITY = "entity"
    AREA = "area"
    FLOOR = "floor"
    WAKE = "wake"
    NUMBERS = "numbers"


class TargetKind(StrEnum):
    ENTITY = "entity"
    AREA = "area"
    FLOOR = "floor"
    TERM = "term"


class Status(StrEnum):
    """One candidate's fate."""
    OK = "ok"                              # eligible
    NOT_PRESERVED = "not_preserved"        # drops or changes explicit content (before HA)
    NOT_VALIDATED = "not_validated"        # eligible so far, past the validation limit
    NO_MATCH = "no_match"                  # HA's matcher does not fully accept it
    CATCH_ALL = "catch_all"                # accepted only by a free-text slot or trigger
    UNRESOLVED = "unresolved"              # the inserted name did not resolve to its target
    CHANGED_RESOLVED = "changed_resolved"  # a slot HA had resolved changed, or cannot be shown kept


class Reason(StrEnum):
    """Why the turn sent what it sent (turns.stt_fix `reason`)."""
    APPLIED = "applied"                    # on: the correction was sent
    WOULD_APPLY = "would_apply"            # shadow: on would have sent it
    ORIGINAL_MATCHED = "original_matched"  # HA's matcher understands the original
    NO_CANDIDATE = "no_candidate"
    NO_VALID_CANDIDATE = "no_valid_candidate"
    AMBIGUOUS = "ambiguous"
    CONTEXTUAL = "contextual"              # a follow-up in a conversation or reply window
    NOT_READY = "not_ready"                # tables not built, HA's matcher unavailable
    TIMEOUT = "timeout"                    # on: past FINAL_DEADLINE_S
    ERROR = "error"
    STALE = "stale"                        # computed under a context that no longer holds


# Rule thresholds (§16.8 table): how alike what a candidate replaces sounds to what replaces
# it (em_phonetics), never including preference: leading words and the wake phrase's end;
# for a slot, a span or a phrase, only the words that differ between the replaced text and
# the replacement (_changed): words in both never dilute it. A slot value is a short name,
# where one segment is a large share ("glow" for "low" is 0.75), and the slot's domain, area
# and floor already narrow its names.
SLOT_MIN: Final = 0.70
NAME_MIN: Final = 0.80
PHRASE_MIN: Final = 0.70
CUTOFF_MIN_COVERAGE: Final = 0.5
LEAD_IN_MIN: Final = 0.75
MIN_TAIL_PHONEMES: Final = 3      # the shortest end of the wake phrase a lead-in may be (segments)
MIN_LETTERS: Final = 4
MAX_LEAD_WORDS: Final = 3
MAX_SPAN_WORDS: Final = 4
VALIDATE_LIMIT: Final = 8
# Frozen with the replay of the 2026-09-27 → 2026-10-17 corpus (tools/text_fix_replay.py):
# the winning interpretation's evidence must lead every other by this much.
AMBIGUITY_MARGIN: Final = 0.05
# User preference: ranks eligible, unambiguous candidates only. An exact user phrase
# outweighs a contained term; higher entries weigh more; the sum is capped.
PHRASE_PREFERENCE: Final = 0.02
TERM_PREFERENCE: Final = 0.01
PREFERENCE_CAP: Final = 0.02
_EPS: Final = 1e-9


@dataclass(frozen=True, slots=True)
class Target:
    """What a slot or name-span candidate inserts: entity ids, an area or floor id, or a
    user term's letters."""
    kind: TargetKind
    ids: frozenset[str]
    name: str


@dataclass(frozen=True, slots=True)
class Candidate:
    text: str
    rule: Rule
    source: Source
    raw: float                       # phonetic similarity (cut-off: of the whole texts, reported only)
    coverage: float | None = None    # cut-off: the share of the phrase's letters heard
    preference: float = 0.0
    target: Target | None = None
    replaced: str | None = None      # the original words replaced or removed
    template: str | None = None      # a trigger phrase's sentence template

    @property
    def evidence(self) -> float:
        """What eligibility and ambiguity read: raw similarity, or a cut-off's coverage."""
        return self.coverage if self.rule == Rule.CUTOFF and self.coverage is not None else self.raw

    @property
    def rank(self) -> float:
        return self.evidence + self.preference


# ── Tables ───────────────────────────────────────────────────────────────────

@dataclass(frozen=True, slots=True)
class Phrase:
    text: str                        # canonical
    letters: str
    source: Source
    template: str | None = None      # trigger
    position: int | None = None      # user phrase: 0 = highest preference
    turn_id: int | None = None       # history: the newest turn that sent it
    ts: float | None = None


@dataclass(frozen=True)
class PhraseLayer:
    """Complete phrases, one per letter string, with their letters sorted forwards and
    reversed for head/tail completion (the cut-off rule). Their pronunciations are searched
    through a PhoneticLayer whose owners are these phrases' positions."""
    phrases: tuple[Phrase, ...]
    _heads: tuple[str, ...]
    _head_ids: tuple[int, ...]
    _tails: tuple[str, ...]
    _tail_ids: tuple[int, ...]

    @classmethod
    def build(cls, phrases: Iterable[Phrase]) -> PhraseLayer:
        """One phrase per letter string, the first given winning."""
        kept: dict[str, Phrase] = {}
        for p in phrases:
            if p.letters and p.letters not in kept:
                kept[p.letters] = p
        items = tuple(kept.values())
        heads = sorted((p.letters, i) for i, p in enumerate(items))
        tails = sorted((p.letters[::-1], i) for i, p in enumerate(items))
        return cls(items, tuple(h for h, _ in heads), tuple(i for _, i in heads),
                   tuple(t for t, _ in tails), tuple(i for _, i in tails))

    def __len__(self) -> int:
        return len(self.phrases)

    def completions(self, letter_key: str, min_coverage: float) -> Iterator[int]:
        """Phrases whose letters start or end with `letter_key` and are longer, covering at
        least `min_coverage` of their letters."""
        limit = len(letter_key) / max(min_coverage, _EPS)
        for keys, ids, probe in ((self._heads, self._head_ids, letter_key),
                                 (self._tails, self._tail_ids, letter_key[::-1])):
            i = bisect.bisect_left(keys, probe)
            while i < len(keys) and keys[i].startswith(probe):
                if len(probe) < len(keys[i]) <= limit + _EPS:
                    yield ids[i]
                i += 1


def _phrase(text: str, source: Source, *, template: str | None = None, position: int | None = None,
            turn_id: int | None = None, ts: float | None = None) -> Phrase | None:
    canonical = fix_key(text)
    key = letters(canonical)
    if not key:
        return None
    return Phrase(canonical, key, source, template, position, turn_id, ts)


def expand_triggers(templates: Sequence[str], *, per_template: int = 5_000,
                    total: int = 100_000) -> tuple[list[Phrase], list[str]]:
    """Every sentence each trigger template can say (hassil), as TRIGGER phrases; and the
    templates not expanded: those with a {list} or {wildcard}, which take any words and are
    left to HA's matcher, and any hassil cannot parse."""
    from hassil.expression import Sentence
    from hassil.parse_expression import parse_sentence
    from hassil.sample import sample_expression

    phrases: list[Phrase] = []
    skipped: list[str] = []
    for template in templates:
        try:
            sentence: Sentence = parse_sentence(template)
            if next(iter(sentence.list_names()), None) is not None:
                skipped.append(template)
                continue
            texts = list(itertools.islice(sample_expression(sentence.expression), per_template))
        except Exception:   # hassil raises its own ParseError, MissingRuleError, …
            skipped.append(template)
            continue
        for text in texts:
            p = _phrase(text, Source.TRIGGER, template=template)
            if p is not None:
                phrases.append(p)
            if len(phrases) >= total:
                return phrases, skipped
    return phrases, skipped


@dataclass(frozen=True, slots=True)
class HistoryEntry:
    """A request HA's own agent answered without error, as sent: its text, and the newest
    turn that sent it."""
    text: str
    turn_id: int
    ts: float


def history_eligible(outcome: object, intent_local: object, response_type: object) -> bool:
    """A turn whose sent text joins the history: HA's built-in agent answered it (not
    merely processed it locally), with no error. The em_db loader filters the same way."""
    return (outcome == "ha" and intent_local in (True, 1) and isinstance(response_type, str)
            and bool(response_type) and response_type != "error")


def sent_text(stt_text: str, stt_fix: str | None) -> str:
    """What a turn sent HA: the applied correction, else the transcript."""
    if stt_fix:
        try:
            record = json.loads(stt_fix)
        except ValueError:
            return stt_text
        if isinstance(record, dict) and record.get("applied") is True and isinstance(record.get("sent"), str):
            return str(record["sent"])
    return stt_text


def build_history(entries: Iterable[HistoryEntry]) -> PhraseLayer:
    """Known requests, deduplicated (newest provenance kept); repetition earns nothing."""
    newest: dict[str, tuple[HistoryEntry, Phrase]] = {}
    for e in entries:
        p = _phrase(e.text, Source.HISTORY, turn_id=e.turn_id, ts=e.ts)
        if p is None:
            continue
        held = newest.get(p.letters)
        if held is None or e.ts > held[0].ts:
            newest[p.letters] = (e, p)
    return PhraseLayer.build(p for _, p in sorted(newest.values(), key=lambda x: -x[0].ts))


@dataclass(frozen=True, slots=True)
class Name:
    text: str
    target: Target
    domain: str | None = None            # entity
    area_ids: frozenset[str] = frozenset()


@dataclass(frozen=True)
class Tables:
    """An immutable snapshot of everything a correction is looked up in, revisioned by the
    service that published it."""
    revision: int
    triggers: PhraseLayer
    history: PhraseLayer
    names: tuple[Name, ...]
    name_words: frozenset[str]                 # every word of an HA name (≥ 3 letters, not a stopword)
    entity_ids_by_name: Mapping[str, frozenset[str]]
    area_id_by_name: Mapping[str, str]
    floor_id_by_name: Mapping[str, str]
    entity_area: Mapping[str, str | None]
    area_floor: Mapping[str, str | None]
    skipped_templates: tuple[str, ...] = ()

    @property
    def summary(self) -> dict[str, int]:
        return {"revision": self.revision, "triggers": len(self.triggers), "history": len(self.history),
                "names": len(self.names), "catch_all_templates": len(self.skipped_templates)}


def build_tables(vocabulary: Vocabulary, templates: Sequence[str], history: Iterable[HistoryEntry],
                 revision: int) -> Tables:
    """The snapshot for one revision: CPU work, run in em_text_fix_worker's child process."""
    triggers, skipped = expand_triggers(templates)
    entity_ids: dict[str, set[str]] = {}
    area_ids: dict[str, str] = {}
    floor_ids: dict[str, str] = {}
    names: list[Name] = []
    area_floor = {a.area_id: a.floor_id for a in vocabulary.areas}
    floor_areas: dict[str, set[str]] = {}
    for a in vocabulary.areas:
        if a.floor_id is not None:
            floor_areas.setdefault(a.floor_id, set()).add(a.area_id)
    for e in vocabulary.entities:
        for n in e.names:
            entity_ids.setdefault(n.casefold(), set()).add(e.entity_id)
    for e in vocabulary.entities:
        for n in e.names:
            ids = frozenset(entity_ids[n.casefold()])
            names.append(Name(n, Target(TargetKind.ENTITY, ids, n), e.domain,
                              frozenset({e.area_id}) if e.area_id else frozenset()))
    for a in vocabulary.areas:
        for n in a.names:
            area_ids.setdefault(n.casefold(), a.area_id)
            names.append(Name(n, Target(TargetKind.AREA, frozenset({a.area_id}), n), None, frozenset({a.area_id})))
    for f in vocabulary.floors:
        for n in f.names:
            floor_ids.setdefault(n.casefold(), f.floor_id)
            names.append(Name(n, Target(TargetKind.FLOOR, frozenset({f.floor_id}), n), None,
                              frozenset(floor_areas.get(f.floor_id, ()))))
    names = [n for n in names if letters(n.text)]
    words = {w for n in names for w in tokens(n.text) if len(w) >= 3 and w not in STOPWORDS}
    return Tables(
        revision=revision, triggers=PhraseLayer.build(triggers), history=build_history(history),
        names=tuple(names), name_words=frozenset(words),
        entity_ids_by_name={k: frozenset(v) for k, v in entity_ids.items()},
        area_id_by_name=area_ids, floor_id_by_name=floor_ids,
        entity_area={e.entity_id: e.area_id for e in vocabulary.entities}, area_floor=area_floor,
        skipped_templates=tuple(skipped),
    )


@dataclass(frozen=True)
class Sounds:
    """Texts' pronunciations (owners in order) and, for the phrase rule's anchor, the texts
    each content word (not a stopword) occurs in, by spelling and by each pronunciation."""
    layer: PhoneticLayer
    anchors: Mapping[str | bytes, npt.NDArray[np.intp]]
    count: int
    unknown: Counter[str]                # words with no pronunciation, by how many texts use them

    @classmethod
    def of(cls, lexicon: Lexicon, texts: Sequence[str]) -> Sounds:
        variants: list[tuple[bytes, ...]] = []
        unknown: Counter[str] = Counter()
        index: dict[str | bytes, list[int]] = {}
        for owner, text in enumerate(texts):
            said = lexicon.pronounce(text)
            variants.append(said.variants())
            unknown.update(said.unknown)
            for key in _anchor_keys(said):
                index.setdefault(key, []).append(owner)
        return cls(PhoneticLayer.build(variants), {k: np.unique(np.array(v, dtype=np.intp)) for k, v in index.items()},
                   len(texts), unknown)

    def anchored(self, said: Pronounced) -> npt.NDArray[np.bool_] | None:
        """The texts sharing a content word with `said`, as spelled or as pronounced; None
        (every text) when `said` has at most one content word, where nothing could anchor."""
        keys = _anchor_keys(said)
        if len({k for k in keys if isinstance(k, str)}) <= 1:
            return None
        allowed = np.zeros(self.count, dtype=np.bool_)
        for key in keys:
            if (owners := self.anchors.get(key)) is not None:
                allowed[owners] = True
        return allowed


def _anchor_keys(said: Pronounced) -> list[str | bytes]:
    keys: list[str | bytes] = []
    for word, found in zip(said.words, said.found):
        if word not in STOPWORDS:
            keys.append(word)
            keys.extend(found.alternatives if found is not None else ())
    return keys


@dataclass(frozen=True)
class Phonetic:
    """The tables' phrases and names as pronunciations under one lexicon, for the rules
    that compare by sound. Owners index tables.triggers.phrases, tables.history.phrases and
    tables.names."""
    lexicon: Lexicon
    triggers: Sounds
    history: Sounds
    names: Sounds
    sources: tuple[PhraseLayer, PhraseLayer, tuple[Name, ...]]

    @property
    def unknown(self) -> Counter[str]:
        """Words of HA's trigger sentences, earlier requests and names with no pronunciation,
        by how many of those texts use them."""
        return self.triggers.unknown + self.history.unknown + self.names.unknown

    def compiled_from(self, tables: Tables, lexicon: Lexicon) -> bool:
        return self.lexicon == lexicon and all(a is b for a, b in zip(self.sources, _sources(tables)))


def _sources(tables: Tables) -> tuple[PhraseLayer, PhraseLayer, tuple[Name, ...]]:
    return (tables.triggers, tables.history, tables.names)


def compile_phonetic(tables: Tables, lexicon: Lexicon, previous: Phonetic | None = None) -> Phonetic:
    """`tables` pronounced under `lexicon`: CPU work, run in the child process. A part whose
    source and lexicon are unchanged from `previous` is reused (a history update recompiles
    only the history)."""
    sources = _sources(tables)
    texts = ([p.text for p in tables.triggers.phrases], [p.text for p in tables.history.phrases],
             [n.text for n in tables.names])
    reuse = previous if previous is not None and previous.lexicon == lexicon else None
    held = (reuse.triggers, reuse.history, reuse.names) if reuse is not None else None
    parts = [held[k] if reuse is not None and held is not None and reuse.sources[k] is sources[k]
             else Sounds.of(lexicon, texts[k]) for k in range(3)]
    return Phonetic(lexicon, parts[0], parts[1], parts[2], sources)


@dataclass(frozen=True)
class UserVocabulary:
    """The ordered `transcriptFixTerms` and `transcriptFixPhrases`, highest preference first,
    and their pronunciations (owners index term_names and phrase_layer.phrases)."""
    terms: tuple[str, ...]
    phrases: tuple[str, ...]
    term_names: tuple[Name, ...]
    phrase_layer: PhraseLayer
    term_sounds: Sounds
    phrase_sounds: Sounds

    def preference(self, text: str) -> float:
        """The capped preference of a candidate's text: an exact user phrase by position, plus
        the best-placed user term it contains. Repeats add nothing."""
        key = letters(text)
        bonus = 0.0
        n = len(self.phrases)
        for i, p in enumerate(self.phrases):
            if letters(p) == key:
                bonus += PHRASE_PREFERENCE * (n - i) / n
                break
        if self.terms:
            spans = _span_letters(tokens(text))
            m = len(self.terms)
            for i, t in enumerate(self.terms):
                if letters(t) in spans:
                    bonus += TERM_PREFERENCE * (m - i) / m
                    break
        return min(PREFERENCE_CAP, bonus)


def _span_letters(words: Sequence[str], longest: int = 6) -> set[str]:
    return {"".join(words[i:j]) for i in range(len(words)) for j in range(i + 1, min(len(words), i + longest) + 1)}


@lru_cache(maxsize=16)
def user_vocabulary(terms: tuple[str, ...], phrases: tuple[str, ...], lexicon: Lexicon) -> UserVocabulary:
    term_names = tuple(Name(t, Target(TargetKind.TERM, frozenset({letters(t)}), t)) for t in terms if letters(t))
    layer = PhraseLayer.build(p for i, text in enumerate(phrases)
                              if (p := _phrase(text, Source.USER_PHRASE, position=i)) is not None)
    return UserVocabulary(terms, phrases, term_names, layer, Sounds.of(lexicon, [n.text for n in term_names]),
                          Sounds.of(lexicon, [p.text for p in layer.phrases]))


# ── Proposal ─────────────────────────────────────────────────────────────────

@dataclass(frozen=True, slots=True)
class Request:
    """What to correct: the canonical text (`fix_key`) and, for a wake turn, the wake
    phrase whose residue a lead-in may be."""
    text: str
    wake_phrase: str | None = None


@dataclass(frozen=True, slots=True)
class Rejected:
    candidate: Candidate
    status: Status
    detail: str


@dataclass(frozen=True)
class Proposal:
    text: str
    validate: tuple[Candidate, ...]      # sent to HA's matcher, best first
    held: tuple[Candidate, ...]          # eligible so far, past VALIDATE_LIMIT
    rejected: tuple[Rejected, ...]       # failed preservation before HA saw them


_NUMBER_RUN = re.compile(
    r"\b(?:(twenty|thirty|forty|fifty|sixty|seventy|eighty|ninety)(?:[ -](one|two|three|four|five|six|seven|"
    r"eight|nine))?|(zero|one|two|three|four|five|six|seven|eight|nine|ten|eleven|twelve|thirteen|fourteen|"
    r"fifteen|sixteen|seventeen|eighteen|nineteen))\b")


def _digits(text: str) -> str:
    def value(m: re.Match[str]) -> str:
        if m.group(3) is not None:
            return str(_NUMBER_WORDS[m.group(3)])
        return str(_NUMBER_WORDS[m.group(1)] + (_NUMBER_WORDS[m.group(2)] if m.group(2) else 0))
    return _NUMBER_RUN.sub(value, text)


def _replace_words(text: str, words: str, new: str) -> str | None:
    m = re.search(r"(?<![\w'])" + r"\s+".join(re.escape(w) for w in words.split()) + r"(?![\w'])", text)
    return None if m is None else text[:m.start()] + new + text[m.end():]


def _changed(heard: Sequence[str], phrase: Sequence[str], lexicon: Lexicon) -> tuple[str, str]:
    """The words of `heard` not in `phrase` and those of `phrase` not in `heard` (each as
    often as the other lacks it, in order), each side's joined: what a candidate replaces,
    and with what. A word in both is never changed, wherever it stands ("turn off the tv"
    and "turn the tv off" change nothing), and two words are the same when spelled alike or
    sharing a pronunciation (CMUdict says "tv" as "television" too)."""
    groups: list[set[bytes]] = []
    ids: dict[str, int] = {}

    def ident(word: str) -> int:
        if word not in ids:
            found = lexicon.word(word)
            said = set(found.alternatives) if found is not None else set()
            ids[word] = next((i for i, g in enumerate(groups) if said & g), len(groups))
            if ids[word] == len(groups):
                groups.append(said)
            else:
                groups[ids[word]] |= said
        return ids[word]

    mine, theirs = [ident(w) for w in heard], [ident(w) for w in phrase]
    shared = Counter(mine) & Counter(theirs)

    def rest(words: Sequence[str], keys: Sequence[int]) -> str:
        left = Counter(shared)
        out = []
        for word, key in zip(words, keys):
            if left[key] > 0:
                left[key] -= 1
            else:
                out.append(word)
        return " ".join(out)

    return rest(heard, mine), rest(phrase, theirs)


class _Proposer:
    def __init__(self, request: Request, original: SentenceRecognition | None, tables: Tables,
                 phonetic: Phonetic, user: UserVocabulary) -> None:
        self.text = request.text
        self.request = request
        self.original = original
        self.tables = tables
        self.phonetic = phonetic
        self.user = user
        self.words = self.text.split()
        self.out: list[Candidate] = []

    def sounds(self, text: str) -> tuple[bytes, ...]:
        """`text`'s pronunciations; () when a word of it has none."""
        return self.phonetic.lexicon.pronounce(text).variants()

    def alike(self, replaced: str, replacement: str) -> tuple[float, str]:
        """How alike the words that differ between `replaced` and `replacement` sound, and
        those words of `replaced`: the evidence of every rule that replaces words. Words they
        share do not dilute it ("other tv" and "Apple TV" are "other" and "apple"). 0.0 when
        only one side has words that differ, or one of them has no pronunciation."""
        mine, theirs = _changed(tokens(replaced), tokens(replacement), self.phonetic.lexicon)
        if not mine and not theirs:
            return 1.0, mine
        return (best_similarity(self.sounds(mine), self.sounds(theirs)) if mine and theirs else 0.0), mine

    def run(self) -> list[Candidate]:
        self.numbers()
        self.lead_in()
        self.slots()
        self.spans()
        self.phrases()
        return self.out

    def add(self, c: Candidate) -> None:
        if fix_key(c.text) != self.text:
            self.out.append(c)

    def numbers(self) -> None:
        fixed = _digits(self.text)
        if fixed != self.text:
            self.add(Candidate(fixed, Rule.NUMBERS, Source.NUMBERS, 1.0))

    def lead_in(self) -> None:
        """Leading words that sound like the wake phrase or its end (what is left of a wake
        word whose start the transcript lost) are dropped."""
        wake = self.request.wake_phrase
        if not wake:
            return
        tails = tuple({v[-n:] for v in self.sounds(wake) for n in range(MIN_TAIL_PHONEMES, len(v) + 1)})
        if not tails:
            return
        for k in range(1, min(MAX_LEAD_WORDS, len(self.words) - 1) + 1):
            span = " ".join(self.words[:k])
            rest = " ".join(self.words[k:])
            if len(letters(rest)) < MIN_LETTERS:
                break
            raw = best_similarity(self.sounds(span), tails)
            if raw >= LEAD_IN_MIN - _EPS:
                self.add(Candidate(fix_key(rest), Rule.LEAD_IN, Source.WAKE, raw, replaced=span))

    def slots(self) -> None:
        o = self.original
        if o is None or o.intent is None or not o.unmatched:
            return
        t = self.tables
        domain = o.details["domain"].value if "domain" in o.details else None
        area = t.area_id_by_name.get(o.details["area"].value.casefold()) if "area" in o.details else None
        floor = t.floor_id_by_name.get(o.details["floor"].value.casefold()) if "floor" in o.details else None
        for slot, value in o.unmatched.items():
            heard = " ".join(value.split())
            if slot not in ("name", "area", "floor") or not letters(heard):
                continue
            kind = {"name": TargetKind.ENTITY, "area": TargetKind.AREA, "floor": TargetKind.FLOOR}[slot]

            def fits(n: Name) -> bool:
                if n.target.kind != kind:
                    return False
                if kind == TargetKind.ENTITY and domain is not None and n.domain != domain:
                    return False
                if kind == TargetKind.ENTITY and area is not None and area not in n.area_ids:
                    return False
                return floor is None or kind == TargetKind.FLOOR or any(
                    t.area_floor.get(a) == floor for a in n.area_ids)

            allowed = np.fromiter((fits(n) for n in t.names), dtype=np.bool_, count=len(t.names))
            scored = [(self.alike(heard, t.names[idx].text)[0], t.names[idx])
                      for idx in self.phonetic.names.layer.search(self.sounds(heard), SLOT_MIN, allowed)]
            for s, n in sorted((x for x in scored if x[0] >= SLOT_MIN - _EPS), key=lambda x: (-x[0], x[1].text))[:5]:
                fixed = _replace_words(self.text, fix_key(heard), n.text)
                if fixed is not None:
                    self.add(Candidate(fixed, Rule.SLOT, Source(n.target.kind.value), s, target=n.target,
                                       replaced=fix_key(heard)))

    def spans(self) -> None:
        """A span of 1–4 words → a known name or user term. A span starts and ends with a word
        that is not a stopword (a name's edge is a content word), is not itself a known name
        or term (it was heard right), and leaves some of the command: a name never replaces
        all of it."""
        layers: tuple[tuple[tuple[Name, ...], Sounds, Source | None], ...] = (
            (self.tables.names, self.phonetic.names, None),
            (self.user.term_names, self.user.term_sounds, Source.USER_TERM))
        known = {letters(n.text) for names, _, _ in layers for n in names}
        for i in range(len(self.words)):
            for j in range(i + 1, min(len(self.words), i + MAX_SPAN_WORDS) + 1):
                if j - i == len(self.words):
                    continue
                span = " ".join(self.words[i:j])
                edge = tokens(span)
                if (len(letters(span)) < MIN_LETTERS or not edge or edge[0] in STOPWORDS or edge[-1] in STOPWORDS
                        or letters(span) in known):
                    continue
                said = self.sounds(span)
                if not said:
                    continue
                for names, sounds, source in layers:
                    for idx in sounds.layer.search(said, NAME_MIN):
                        n = names[idx]
                        s = self.alike(span, n.text)[0]
                        if fix_key(n.text) == fix_key(span) or s < NAME_MIN - _EPS:
                            continue
                        fixed = " ".join(self.words[:i] + [n.text] + self.words[j:])
                        self.add(Candidate(fixed, Rule.NAME, source or Source(n.target.kind.value), s,
                                           target=n.target, replaced=span))

    def phrases(self) -> None:
        """The whole command → a known phrase, judged on the words that differ. A phrase must
        share a content word with what was heard (its anchor), unless the heard text has only
        one; the rest must sound alike. Candidates are retrieved at PHRASE_MIN by the
        similarity of the whole texts (PhoneticLayer.search), which is at least that of their
        differing words since the shared ones only add matched segments. A name, slot or span
        is retrieved the same way."""
        key = letters(self.text)
        if len(key) < MIN_LETTERS:
            return
        heard = self.phonetic.lexicon.pronounce(self.text)
        said = heard.variants()
        layers = ((self.user.phrase_layer, self.user.phrase_sounds), (self.tables.triggers, self.phonetic.triggers),
                  (self.tables.history, self.phonetic.history))
        for layer, sounds in layers:
            for idx in (sounds.layer.search(said, PHRASE_MIN, sounds.anchored(heard)) if said else {}):
                p = layer.phrases[idx]
                if p.text == self.text:
                    continue
                raw, mine = self.alike(self.text, p.text)
                if raw >= PHRASE_MIN - _EPS:
                    self.add(Candidate(p.text, Rule.PHRASE, p.source, raw, replaced=mine, template=p.template))
            for idx in layer.completions(key, CUTOFF_MIN_COVERAGE):
                p = layer.phrases[idx]
                self.add(Candidate(p.text, Rule.CUTOFF, p.source, best_similarity(said, self.sounds(p.text)),
                                   coverage=len(key) / len(p.letters), template=p.template))


def _preserved(c: Candidate, original: _Protected, original_tokens: Sequence[str],
               name_words: frozenset[str]) -> str | None:
    """Why `c` fails to preserve the original's explicit content, or None."""
    if c.rule in (Rule.SLOT, Rule.NAME, Rule.LEAD_IN):
        removed = tokens(c.replaced or "")
        if _Protected.of(removed).any():
            return "replaces a protected word"
        if c.rule == Rule.LEAD_IN and any(w in name_words for w in removed):
            return "removes a name word"
        return None
    mine = _Protected.of(tokens(c.text))
    if mine.negations != original.negations:
        return "negation"
    if mine.numbers != original.numbers:
        return "number"
    if mine.units != original.units:
        return "unit"
    if c.rule == Rule.NUMBERS:
        return None
    if original.polarity - mine.polarity or original.verbs - mine.verbs:
        return "drops an action"
    if mine.polarity - original.polarity:
        return "adds a polarity"
    if mine.verbs - original.verbs and (c.rule != Rule.PHRASE or original.verbs):
        return "adds an action"
    kept = set(tokens(c.text))
    for w in original_tokens:
        if w in name_words and w not in kept:
            return f"drops {w!r}"
    return None


def propose(request: Request, original: SentenceRecognition | None, tables: Tables, phonetic: Phonetic,
            user: UserVocabulary) -> Proposal:
    """Candidates for text HA's matcher did not fully accept: preserved ones, best first,
    the first VALIDATE_LIMIT distinct letter strings to validate."""
    text = request.text
    if (original is not None and original.match) or len(letters(text)) < MIN_LETTERS:
        return Proposal(text, (), (), ())
    words = tokens(text)
    protected = _Protected.of(words)
    best: dict[str, Candidate] = {}
    rejected: list[Rejected] = []
    for c in _Proposer(request, original, tables, phonetic, user).run():
        why = _preserved(c, protected, words, tables.name_words)
        if why is not None:
            rejected.append(Rejected(c, Status.NOT_PRESERVED, why))
            continue
        c = replace(c, preference=user.preference(c.text))
        key = fix_key(c.text)
        held = best.get(key)
        if held is None or (c.rank, c.evidence) > (held.rank, held.evidence):
            best[key] = c
    ordered = sorted(best.values(), key=lambda c: (-c.rank, -c.evidence, c.rule, c.text))
    validate: list[Candidate] = []
    held_back: list[Candidate] = []
    seen: set[str] = set()
    for c in ordered:
        k = letters(c.text)
        if len(validate) < VALIDATE_LIMIT and k not in seen:
            validate.append(c)
            seen.add(k)
        else:
            held_back.append(c)
    rejected.sort(key=lambda r: -r.candidate.raw)
    return Proposal(text, tuple(validate), tuple(held_back), tuple(rejected))


# ── Acceptance ───────────────────────────────────────────────────────────────

@dataclass(frozen=True, slots=True)
class Checked:
    candidate: Candidate
    status: Status
    verdict: SentenceRecognition | None = None
    detail: str | None = None
    interpretation: tuple[str, ...] | None = None


@dataclass(frozen=True)
class Decision:
    """`selected`: the correction `on` sends (None: abstain); `proposed`: the best HA
    accepted, eligible or not, for the record."""
    reason: Reason
    selected: Checked | None = None
    proposed: Checked | None = None
    checked: tuple[Checked, ...] = ()
    rejected: tuple[Rejected, ...] = ()
    preference_changed: bool = False


def catch_all(v: SentenceRecognition) -> bool:
    """Matched only through words a free-text slot or trigger wildcard takes."""
    if v.intent is None:
        return "{" in v.template
    return v.intent in CATCH_ALL_INTENTS or any(slot not in STRUCTURED_SLOTS for slot in v.details)


def interpretation(v: SentenceRecognition, text: str, t: Tables) -> tuple[str, ...]:
    """What executing `text` would do, as far as the matcher shows it: spellings with the
    same interpretation (a name or its alias, two splits of one word) are one choice."""
    if v.intent is None:
        return ("trigger", v.template, letters(text)) if catch_all(v) else ("trigger", v.template)
    slots = sorted(f"{k}={_canonical(k, s.value, t)}" for k, s in v.details.items())
    return ("intent", v.intent, *slots, "targets=" + ",".join(v.targets))


def _canonical(slot: str, value: str, t: Tables) -> str:
    v = value.casefold()
    if slot == "area":
        return t.area_id_by_name.get(v, v)
    if slot == "floor":
        return t.floor_id_by_name.get(v, v)
    if slot == "name":
        return ",".join(sorted(t.entity_ids_by_name.get(v, frozenset({v}))))
    return v


def _keeps_resolved(original: SentenceRecognition | None, v: SentenceRecognition, t: Tables) -> str | None:
    """Which slot the original's recognition had resolved that `v` changes or cannot be
    shown to keep, or None."""
    if original is None or original.intent is None:
        return None
    for slot in RESOLVED_SLOTS:
        if slot not in original.details:
            continue
        want = _canonical(slot, original.details[slot].value, t)
        if slot in v.details:
            if _canonical(slot, v.details[slot].value, t) != want:
                return slot
            continue
        if v.intent is None or not v.targets:
            return slot
        if slot == "domain" and all(e.split(".", 1)[0] == want for e in v.targets):
            continue
        if slot == "area" and all(t.entity_area.get(e) == want for e in v.targets):
            continue
        if slot == "floor" and all(t.area_floor.get(t.entity_area.get(e) or "") == want for e in v.targets):
            continue
        if slot == "name" and set(v.targets) <= set(want.split(",")):
            continue
        return slot
    return None


def _resolves_target(c: Candidate, v: SentenceRecognition, t: Tables) -> bool:
    target = c.target
    if target is None:
        return True
    if target.kind == TargetKind.TERM:
        # A term is no HA name: it must be literally what was matched, not a wildcard.
        if v.intent is None:
            return target.name.casefold() in v.template.casefold() or letters(target.name) in letters(v.template)
        return any(letters(s.value) == letters(target.name) for s in v.details.values())
    slot = {TargetKind.ENTITY: "name", TargetKind.AREA: "area", TargetKind.FLOOR: "floor"}[target.kind]
    if slot not in v.details:
        return False
    if _canonical(slot, v.details[slot].value, t) not in {",".join(sorted(target.ids))} | set(target.ids):
        return False
    return target.kind != TargetKind.ENTITY or bool(set(v.targets) & target.ids)


def _check(c: Candidate, v: SentenceRecognition | None, original: SentenceRecognition | None,
           t: Tables) -> Checked:
    if v is None or not v.match:
        return Checked(c, Status.NO_MATCH, v)
    meaning = interpretation(v, c.text, t)
    if catch_all(v) and c.rule not in (Rule.LEAD_IN, Rule.PHRASE, Rule.CUTOFF):
        return Checked(c, Status.CATCH_ALL, v, v.template, meaning)
    if c.rule in (Rule.SLOT, Rule.NAME) and not _resolves_target(c, v, t):
        return Checked(c, Status.UNRESOLVED, v, c.target.name if c.target else None, meaning)
    changed = _keeps_resolved(original, v, t)
    if changed is not None:
        return Checked(c, Status.CHANGED_RESOLVED, v, changed, meaning)
    return Checked(c, Status.OK, v, None, meaning)


def decide(proposal: Proposal, verdicts: Sequence[SentenceRecognition | None],
           original: SentenceRecognition | None, tables: Tables) -> Decision:
    """Apply acceptance (§16.8) to HA's verdict on each validated candidate."""
    if original is not None and original.match:
        return Decision(Reason.ORIGINAL_MATCHED)
    checked = [_check(c, v, original, tables) for c, v in zip(proposal.validate, verdicts)]
    rejected = proposal.rejected
    if not checked:
        return Decision(Reason.NO_CANDIDATE, rejected=rejected)
    groups: dict[tuple[str, ...], list[Checked]] = {}
    for ch in checked:
        if ch.interpretation is not None:
            groups.setdefault(ch.interpretation, []).append(ch)
    if not groups:
        return Decision(Reason.NO_VALID_CANDIDATE, checked=tuple(checked), rejected=rejected)
    ranked = sorted(groups.values(), key=lambda g: (-max(ch.candidate.evidence for ch in g),
                                                    min(ch.candidate.text for ch in g)))
    top = ranked[0]
    lead = max(ch.candidate.evidence for ch in top)
    proposed = max(top, key=lambda ch: (ch.status == Status.OK, ch.candidate.rank, ch.candidate.evidence))
    ambiguous = len(ranked) > 1 and lead - max(ch.candidate.evidence for ch in ranked[1]) < AMBIGUITY_MARGIN - _EPS
    validated = {letters(ch.candidate.text) for ch in checked}
    unverified = any(c.evidence > lead - AMBIGUITY_MARGIN + _EPS and letters(c.text) not in validated
                     for c in proposal.held)
    if ambiguous or unverified:
        return Decision(Reason.AMBIGUOUS, None, proposed, tuple(checked), rejected)
    eligible = [ch for ch in top if ch.status == Status.OK]
    if not eligible:
        return Decision(Reason.NO_VALID_CANDIDATE, None, proposed, tuple(checked), rejected)
    by_rank = max(eligible, key=lambda ch: (ch.candidate.rank, ch.candidate.evidence, ch.candidate.text))
    by_evidence = max(eligible, key=lambda ch: (ch.candidate.evidence, ch.candidate.text))
    return Decision(Reason.APPLIED, by_rank, by_rank, tuple(checked), rejected,
                    preference_changed=by_rank is not by_evidence)


# ── Context and record ───────────────────────────────────────────────────────

@dataclass(frozen=True, slots=True)
class FixContext:
    """Everything a result depends on besides its text. A result is used only while every
    field still holds: the HA connection and table revisions are the service's, the rest
    the turn's."""
    connection: int
    tables: int
    device: str | None
    pipeline: str
    language: str | None
    config: str                   # FixConfig.digest
    wake_phrase: str | None


@dataclass(frozen=True)
class FixOutcome:
    """A correction computed for one canonical text under one context."""
    key: str
    context: FixContext
    decision: Decision
    original: SentenceRecognition | None
    latency_ms: int


def _verdict(v: SentenceRecognition | None) -> dict[str, object] | None:
    if v is None:
        return None
    out: dict[str, object] = {"match": v.match, "source": v.source, "template": v.template}
    if v.intent is not None:
        out["intent"] = v.intent
        out["slots"] = {k: s.value for k, s in v.details.items()}
        if v.unmatched:
            out["unmatched"] = dict(v.unmatched)
    return out


def _candidate(c: Candidate, status: Status, detail: str | None = None,
               verdict: SentenceRecognition | None = None) -> dict[str, object]:
    """One candidate as recorded; `matched`/`intent`: what HA's matcher accepted it as."""
    out: dict[str, object] = {
        "text": c.text, "rule": c.rule, "source": c.source, "raw": round(c.raw, 3),
        "coverage": None if c.coverage is None else round(c.coverage, 3),
        "preference": round(c.preference, 4), "rank": round(c.rank, 4), "status": status}
    if detail:
        out["detail"] = detail
    if c.template:
        out["template"] = c.template
    if verdict is not None and verdict.match:
        out["matched"] = verdict.template
        if verdict.intent is not None:
            out["intent"] = verdict.intent
    return out


RECORD_CANDIDATES: Final = 6


@dataclass
class FixRecord:
    """A turn's `turns.stt_fix`: the original, what was proposed, what was sent, and why."""
    mode: TextFixMode
    original: str
    sent: str
    reason: Reason
    applied: bool = False
    outcome: FixOutcome | None = None
    cache: str | None = None            # "hit": a pause's result was reused; "miss": computed at the final text
    latency_ms: int | None = None       # final text → result
    deadline_met: bool | None = None    # whether `on` would have had it within FINAL_DEADLINE_S
    error: str | None = None

    def as_dict(self, candidates: int = RECORD_CANDIDATES) -> dict[str, object]:
        """The record; `candidates`: how many to list, those HA was asked about first."""
        out: dict[str, object] = {"mode": self.mode, "reason": self.reason, "original": self.original,
                                  "proposed": None, "sent": self.sent, "applied": self.applied}
        o = self.outcome
        if o is not None:
            d = o.decision
            p = d.proposed
            if p is not None:
                c = p.candidate
                out.update(proposed=c.text, rule=c.rule, source=c.source, raw=round(c.raw, 3),
                           coverage=None if c.coverage is None else round(c.coverage, 3),
                           preference=round(c.preference, 4), rank=round(c.rank, 4), status=p.status)
            out["preference_changed"] = d.preference_changed
            out["verdict"] = {"original": _verdict(o.original), "proposed": _verdict(p.verdict if p else None)}
            shown = sorted(d.checked, key=lambda ch: -ch.candidate.rank)[:candidates]
            out["candidates"] = [_candidate(ch.candidate, ch.status, ch.detail, ch.verdict) for ch in shown] + [
                _candidate(r.candidate, r.status, r.detail)
                for r in d.rejected[:max(0, candidates - len(shown))]]
            ctx = o.context
            out["context"] = {"connection": ctx.connection, "tables": ctx.tables, "config": ctx.config,
                              "pipeline": ctx.pipeline, "language": ctx.language}
            out["compute_ms"] = o.latency_ms
        out.update(cache=self.cache, latency_ms=self.latency_ms, deadline_met=self.deadline_met)
        if self.error:
            out["error"] = self.error
        return out

    def dumps(self) -> str:
        return json.dumps(self.as_dict(), separators=(",", ":"))


def trial_record(text: str, outcome: FixOutcome) -> FixRecord:
    """What `on` would record and send for `text` heard as a standalone command."""
    selected = outcome.decision.selected
    if selected is None:
        return FixRecord(TextFixMode.ON, text, text, outcome.decision.reason, outcome=outcome)
    return FixRecord(TextFixMode.ON, text, selected.candidate.text, Reason.APPLIED, applied=True, outcome=outcome)
