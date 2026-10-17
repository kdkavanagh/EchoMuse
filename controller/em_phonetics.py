"""Pronunciations and how alike two of them sound (SPEC §16.8).

Pure. A word's pronunciation is, in order: the user's guide (`transcriptFixPronunciations`),
the CMU Pronouncing Dictionary (cmudict), a number (0–9999, an ordinal like "3rd", or "%")
read as CMUdict's words for it, or, for a token of one to four letters with no vowel, its
letters' names ("RMV" → AA R EH M V IY: decoders write some words as letters). Any other word
has no pronunciation, and text containing it is never compared by sound. A guide replaces
the dictionary's pronunciations of its word.

Pronunciations are written in ARPAbet as CMUdict writes them, stress dropped; AH is one
segment however stressed (AX, ARPAbet's schwa, is accepted as AH), so "the" is DH AH or DH IY.
They are compared as articulatory segments: each phoneme is one, except the diphthongs AW,
AY, EY, OW and OY, which are their two (AY is a + IH), so "time" and "dim" differ in more
than one vowel quality. Two pronunciations are
compared by edit distance over segments: inserting or deleting one costs 1; substituting one
for another costs how many of their PanPhon articulatory features differ, scaled so the two
most different segments cost 2, as much as deleting one and inserting the other (T for D
costs 0.15, P for F 0.30, V for DH 0.52, a vowel for a stop about 1.5). Similarity is
1 − distance / the longer pronunciation's length in segments.
"""

from __future__ import annotations

import hashlib
import itertools
import json
import math
import re
from collections.abc import Sequence
from dataclasses import dataclass, field
from enum import StrEnum
from functools import lru_cache
from typing import Final

import numpy as np
import numpy.typing as npt

MAX_GUIDES: Final = 200
MAX_GUIDE_PHONEMES: Final = 30
MAX_WORD_CHARS: Final = 40
# A text's words may each have several pronunciations ("the": DH AH, DH IY); at most this
# many of their combinations are compared, the dictionary's first alternatives first.
MAX_VARIANTS: Final = 4
# Rows scored by weighted distance per query: of more in the length window, those sharing
# the most segment pairs with the query (one numpy pass over all of them) go first.
RERANK: Final = 256
_EPS: Final = 1e-6

# PanPhon 0.22.2 feature vectors (Mortensen et al., COLING 2016): +1 present, −1 absent,
# 0 unspecified. Features: syl son cons cont delrel lat nas strid voi sg cg ant cor distr lab
# hi lo back round velaric tense long hitone hireg. IPA: AA ɑ, AE æ, AH ʌ, AO ɔ, EH ɛ,
# ER ɹ̩, IH ɪ, IY i, UH ʊ, UW u, CH t͡ʃ, DH ð, G ɡ, HH h, JH d͡ʒ, NG ŋ, R ɹ, SH ʃ, TH θ, Y j,
# ZH ʒ, the rest as written; a, e, o are the onsets of AW, AY, EY, OW (OY's is AO).
_FEATURES: Final[dict[str, tuple[int, ...]]] = {
    "AA": (1, 1, -1, 1, 0, -1, -1, -1, 1, -1, -1, 0, -1, 0, -1, -1, 1, 1, -1, -1, 1, -1, 0, 0),
    "AE": (1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 0, -1, 0, -1, -1, 1, -1, -1, -1, 1, -1, 0, 0),
    "AH": (1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 0, -1, 0, -1, -1, -1, 1, -1, -1, 1, -1, 0, 0),
    "AO": (1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 0, -1, 0, -1, -1, -1, 1, 1, -1, -1, -1, 0, 0),
    "EH": (1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 0, -1, 0, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0),
    "ER": (1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 1, 1, -1, -1, 1, -1, -1, 1, -1, 0, -1, 0, 0),
    "IH": (1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 0, -1, 0, -1, 1, -1, -1, -1, -1, -1, -1, 0, 0),
    "IY": (1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 0, -1, 0, -1, 1, -1, -1, -1, -1, 1, -1, 0, 0),
    "UH": (1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 0, -1, 0, -1, 1, -1, 1, 1, -1, -1, -1, 0, 0),
    "UW": (1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 0, -1, 0, 1, 1, -1, 1, 1, -1, 1, -1, 0, 0),
    "a": (1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 0, -1, 0, -1, -1, 1, 1, -1, -1, 1, -1, 0, 0),
    "e": (1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 0, -1, 0, -1, -1, -1, -1, -1, -1, 1, -1, 0, 0),
    "o": (1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 0, -1, 0, -1, -1, -1, 1, 1, -1, 1, -1, 0, 0),
    "B": (-1, -1, 1, -1, -1, -1, -1, -1, 1, -1, -1, 1, -1, 0, 1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "CH": (-1, -1, 1, -1, 1, -1, -1, 1, -1, -1, -1, -1, 1, 1, -1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "D": (-1, -1, 1, -1, -1, -1, -1, -1, 1, -1, -1, 1, 1, -1, -1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "DH": (-1, -1, 1, 1, -1, -1, -1, -1, 1, -1, -1, 1, 1, 1, -1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "F": (-1, -1, 1, 1, -1, -1, -1, 1, -1, -1, -1, 1, -1, 0, 1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "G": (-1, -1, 1, -1, -1, -1, -1, -1, 1, -1, -1, -1, -1, 0, -1, 1, -1, 1, -1, -1, 0, -1, 0, 0),
    "HH": (-1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, -1, -1, -1, 1, -1, -1, 0, -1, 0, 0),
    "JH": (-1, -1, 1, -1, 1, -1, -1, 1, 1, -1, -1, -1, 1, 1, -1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "K": (-1, -1, 1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, -1, 1, -1, 1, -1, -1, 0, -1, 0, 0),
    "L": (-1, 1, 1, 1, -1, 1, -1, -1, 1, -1, -1, 1, 1, -1, -1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "M": (-1, 1, 1, -1, -1, -1, 1, -1, 1, -1, -1, 1, -1, 0, 1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "N": (-1, 1, 1, -1, -1, -1, 1, -1, 1, -1, -1, 1, 1, -1, -1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "NG": (-1, 1, 1, -1, -1, -1, 1, -1, 1, -1, -1, -1, -1, 0, -1, 1, -1, 1, -1, -1, 0, -1, 0, 0),
    "P": (-1, -1, 1, -1, -1, -1, -1, -1, -1, -1, -1, 1, -1, 0, 1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "R": (-1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, 1, 1, -1, -1, 1, -1, -1, 1, -1, 0, -1, 0, 0),
    "S": (-1, -1, 1, 1, -1, -1, -1, 1, -1, -1, -1, 1, 1, -1, -1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "SH": (-1, -1, 1, 1, -1, -1, -1, 1, -1, -1, -1, -1, 1, 1, -1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "T": (-1, -1, 1, -1, -1, -1, -1, -1, -1, -1, -1, 1, 1, -1, -1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "TH": (-1, -1, 1, 1, -1, -1, -1, -1, -1, -1, -1, 1, 1, 1, -1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "V": (-1, -1, 1, 1, -1, -1, -1, 1, 1, -1, -1, 1, -1, 0, 1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "W": (-1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, -1, -1, 0, 1, 1, -1, 1, 1, -1, 0, -1, 0, 0),
    "Y": (-1, 1, -1, 1, -1, -1, -1, -1, 1, -1, -1, -1, -1, 0, -1, 1, -1, -1, -1, -1, 0, -1, 0, 0),
    "Z": (-1, -1, 1, 1, -1, -1, -1, 1, 1, -1, -1, 1, 1, -1, -1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
    "ZH": (-1, -1, 1, 1, -1, -1, -1, 1, 1, -1, -1, -1, 1, 1, -1, -1, -1, -1, -1, -1, 0, -1, 0, 0),
}
SEGMENTS: Final = tuple(_FEATURES)
_CODE: Final = {s: i for i, s in enumerate(SEGMENTS)}
_DIPHTHONGS: Final = {"AW": ("a", "UH"), "AY": ("a", "IH"), "EY": ("e", "IH"), "OW": ("o", "UH"), "OY": ("AO", "IH")}
# ARPAbet as written (CMUdict's 39, plus AX for AH), each as its segment codes.
ARPABET: Final[dict[str, bytes]] = {
    **{p: bytes((_CODE[p],)) for p in SEGMENTS if p not in ("a", "e", "o")},
    **{p: bytes(_CODE[s] for s in segs) for p, segs in _DIPHTHONGS.items()},
    "AX": bytes((_CODE["AH"],)),
}
_VECTORS = np.array([_FEATURES[s] for s in SEGMENTS], dtype=np.float32)
_L1 = np.abs(_VECTORS[:, None, :] - _VECTORS[None, :, :]).sum(axis=2)
SUB_COST: Final[npt.NDArray[np.float32]] = (2.0 * _L1 / _L1.max()).astype(np.float32)
_SUB: Final[list[list[float]]] = SUB_COST.tolist()
# Rows of a PhoneticLayer are padded with _PAD, which costs 1 against every segment (like an
# insertion), so cells past a row's end never undercut its real ones.
_PAD: Final = len(SEGMENTS)
_PADDED_COST: Final = np.ones((len(SEGMENTS) + 1, len(SEGMENTS) + 1), dtype=np.float32)
_PADDED_COST[:-1, :-1] = SUB_COST
_PAIRS: Final = (_PAD + 1) ** 2

_LETTER_NAMES: Final = dict(zip("abcdefghijklmnopqrstuvwxyz", (
    "EY", "B IY", "S IY", "D IY", "IY", "EH F", "JH IY", "EY CH", "AY", "JH EY", "K EY", "EH L", "EH M", "EH N",
    "OW", "P IY", "K Y UW", "AA R", "EH S", "T IY", "Y UW", "V IY", "D AH B AH L Y UW", "EH K S", "W AY", "Z IY")))
_SPELLED: Final = re.compile(r"[b-df-hj-np-tv-xz]{1,4}")      # no vowel (nor y): letters, not a word
_TOKEN: Final = re.compile(r"[a-z0-9]+(?:'[a-z]+)?|%")
_WORD: Final = re.compile(r"[a-z0-9]+(?:'[a-z]+)?")
_SYMBOL: Final = re.compile(r"([A-Z]+)([012]?)")


def tokens(text: str) -> tuple[str, ...]:
    """Words, numbers, and `%`, casefolded; apostrophes inside a word are kept."""
    return tuple(_TOKEN.findall(text.casefold().replace("\u2019", "'")))


def parse_arpabet(text: str) -> str:
    """ARPAbet as CMUdict writes it ("P L EH1 K S"), canonical: stress dropped, AX written
    AH. ValueError names an unknown symbol."""
    out = []
    for symbol in text.upper().split():
        m = _SYMBOL.fullmatch(symbol)
        if m is None or m.group(1) not in ARPABET:
            raise ValueError(f"unknown phoneme {symbol!r}")
        out.append("AH" if m.group(1) == "AX" else m.group(1))
    if not out:
        raise ValueError("no phonemes")
    return " ".join(out)


def segments(arpabet: str) -> bytes:
    """Canonical ARPAbet (parse_arpabet) as segment codes."""
    return b"".join(ARPABET[p] for p in arpabet.split())


def parse_guides(raw: object) -> tuple[tuple[str, str], ...]:
    """A `transcriptFixPronunciations` value: {word: "ARPABET"}, as (word, canonical ARPAbet)
    pairs in word order, each word casefolded. ValueError names what is wrong."""
    if not isinstance(raw, dict):
        raise ValueError('must be an object of {"word": "phonemes"}')
    if len(raw) > MAX_GUIDES:
        raise ValueError(f"at most {MAX_GUIDES} words")
    out: dict[str, str] = {}
    for word, phonemes in raw.items():
        if not isinstance(word, str) or not isinstance(phonemes, str):
            raise ValueError('must be an object of {"word": "phonemes"}')
        key = word.strip().casefold().replace("\u2019", "'")
        if not _WORD.fullmatch(key) or len(key) > MAX_WORD_CHARS:
            raise ValueError(f"{word!r} is not one word of letters and digits (at most {MAX_WORD_CHARS})")
        if key in out:
            raise ValueError(f"{word!r} is listed twice")
        try:
            canonical = parse_arpabet(phonemes)
        except ValueError as err:
            raise ValueError(f"{word!r}: {err}") from None
        if len(canonical.split()) > MAX_GUIDE_PHONEMES:
            raise ValueError(f"{word!r}: at most {MAX_GUIDE_PHONEMES} phonemes")
        out[key] = canonical
    return tuple(sorted(out.items()))


@lru_cache(maxsize=1)
def _dictionary() -> dict[str, str]:
    """CMUdict as word → its pronunciations joined by "|": about 17 MB, against 57 MB for
    cmudict.dict(); a word is parsed when first used."""
    import cmudict

    out: dict[str, str] = {}
    for line in cmudict.dict_string().splitlines():
        word, _, pron = line.partition(" ")
        pron = pron.split("#", 1)[0].strip()
        word = word.split("(", 1)[0]
        if pron:
            out[word] = f"{out[word]}|{pron}" if word in out else pron
    return out


def load_dictionary() -> None:
    """Read CMUdict now (about 0.4 s), off the event loop, rather than at the first lookup."""
    _dictionary()


class PronunciationSource(StrEnum):
    GUIDE = "guide"
    DICTIONARY = "dictionary"
    LETTERS = "letters"
    NUMBER = "number"


@dataclass(frozen=True, slots=True)
class Word:
    """A word's pronunciations, most common first: ARPAbet to show, segments to compare."""
    source: PronunciationSource
    arpabet: tuple[str, ...]
    alternatives: tuple[bytes, ...]

    @classmethod
    def of(cls, source: PronunciationSource, arpabet: Sequence[str]) -> Word:
        spelled = tuple(dict.fromkeys(arpabet))
        return cls(source, spelled, tuple(segments(a) for a in spelled))


@lru_cache(maxsize=65536)
def _dictionary_word(word: str) -> Word | None:
    raw = _dictionary().get(word)
    return None if raw is None else Word.of(PronunciationSource.DICTIONARY,
                                            [parse_arpabet(p) for p in raw.split("|")])


@lru_cache(maxsize=4096)
def _letters_word(word: str) -> Word:
    return Word.of(PronunciationSource.LETTERS, [" ".join(_LETTER_NAMES[c] for c in word)])


_ONES: Final = ("zero one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen "
                "sixteen seventeen eighteen nineteen").split()
_TENS: Final = ("", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety")
_ORDINALS: Final = {"one": "first", "two": "second", "three": "third", "five": "fifth", "eight": "eighth",
                    "nine": "ninth", "twelve": "twelfth"}
_NUMBER: Final = re.compile(r"(\d{1,4})(st|nd|rd|th)?")


def _say(n: int) -> list[str]:
    """0–9999 as the words said."""
    if n < 20:
        return [_ONES[n]]
    if n < 100:
        return [_TENS[n // 10]] + (_say(n % 10) if n % 10 else [])
    if n < 1000:
        return [_ONES[n // 100], "hundred"] + (_say(n % 100) if n % 100 else [])
    return _say(n // 1000) + ["thousand"] + (_say(n % 1000) if n % 1000 else [])


@lru_cache(maxsize=4096)
def _number_word(token: str) -> Word | None:
    """A number (0–9999), an ordinal ("3rd") or "%", read as CMUdict's words for it."""
    m = _NUMBER.fullmatch(token)
    if token == "%":
        words = ["percent"]
    elif m is None:
        return None
    else:
        words = _say(int(m.group(1)))
        if m.group(2):
            last = words[-1]
            words[-1] = _ORDINALS.get(last) or (last[:-1] + "ieth" if last.endswith("ty") else last + "th")
    found = [_dictionary_word(w) for w in words]
    if any(f is None for f in found):
        return None
    return Word.of(PronunciationSource.NUMBER, [" ".join(f.arpabet[0] for f in found if f is not None)])


@dataclass(frozen=True)
class Lexicon:
    """CMUdict with the user's guides in place of its entries; numbers read aloud and
    vowel-less tokens spelled."""
    guides: tuple[tuple[str, str], ...] = ()
    _guides: dict[str, Word] = field(init=False, compare=False, repr=False)

    def __post_init__(self) -> None:
        object.__setattr__(self, "_guides", {w: Word.of(PronunciationSource.GUIDE, [p]) for w, p in self.guides})

    @property
    def digest(self) -> str:
        return hashlib.sha256(json.dumps(self.guides).encode()).hexdigest()[:12]

    def word(self, word: str) -> Word | None:
        if (guide := self._guides.get(word)) is not None:
            return guide
        if (found := _dictionary_word(word)) is not None:
            return found
        if (number := _number_word(word)) is not None:
            return number
        return _letters_word(word) if _SPELLED.fullmatch(word) else None

    def pronounce(self, text: str) -> Pronounced:
        words = tokens(text)
        return Pronounced(words, tuple(self.word(w) for w in words))


@dataclass(frozen=True)
class Pronounced:
    """A text's words and each one's pronunciations (None: it has none)."""
    words: tuple[str, ...]
    found: tuple[Word | None, ...]

    @property
    def unknown(self) -> tuple[str, ...]:
        return tuple(dict.fromkeys(w for w, f in zip(self.words, self.found) if f is None))

    def variants(self, start: int = 0, end: int | None = None) -> tuple[bytes, ...]:
        """The pronunciations of words[start:end] as segments, at most MAX_VARIANTS; () when
        one of those words has no pronunciation, or there are none."""
        found = self.found[start:end]
        if not found or any(f is None for f in found):
            return ()
        options = [f.alternatives for f in found if f is not None]
        combos = itertools.islice(itertools.product(*options), MAX_VARIANTS * 4)
        return tuple(dict.fromkeys(b"".join(c) for c in combos))[:MAX_VARIANTS]


def distance(a: bytes, b: bytes) -> float:
    """Weighted edit distance between two segment strings."""
    prev = [float(j) for j in range(len(b) + 1)]
    for i, x in enumerate(a, 1):
        cost = _SUB[x]
        cur = [float(i)]
        for j, y in enumerate(b, 1):
            cur.append(min(prev[j] + 1.0, cur[j - 1] + 1.0, prev[j - 1] + cost[y]))
        prev = cur
    return prev[-1]


def similarity(a: bytes, b: bytes) -> float:
    longest = max(len(a), len(b))
    return 1.0 - distance(a, b) / longest if longest else 1.0


def best_similarity(a: Sequence[bytes], b: Sequence[bytes]) -> float:
    """The best similarity between any pronunciation of one text and any of the other; 0.0
    when either has none."""
    return max((similarity(x, y) for x in a for y in b), default=0.0)


def batch_distance(query: bytes, rows: npt.NDArray[np.uint8], lengths: npt.NDArray[np.intp],
                   limits: npt.NDArray[np.float32]) -> npt.NDArray[np.float32]:
    """`distance(query, row)` for each _PAD-padded row, at once, one pass per query segment;
    inf for a row whose distance must exceed its limit. A row's insertions are a running
    minimum: cur[j] = j + min over k ≤ j of (base[k] − k). After query segment i the distance
    is at least min over j of cur[j] + |(n − i) − (length − j)|, the length difference left
    to pay in insertions or deletions, so a row is dropped once that passes its limit."""
    out = np.full(len(rows), np.inf, dtype=np.float32)
    alive = np.arange(len(rows))
    width = rows.shape[1]
    steps = np.arange(width + 1, dtype=np.float32)
    prev = np.broadcast_to(steps, (len(rows), width + 1)).copy()
    left = lengths.astype(np.float32)[:, None] - steps          # segments of the row after column j
    for i, segment in enumerate(query, 1):
        base = np.empty_like(prev)
        base[:, 0] = i
        np.minimum(prev[:, 1:] + 1.0, prev[:, :-1] + _PADDED_COST[segment][rows], out=base[:, 1:])
        prev = steps + np.minimum.accumulate(base - steps, axis=1)
        keep = (prev + np.abs(left - (len(query) - i))).min(axis=1) <= limits
        if not keep.all():
            alive, prev, rows, lengths, limits, left = (alive[keep], prev[keep], rows[keep], lengths[keep],
                                                        limits[keep], left[keep])
            if not len(alive):
                return out
    out[alive] = prev[np.arange(len(alive)), lengths]
    return out


@dataclass(frozen=True)
class PhoneticLayer:
    """The pronunciations of a list of texts (their owners) for searching all at once: one
    _PAD-padded row per pronunciation, sorted by length."""
    rows: npt.NDArray[np.uint8]
    lengths: npt.NDArray[np.intp]
    owners: npt.NDArray[np.intp]
    starts: npt.NDArray[np.intp]          # starts[L]: the first row at least L long

    @classmethod
    def build(cls, variants: Sequence[Sequence[bytes]]) -> PhoneticLayer:
        """`variants[owner]`: that text's pronunciations (none: it is never found)."""
        items = sorted((len(v), owner, v) for owner, vs in enumerate(variants) for v in vs if v)
        width = max((n for n, _, _ in items), default=0)
        rows = np.full((len(items), max(width, 1)), _PAD, dtype=np.uint8)
        for r, (n, _, v) in enumerate(items):
            rows[r, :n] = np.frombuffer(v, dtype=np.uint8)
        lengths = np.array([n for n, _, _ in items], dtype=np.intp)
        owners = np.array([o for _, o, _ in items], dtype=np.intp)
        starts = np.searchsorted(lengths, np.arange(width + 2), side="left").astype(np.intp)
        return cls(rows, lengths, owners, starts)

    def __len__(self) -> int:
        return len(self.rows)

    def search(self, queries: Sequence[bytes], minimum: float,
               allowed: npt.NDArray[np.bool_] | None = None) -> dict[int, float]:
        """Each owner with a pronunciation within `minimum` similarity of a query's, and its
        best similarity. `allowed[owner]` False: never considered. Only rows whose length
        allows `minimum` are compared (similarity ≤ shorter / longer length), and of more
        than RERANK such rows only the RERANK that share the most segment pairs with the
        query (one vectorized lookup over all of them) are scored by the weighted distance."""
        best: dict[int, float] = {}
        width = len(self.starts) - 2
        for query in queries:
            n = len(query)
            if n == 0 or width <= 0:
                continue
            lo = max(1, math.ceil(n * minimum - _EPS))
            hi = width if minimum <= 0 else min(width, math.floor(n / minimum + _EPS))
            if lo > hi:
                continue
            a, b = int(self.starts[lo]), int(self.starts[hi + 1])
            if a >= b:
                continue
            owners, rows, lengths = self.owners[a:b], self.rows[a:b, :hi], self.lengths[a:b]
            if allowed is not None:
                keep = allowed[owners]
                owners, rows, lengths = owners[keep], rows[keep], lengths[keep]
                if not len(owners):
                    continue
            if len(rows) > RERANK:
                score = _shared_pairs(query, rows) / np.maximum(lengths, n)
                top = np.argpartition(-score, RERANK)[:RERANK]
                owners, rows, lengths = owners[top], rows[top], lengths[top]
            longest = np.maximum(lengths, n).astype(np.float32)
            sims = 1.0 - batch_distance(query, rows, lengths, (1.0 - minimum) * longest + _EPS) / longest
            hit = sims >= minimum - _EPS
            for owner, sim in zip(owners[hit].tolist(), sims[hit].tolist()):
                if sim > best.get(owner, -1.0):
                    best[owner] = sim
        return best


def _shared_pairs(query: bytes, rows: npt.NDArray[np.uint8]) -> npt.NDArray[np.intp]:
    """For each row, how many of its adjacent segment pairs occur in `query`: a lookup table
    over every pair code, gathered for the whole row block at once (padding never matches).
    Divided by the longer length it ranks like similarity: a long row sharing many pairs
    with a short query is not near it."""
    lookup = np.zeros(_PAIRS, dtype=np.bool_)
    q = np.frombuffer(query, dtype=np.uint8).astype(np.intp)
    lookup[q[:-1] * (_PAD + 1) + q[1:]] = True
    pairs = rows[:, :-1].astype(np.intp) * (_PAD + 1) + rows[:, 1:]
    counts: npt.NDArray[np.intp] = lookup[pairs].sum(axis=1)
    return counts
