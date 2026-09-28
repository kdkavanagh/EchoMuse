"""Text normalization shared by all grammar families."""

from __future__ import annotations

import re

_SMALL = {
    "zero": 0,
    "one": 1,
    "two": 2,
    "three": 3,
    "four": 4,
    "five": 5,
    "six": 6,
    "seven": 7,
    "eight": 8,
    "nine": 9,
    "ten": 10,
    "eleven": 11,
    "twelve": 12,
    "thirteen": 13,
    "fourteen": 14,
    "fifteen": 15,
    "sixteen": 16,
    "seventeen": 17,
    "eighteen": 18,
    "nineteen": 19,
}
_TENS = {
    "twenty": 20,
    "thirty": 30,
    "forty": 40,
    "fifty": 50,
    "sixty": 60,
    "seventy": 70,
    "eighty": 80,
    "ninety": 90,
}
_PUNCTUATION = re.compile(r"[^\w'/]+", re.UNICODE)
_AMPM = re.compile(r"\b([ap])\s*\.?\s*m\.?\b", re.IGNORECASE)
_DIGIT_AMPM = re.compile(r"(?<=\d)(am|pm)\b", re.IGNORECASE)


def _convert_numbers(words: list[str]) -> list[str]:
    converted: list[str] = []
    index = 0
    while index < len(words):
        word = words[index]
        if word in _TENS:
            value = _TENS[word]
            if index + 1 < len(words) and words[index + 1] in _SMALL:
                ones = _SMALL[words[index + 1]]
                if 0 < ones < 10:
                    value += ones
                    index += 1
            converted.append(str(value))
        elif word in _SMALL:
            converted.append(str(_SMALL[word]))
        else:
            converted.append(word)
        index += 1
    return converted


def normalize(text: str) -> str:
    """Return the §16.2/§16.6 canonical lowercase token string.

    Punctuation becomes a word boundary (edge punctuation is trimmed, internal
    punctuation such as ``7:30`` splits), whitespace collapses, ``a.m.``/``a m``
    become ``am`` (likewise pm), and English number words through 99 become
    decimal tokens. Apostrophes inside words and the timer spelling ``1/2``
    are kept, so negations such as ``don't`` survive as their own token.
    """

    value = _AMPM.sub(lambda match: f"{match.group(1).lower()}m", text.casefold())
    value = _DIGIT_AMPM.sub(r" \1", value)
    value = _PUNCTUATION.sub(" ", value.replace("-", " "))
    words = [word.strip("'/") for word in value.split()]
    return " ".join(_convert_numbers([word for word in words if word]))


def tokens(text: str) -> tuple[str, ...]:
    """Normalized tokens of ``text`` (``normalize(text).split()``)."""

    return tuple(normalize(text).split())
