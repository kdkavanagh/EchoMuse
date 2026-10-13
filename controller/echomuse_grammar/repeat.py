"""Authoritative parse of "say that again": a request to replay this speaker's last
spoken response (§16.6, §16.7).

Only a whole sentence matches, so the family has no ``needs_more`` prefix. A match that is
also the start of other common requests ("what was that song", "repeat after me") is
``extendable``: the endpointer waits the longer pause before it commits.
"""

from __future__ import annotations

import re
from dataclasses import dataclass

from .normalize import normalize

_COMPLETE = (r"say (?:that|it) (?:again|1 more time)|repeat (?:that|it|yourself|what you (?:just )?said)"
             r"|what did you just say|come again")
_EXTENDABLE = r"repeat|what did you say|what was that|pardon(?: me)?"
# On normalized text, where "one more time" is "1 more time".
_PATTERN = re.compile(
    r"(?:sorry )?(?:(?:can|could|would|will) you )?(?:please )?"
    rf"(?:(?P<complete>{_COMPLETE})|{_EXTENDABLE})"
    r"(?P<again> (?:again|1 more time))?(?: please)?"
)


@dataclass(frozen=True)
class RepeatRequest:
    """A request to replay the last response. ``extendable``: the sentence also begins other
    requests ("what was that song"), and no "again" closes it."""

    extendable: bool = False


def parse_repeat(text: str) -> RepeatRequest | None:
    """Authoritative parse of a request to repeat the last response; None when ``text`` is not one."""
    match = _PATTERN.fullmatch(normalize(text))
    if match is None:
        return None
    return RepeatRequest(extendable=match.group("complete") is None and match.group("again") is None)
