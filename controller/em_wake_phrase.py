"""Wake-phrase location, text removal, and wake verification (SPEC §8.4, §16.6).

No audio is ever trimmed: the committed span always contains the wake word and
only the text is changed. Streaming positions are capture-epoch sample indices;
character spans index the original transcript.
"""

from __future__ import annotations

import bisect
from dataclasses import dataclass
from typing import Sequence

# §16.6 wake-phrase removal.
MATCH_DISTANCE = 0.45
MAX_WINDOW_WORDS = 3
# §16.6: the streaming window must end by candidate open + the verification lookahead.
LOOKAHEAD_SAMPLES = 7_680  # 480 ms at 16 kHz
# §16.6 wake verification.
VERIFY_DISTANCE = 0.40

_APOSTROPHES = "'\u2019"
_SENTENCEPIECE_SPACE = "\u2581"


@dataclass(frozen=True, slots=True)
class Word:
    """One `words()` entry: lowercased text and its [start, end) span in the original."""

    text: str
    start: int
    end: int


@dataclass(frozen=True, slots=True)
class Window:
    """A minimal matching window of words [start, stop) with its distance."""

    start: int
    stop: int
    distance: float


def levenshtein(a: str, b: str) -> int:
    """Edit distance with unit insert/delete/substitute costs."""
    if len(a) < len(b):
        a, b = b, a
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]


def _is_word_char(ch: str) -> bool:
    return ch.isalpha() or ch.isdigit() or ch in _APOSTROPHES


def words(text: str) -> list[Word]:
    """§16.6 `words()`: lowercase; anything but letters, digits and apostrophes separates."""
    out: list[Word] = []
    i, n = 0, len(text)
    while i < n:
        if not _is_word_char(text[i]):
            i += 1
            continue
        j = i
        while j < n and _is_word_char(text[j]):
            j += 1
        out.append(Word(text[i:j].lower(), i, j))
        i = j
    return out


def _letters(text: str) -> str:
    return "".join(ch for ch in text.lower() if ch.isalpha())


def window_distance(ws: Sequence[Word], wake_phrase: str) -> float:
    """§16.6 `distance()`: letters joined, Levenshtein / longer length."""
    letters = "".join(ch for w in ws for ch in w.text if ch.isalpha())
    phrase = _letters(wake_phrase)
    longer = max(len(letters), len(phrase))
    if longer == 0:
        return 1.0
    return levenshtein(letters, phrase) / longer


def minimal_windows(ws: Sequence[Word], wake_phrase: str) -> list[Window]:
    """Windows of 1–3 words that match while neither one-word-shorter sub-window does."""
    cache: dict[tuple[int, int], float] = {}

    def dist(i: int, j: int) -> float:
        if (i, j) not in cache:
            cache[(i, j)] = window_distance(ws[i:j], wake_phrase) if j > i else 1.0
        return cache[(i, j)]

    out: list[Window] = []
    for i in range(len(ws)):
        for size in range(1, MAX_WINDOW_WORDS + 1):
            j = i + size
            if j > len(ws):
                break
            d = dist(i, j)
            if d > MATCH_DISTANCE:
                continue
            if size > 1 and (dist(i + 1, j) <= MATCH_DISTANCE or dist(i, j - 1) <= MATCH_DISTANCE):
                continue
            out.append(Window(i, j, d))
    return out


def _cut_through(text: str, end: int) -> int:
    """Index after `end` and any punctuation/whitespace that follows it."""
    while end < len(text) and not text[end].isalnum():
        end += 1
    return end


def normalize_command(text: str) -> str:
    """§16.2 local-command normalization: lowercase, trim edge punctuation, collapse whitespace."""
    collapsed = " ".join(text.lower().split())
    start, end = 0, len(collapsed)
    while start < end and not collapsed[start].isalnum():
        start += 1
    while end > start and not collapsed[end - 1].isalnum():
        end -= 1
    return collapsed[start:end]


@dataclass(frozen=True)
class StreamingTranscript:
    """A streaming ASR result with per-word emission samples.

    `first_samples[i]`/`last_samples[i]` are the emission samples of the tokens
    holding word i's first and last characters (emission times, not alignments).
    """

    text: str
    words: tuple[Word, ...]
    first_samples: tuple[int, ...]
    last_samples: tuple[int, ...]

    @classmethod
    def from_tokens(cls, tokens: Sequence[str], emission_samples: Sequence[int]) -> StreamingTranscript:
        """Build from recognizer tokens (`▁` or a leading space starts a word)."""
        if len(tokens) != len(emission_samples):
            raise ValueError("tokens and emission_samples differ in length")
        pieces: list[str] = []
        token_ends: list[int] = []
        length = 0
        for token in tokens:
            piece = token.replace(_SENTENCEPIECE_SPACE, " ")
            pieces.append(piece)
            length += len(piece)
            token_ends.append(length)
        text = "".join(pieces)
        ws = tuple(words(text))
        samples = [int(s) for s in emission_samples]

        def token_at(char_index: int) -> int:
            return bisect.bisect_right(token_ends, char_index)

        return cls(
            text=text,
            words=ws,
            first_samples=tuple(samples[token_at(w.start)] for w in ws),
            last_samples=tuple(samples[token_at(w.end - 1)] for w in ws),
        )


@dataclass(frozen=True, slots=True)
class CommandText:
    """Streaming command text after the wake cut, §16.2-normalized (step 4)."""

    text: str
    first_token_sample: int | None


def locate_streaming(transcript: StreamingTranscript, wake_phrase: str, deadline: int) -> Window | None:
    """Step 2: minimal window whose last word was emitted by `deadline`; lowest distance, then earliest.

    `deadline` is candidate open + LOOKAHEAD_SAMPLES.
    """
    candidates = [
        w
        for w in minimal_windows(transcript.words, wake_phrase)
        if transcript.last_samples[w.stop - 1] <= deadline
    ]
    if not candidates:
        return None
    return min(candidates, key=lambda w: (w.distance, w.start))


def strip_final(final_text: str, streaming_start: int, wake_phrase: str) -> str:
    """Step 3: remove the matching window near `streaming_start` and everything before it."""
    ws = words(final_text)
    candidates = [w for w in minimal_windows(ws, wake_phrase) if w.start <= streaming_start + 1]
    if not candidates:
        return final_text
    best = min(candidates, key=lambda w: (w.distance, abs(w.start - streaming_start), w.start))
    return final_text[_cut_through(final_text, ws[best.stop - 1].end):]


def final_command_text(
    final_text: str, *, wake_initiated: bool, streaming_window: Window | None, wake_phrase: str
) -> str:
    """Steps 1–3: the HA transcript routed onward. Only wake-initiated turns strip anything.

    When the streaming transcript has no window (Kroko can drop a quiet wake word),
    step 3 runs with `s` = 0: the wake word is assumed to open the utterance.
    """
    if not wake_initiated:
        return final_text
    return strip_final(final_text, streaming_window.start if streaming_window is not None else 0, wake_phrase)


def command_text(transcript: StreamingTranscript, window: Window | None) -> CommandText:
    """Step 4: cut the streaming transcript through `window` (if any), then normalize (§16.2)."""
    cut = 0
    if window is not None:
        cut = _cut_through(transcript.text, transcript.words[window.stop - 1].end)
    first = next((i for i, w in enumerate(transcript.words) if w.start >= cut), None)
    return CommandText(
        text=normalize_command(transcript.text[cut:]),
        first_token_sample=None if first is None else transcript.first_samples[first],
    )


@dataclass(frozen=True, slots=True)
class Verification:
    """Wake verification result; `distance` is None when no substring could be compared."""

    passed: bool
    distance: float | None


def verify_wake(transcript: str, core: str) -> Verification:
    """§16.6 alias test: min over substrings of length len(core)±1 of lev/len(core) ≤ 0.40."""
    letters = _letters(transcript)
    core = _letters(core)
    if not letters or not core:
        return Verification(False, None)
    best: float | None = None
    for size in range(max(1, len(core) - 1), len(core) + 2):
        for i in range(len(letters) - size + 1):
            d = levenshtein(letters[i : i + size], core) / len(core)
            if best is None or d < best:
                best = d
    if best is None:
        return Verification(False, None)
    return Verification(best <= VERIFY_DISTANCE, best)
