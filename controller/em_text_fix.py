"""Transcript correction (SPEC §16.8): repair a misheard command before HA sees it.

Pure: candidate tables, the candidate rules, semantic preservation, ambiguity, and user
preference. No I/O; the HA calls and the table lifecycle are `em_text_fix_service`'s, the
turn plumbing `em_session`'s.

A correction is proposed only for text HA's sentence matcher does not already understand,
from six rules (`Rule`), and is applied only when HA's matcher fully accepts it under the
turn's own context, it preserves everything the original said explicitly (negation,
polarity, actions, numbers, units, named targets, and every slot HA had resolved), it is
not a security-state command, and no competing interpretation comes within
AMBIGUITY_MARGIN of it. Similarity here is a retrieval and scoring heuristic, not a
probability that the correction is what was meant.

Every rule reads only the canonical text (`fix_key`): lowercase, edge punctuation trimmed,
whitespace collapsed, exactly what the endpoint's stable prefix is. So a correction
computed at a pause holds for a final transcript with the same key. Letters spelled as
an acronym ("RMV") are recognized by having no vowel, not by their case.
"""

from __future__ import annotations

import bisect
import hashlib
import itertools
import json
import re
from collections import Counter
from collections.abc import Iterable, Iterator, Mapping, Sequence
from dataclasses import dataclass, replace
from enum import StrEnum
from functools import lru_cache
from typing import Final

from em_ha_client import SentenceRecognition, Vocabulary
from em_wake_phrase import normalize_command

# ── Configuration ────────────────────────────────────────────────────────────

MODE_KEY: Final = "transcriptFix"
TERMS_KEY: Final = "transcriptFixTerms"
PHRASES_KEY: Final = "transcriptFixPhrases"
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
    """One device's effective correction settings."""
    mode: TextFixMode = TextFixMode.OFF
    terms: tuple[str, ...] = ()
    phrases: tuple[str, ...] = ()

    @property
    def digest(self) -> str:
        """Identifies these settings in a result's context and its record."""
        blob = json.dumps([self.mode, self.terms, self.phrases], separators=(",", ":"))
        return hashlib.sha256(blob.encode()).hexdigest()[:12]


def fix_config(cfg: Mapping[str, object]) -> FixConfig:
    """The settings in an effective device config. The API validates every write, so a bad
    stored value means a hand-edited database: ValueError, and the caller turns it off."""
    raw_mode = cfg.get(MODE_KEY, TextFixMode.OFF.value)
    return FixConfig(parse_mode(raw_mode), parse_list(cfg.get(TERMS_KEY, [])),
                     parse_list(cfg.get(PHRASES_KEY, [])))


# ── Text features ────────────────────────────────────────────────────────────

_TOKEN = re.compile(r"[a-z0-9]+(?:'[a-z]+)?|%")
_NOT_LETTER = re.compile(r"[^a-z0-9]")
_SOUND = (("ph", "f"), ("ck", "k"), ("gh", "g"), ("wh", "w"), ("kn", "n"), ("wr", "r"), ("qu", "kw"),
          ("x", "ks"), ("q", "k"), ("z", "s"), ("ce", "se"), ("ci", "si"), ("cy", "sy"), ("c", "k"),
          ("v", "f"), ("dg", "j"), ("th", "t"))
_DOUBLED = re.compile(r"(.)\1+")
_LATER_VOWELS = re.compile(r"[aeiouyhw]")
_LETTER_NAMES = dict(zip("abcdefghijklmnopqrstuvwxyz",
                         "ay bee see dee ee ef gee aitch eye jay kay el em en oh pee cue ar es tee you vee "
                         "doubleyou ex why zee".split()))
_SPELLED = re.compile(r"[b-df-hj-np-tv-xz]{1,4}")   # no vowel (nor y): letters, not a word


def fix_key(text: str) -> str:
    """The canonical text every rule reads (normalize_command), and the key a result is
    reused by: lowercase, edge punctuation trimmed, whitespace collapsed."""
    return normalize_command(text)


def tokens(text: str) -> tuple[str, ...]:
    """Words, numbers, and `%`, casefolded; apostrophes inside a word are kept."""
    return tuple(_TOKEN.findall(text.casefold().replace("\u2019", "'")))


def letters(text: str) -> str:
    return _NOT_LETTER.sub("", text.casefold())


def sound_key(text: str) -> str:
    """A rough sound key: ph→f, c→k/s, v→f, th→t, …; doubled letters collapsed; vowels
    (and h, w, y) after the first letter dropped."""
    s = letters(text)
    for a, b in _SOUND:
        s = s.replace(a, b)
    s = _DOUBLED.sub(r"\1", s)
    return s[:1] + _LATER_VOWELS.sub("", s[1:])


def spoken_letters(text: str) -> str:
    """`text` with each vowel-less word of 1–4 letters said as letter names: decoders write
    some words as letters ("arm the" → "RMV", "the" → "v")."""
    return " ".join(" ".join(_LETTER_NAMES[c] for c in w) if _SPELLED.fullmatch(w) else w
                    for w in tokens(text))


def _distance(a: str, b: str, limit: int) -> int:
    """Edit distance (unit costs), or limit + 1 once it must exceed `limit`."""
    if len(a) < len(b):
        a, b = b, a
    la, lb = len(a), len(b)
    if la - lb > limit:
        return limit + 1
    if lb == 0:
        return la
    over = limit + 1
    prev = [j if j <= limit else over for j in range(lb + 1)]
    for i in range(1, la + 1):
        ca = a[i - 1]
        lo, hi = max(1, i - limit), min(lb, i + limit)
        cur = [over] * (lb + 1)
        cur[0] = i if i <= limit else over
        best = cur[0]
        for j in range(lo, hi + 1):
            v = prev[j - 1] + (ca != b[j - 1])
            if prev[j] + 1 < v:
                v = prev[j] + 1
            if cur[j - 1] + 1 < v:
                v = cur[j - 1] + 1
            cur[j] = v if v <= limit else over
            if v < best:
                best = v
        if best > limit:
            return over
        prev = cur
    return prev[lb]


def _ratio(a: str, b: str, minimum: float = 0.0) -> float:
    """1 − distance/longer length; 0.0 when below `minimum`."""
    longest = max(len(a), len(b), 1)
    limit = int((1.0 - minimum) * longest + 1e-9)
    d = _distance(a, b, limit)
    return 0.0 if d > limit else 1.0 - d / longest


def similarity(a: str, b: str, minimum: float = 0.0) -> float:
    """The better of the letter and sound-key ratios (0–1); 0.0 when both are below `minimum`."""
    return _similarity(letters(a), sound_key(a), letters(b), sound_key(b), minimum)


def _similarity(la: str, sa: str, lb: str, sb: str, minimum: float) -> float:
    return max(_ratio(la, lb, minimum), _ratio(sa, sb, minimum))


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
SECURITY_WORDS: Final = frozenset({
    "arm", "armed", "arming", "disarm", "disarmed", "disarming", "alarm", "alarms", "lock", "locks", "locked",
    "locking", "unlock", "unlocked", "unlocking", "security", "burglar", "intruder", "garage", "door",
    "doors", "gate", "gates", "deadbolt",
})
SECURITY_DOMAINS: Final = frozenset({"lock", "alarm_control_panel"})
SECURITY_DEVICE_CLASSES: Final = frozenset({"garage", "door", "gate", "lock"})
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
    SECURITY = "security"                  # a security-state command: never applied automatically


class Reason(StrEnum):
    """Why the turn sent what it sent (turns.stt_fix `reason`)."""
    APPLIED = "applied"                    # on: the correction was sent
    WOULD_APPLY = "would_apply"            # shadow: on would have sent it
    ORIGINAL_MATCHED = "original_matched"  # HA's matcher understands the original
    NO_CANDIDATE = "no_candidate"
    NO_VALID_CANDIDATE = "no_valid_candidate"
    AMBIGUOUS = "ambiguous"
    SECURITY = "security"
    CONTEXTUAL = "contextual"              # a follow-up in a conversation or reply window
    NOT_READY = "not_ready"                # tables not built, HA's matcher unavailable
    TIMEOUT = "timeout"                    # on: past FINAL_DEADLINE_S
    ERROR = "error"
    STALE = "stale"                        # computed under a context that no longer holds


# Rule thresholds (§16.8 table). Raw similarity, never including preference.
SLOT_MIN: Final = 0.75
NAME_MIN: Final = 0.80
PHRASE_MIN: Final = 0.85
CUTOFF_MIN_COVERAGE: Final = 0.5
LEAD_IN_MIN: Final = 0.75
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
    raw: float                       # raw similarity (cut-off: of the whole texts, reported only)
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
    sound: str
    source: Source
    template: str | None = None      # trigger
    position: int | None = None      # user phrase: 0 = highest preference
    turn_id: int | None = None       # history: the newest turn that sent it
    ts: float | None = None


def _grams(s: str, n: int) -> frozenset[str]:
    return frozenset(s[i:i + n] for i in range(len(s) - n + 1)) if len(s) >= n else frozenset({s} if s else ())


@dataclass(frozen=True)
class PhraseLayer:
    """Complete phrases with their lookup tables: letter trigrams and sound-key bigrams for
    similarity retrieval (q-gram count filter), sorted letters and reversed letters for
    head/tail completion."""
    phrases: tuple[Phrase, ...]
    _letter_grams: Mapping[str, tuple[int, ...]]
    _sound_grams: Mapping[str, tuple[int, ...]]
    _gram_counts: tuple[tuple[int, int], ...]   # (distinct letter trigrams, distinct sound bigrams)
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
        letter_grams: dict[str, list[int]] = {}
        sound_grams: dict[str, list[int]] = {}
        counts: list[tuple[int, int]] = []
        for i, p in enumerate(items):
            lg, sg = _grams(p.letters, 3), _grams(p.sound, 2)
            counts.append((len(lg), len(sg)))
            for g in lg:
                letter_grams.setdefault(g, []).append(i)
            for g in sg:
                sound_grams.setdefault(g, []).append(i)
        heads = sorted((p.letters, i) for i, p in enumerate(items))
        tails = sorted((p.letters[::-1], i) for i, p in enumerate(items))
        return cls(items, {g: tuple(v) for g, v in letter_grams.items()},
                   {g: tuple(v) for g, v in sound_grams.items()}, tuple(counts),
                   tuple(h for h, _ in heads), tuple(i for _, i in heads),
                   tuple(t for t, _ in tails), tuple(i for _, i in tails))

    def __len__(self) -> int:
        return len(self.phrases)

    def near(self, letter_key: str, sound: str, minimum: float) -> set[int]:
        """Phrases that may be within `minimum` similarity: a distinct q-gram survives each
        edit unless one of its q positions changed, so a match keeps at least
        max(distinct grams) − k·q of them, k the edits the threshold allows."""
        out: set[int] = set()
        for key, n, grams, which in ((letter_key, 3, self._letter_grams, 0), (sound, 2, self._sound_grams, 1)):
            mine = _grams(key, n)
            if not mine:
                continue
            counts: Counter[int] = Counter()
            for g in mine:
                counts.update(grams.get(g, ()))
            for i, common in counts.items():
                p = self.phrases[i]
                other = p.letters if which == 0 else p.sound
                longest = max(len(key), len(other))
                k = int((1.0 - minimum) * longest + _EPS)
                if abs(len(key) - len(other)) > k:
                    continue
                if common >= max(len(mine), self._gram_counts[i][which]) - k * n:
                    out.add(i)
        return out

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
    return Phrase(canonical, key, sound_key(canonical), source, template, position, turn_id, ts)


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
    letters: str
    sound: str
    target: Target
    domain: str | None = None            # entity
    area_ids: frozenset[str] = frozenset()


@dataclass(frozen=True)
class NameIndex:
    names: tuple[Name, ...]
    _by_letters: Mapping[int, tuple[int, ...]]
    _by_sound: Mapping[int, tuple[int, ...]]

    @classmethod
    def build(cls, names: Iterable[Name]) -> NameIndex:
        items = tuple(n for n in names if n.letters)
        by_letters: dict[int, list[int]] = {}
        by_sound: dict[int, list[int]] = {}
        for i, n in enumerate(items):
            by_letters.setdefault(len(n.letters), []).append(i)
            by_sound.setdefault(len(n.sound), []).append(i)
        return cls(items, {k: tuple(v) for k, v in by_letters.items()},
                   {k: tuple(v) for k, v in by_sound.items()})

    def near(self, letter_key: str, sound: str, minimum: float) -> set[int]:
        out: set[int] = set()
        for key, buckets in ((letter_key, self._by_letters), (sound, self._by_sound)):
            n = len(key)
            lo, hi = int(n * minimum - _EPS) if n else 0, int(n / max(minimum, _EPS) + _EPS)
            for length in range(max(lo, 1), hi + 1):
                out.update(buckets.get(length, ()))
        return out


@dataclass(frozen=True)
class Tables:
    """An immutable snapshot of everything a correction is looked up in, revisioned by the
    service that published it."""
    revision: int
    triggers: PhraseLayer
    history: PhraseLayer
    names: NameIndex
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
                "names": len(self.names.names), "catch_all_templates": len(self.skipped_templates)}


def build_tables(vocabulary: Vocabulary, templates: Sequence[str], history: Iterable[HistoryEntry],
                 revision: int) -> Tables:
    """The snapshot for one revision: CPU work, run off the event loop."""
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
            names.append(Name(n, letters(n), sound_key(n), Target(TargetKind.ENTITY, ids, n), e.domain,
                              frozenset({e.area_id}) if e.area_id else frozenset()))
    for a in vocabulary.areas:
        for n in a.names:
            area_ids.setdefault(n.casefold(), a.area_id)
            names.append(Name(n, letters(n), sound_key(n), Target(TargetKind.AREA, frozenset({a.area_id}), n),
                              None, frozenset({a.area_id})))
    for f in vocabulary.floors:
        for n in f.names:
            floor_ids.setdefault(n.casefold(), f.floor_id)
            names.append(Name(n, letters(n), sound_key(n), Target(TargetKind.FLOOR, frozenset({f.floor_id}), n),
                              None, frozenset(floor_areas.get(f.floor_id, ()))))
    words = {w for n in names for w in tokens(n.text) if len(w) >= 3 and w not in STOPWORDS}
    return Tables(
        revision=revision, triggers=PhraseLayer.build(triggers), history=build_history(history),
        names=NameIndex.build(names), name_words=frozenset(words),
        entity_ids_by_name={k: frozenset(v) for k, v in entity_ids.items()},
        area_id_by_name=area_ids, floor_id_by_name=floor_ids,
        entity_area={e.entity_id: e.area_id for e in vocabulary.entities}, area_floor=area_floor,
        skipped_templates=tuple(skipped),
    )


@dataclass(frozen=True)
class UserVocabulary:
    """The ordered `transcriptFixTerms` and `transcriptFixPhrases`, highest preference first."""
    terms: tuple[str, ...]
    phrases: tuple[str, ...]
    term_names: NameIndex
    phrase_layer: PhraseLayer

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
def user_vocabulary(terms: tuple[str, ...], phrases: tuple[str, ...]) -> UserVocabulary:
    term_names = NameIndex.build(
        Name(t, letters(t), sound_key(t), Target(TargetKind.TERM, frozenset({letters(t)}), t)) for t in terms)
    layer = PhraseLayer.build(p for i, text in enumerate(phrases)
                              if (p := _phrase(text, Source.USER_PHRASE, position=i)) is not None)
    return UserVocabulary(terms, phrases, term_names, layer)


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


def _wake_residue(span: str, wake_phrase: str) -> float:
    """How much a leading span sounds like the wake phrase or its tail (what is left of a
    wake word whose start the transcript lost)."""
    span_letters = letters(span)
    wake = letters(wake_phrase)
    if len(span_letters) < 3 or not wake:
        return 0.0
    span_sound = sound_key(span_letters)
    best = 0.0
    for k in range(3, len(wake) + 1):
        tail = wake[-k:]
        best = max(best, _similarity(span_letters, span_sound, tail, sound_key(tail), LEAD_IN_MIN))
    return best


def _replace_words(text: str, words: str, new: str) -> str | None:
    m = re.search(r"(?<![\w'])" + r"\s+".join(re.escape(w) for w in words.split()) + r"(?![\w'])", text)
    return None if m is None else text[:m.start()] + new + text[m.end():]


class _Proposer:
    def __init__(self, request: Request, original: SentenceRecognition | None, tables: Tables,
                 user: UserVocabulary) -> None:
        self.text = request.text
        self.request = request
        self.original = original
        self.tables = tables
        self.user = user
        self.words = self.text.split()
        self.tokens = tokens(self.text)
        self.protected = _Protected.of(self.tokens)
        self.out: list[Candidate] = []

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
        wake = self.request.wake_phrase
        if not wake:
            return
        for k in range(1, min(MAX_LEAD_WORDS, len(self.words) - 1) + 1):
            span = " ".join(self.words[:k])
            rest = " ".join(self.words[k:])
            if len(letters(rest)) < MIN_LETTERS:
                break
            raw = _wake_residue(span, wake)
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
            scored: list[tuple[float, Name]] = []
            for n in t.names.names:
                if n.target.kind != kind:
                    continue
                if kind == TargetKind.ENTITY and domain is not None and n.domain != domain:
                    continue
                if kind == TargetKind.ENTITY and area is not None and area not in n.area_ids:
                    continue
                if kind in (TargetKind.ENTITY, TargetKind.AREA) and floor is not None and not any(
                        t.area_floor.get(a) == floor for a in n.area_ids):
                    continue
                s = _similarity(letters(heard), sound_key(heard), n.letters, n.sound, SLOT_MIN)
                if s >= SLOT_MIN - _EPS:
                    scored.append((s, n))
            for s, n in sorted(scored, key=lambda x: (-x[0], x[1].text))[:5]:
                fixed = _replace_words(self.text, fix_key(heard), n.text)
                if fixed is not None:
                    self.add(Candidate(fixed, Rule.SLOT, Source(n.target.kind.value), s, target=n.target,
                                       replaced=fix_key(heard)))

    def spans(self) -> None:
        sources: tuple[tuple[NameIndex, Source | None], ...] = (
            (self.tables.names, None), (self.user.term_names, Source.USER_TERM))
        for i in range(len(self.words)):
            for j in range(i + 1, min(len(self.words), i + MAX_SPAN_WORDS) + 1):
                span = " ".join(self.words[i:j])
                span_tokens = tokens(span)
                key = letters(span)
                if len(key) < MIN_LETTERS or all(w in STOPWORDS for w in span_tokens):
                    continue
                sound = sound_key(span)
                for index, source in sources:
                    for idx in index.near(key, sound, NAME_MIN):
                        n = index.names[idx]
                        if fix_key(n.text) == fix_key(span):
                            continue
                        s = 1.0 if n.letters == key else _similarity(key, sound, n.letters, n.sound, NAME_MIN)
                        if s < NAME_MIN - _EPS:
                            continue
                        fixed = " ".join(self.words[:i] + [n.text] + self.words[j:])
                        self.add(Candidate(fixed, Rule.NAME, source or Source(n.target.kind.value), s,
                                           target=n.target, replaced=span))

    def phrases(self) -> None:
        key = letters(self.text)
        if len(key) < MIN_LETTERS:
            return
        sound = sound_key(self.text)
        spoken = spoken_letters(self.text)
        views = [(key, sound)]
        if fix_key(spoken) != self.text:
            views.append((letters(spoken), sound_key(spoken)))
        for layer in (self.user.phrase_layer, self.tables.triggers, self.tables.history):
            found: set[int] = set()
            for k, s in views:
                found |= layer.near(k, s, PHRASE_MIN)
            for idx in found:
                p = layer.phrases[idx]
                if p.text == self.text:
                    continue
                raw = max(_similarity(k, s, p.letters, p.sound, PHRASE_MIN) for k, s in views)
                if raw >= PHRASE_MIN - _EPS:
                    self.add(Candidate(p.text, Rule.PHRASE, p.source, raw, template=p.template))
            for idx in layer.completions(key, CUTOFF_MIN_COVERAGE):
                p = layer.phrases[idx]
                coverage = len(key) / len(p.letters)
                self.add(Candidate(p.text, Rule.CUTOFF, p.source, similarity(self.text, p.text),
                                   coverage=coverage, template=p.template))


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


def propose(request: Request, original: SentenceRecognition | None, tables: Tables,
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
    for c in _Proposer(request, original, tables, user).run():
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


def security_sensitive(original_text: str, candidate_text: str, v: SentenceRecognition | None) -> bool:
    """A lock, alarm panel, door or gate command, by its words or by what it would target."""
    words = set(tokens(original_text)) | set(tokens(spoken_letters(original_text))) | set(tokens(candidate_text))
    if words & SECURITY_WORDS:
        return True
    if v is None:
        return False
    if "domain" in v.details and v.details["domain"].value in SECURITY_DOMAINS:
        return True
    if "device_class" in v.details and v.details["device_class"].value in SECURITY_DEVICE_CLASSES:
        return True
    return any(e.split(".", 1)[0] in SECURITY_DOMAINS for e in v.targets)


def _check(c: Candidate, v: SentenceRecognition | None, original_text: str,
           original: SentenceRecognition | None, t: Tables) -> Checked:
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
    if security_sensitive(original_text, c.text, v):
        return Checked(c, Status.SECURITY, v, None, meaning)
    return Checked(c, Status.OK, v, None, meaning)


def decide(proposal: Proposal, verdicts: Sequence[SentenceRecognition | None],
           original: SentenceRecognition | None, tables: Tables) -> Decision:
    """Apply acceptance (§16.8) to HA's verdict on each validated candidate."""
    if original is not None and original.match:
        return Decision(Reason.ORIGINAL_MATCHED)
    checked = [_check(c, v, proposal.text, original, tables) for c, v in zip(proposal.validate, verdicts)]
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
        reason = Reason.SECURITY if any(ch.status == Status.SECURITY for ch in top) else Reason.NO_VALID_CANDIDATE
        return Decision(reason, None, proposed, tuple(checked), rejected)
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


def _candidate(c: Candidate, status: Status, detail: str | None = None) -> dict[str, object]:
    return {"text": c.text, "rule": c.rule, "source": c.source, "raw": round(c.raw, 3),
            "coverage": None if c.coverage is None else round(c.coverage, 3),
            "preference": round(c.preference, 4), "rank": round(c.rank, 4), "status": status,
            **({"detail": detail} if detail else {}), **({"template": c.template} if c.template else {})}


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

    def as_dict(self) -> dict[str, object]:
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
            shown = sorted(d.checked, key=lambda ch: -ch.candidate.rank)[:RECORD_CANDIDATES]
            out["candidates"] = [_candidate(ch.candidate, ch.status, ch.detail) for ch in shown] + [
                _candidate(r.candidate, r.status, r.detail)
                for r in d.rejected[:max(0, RECORD_CANDIDATES - len(shown))]]
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
