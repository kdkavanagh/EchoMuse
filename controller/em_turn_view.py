"""
The turn view (docs/echo-show-companion-plan.md §11 decision 8): what a
`turn_view_v1` device shows of one turn, sent as `turn.view` (protocol-v1
§4.9). The firmware relays the body unchanged to its Kiosk Satellite plugin.

The controller sends the whole view every time, never a delta, so a lost
message costs nothing: the next one carries everything.
"""

from __future__ import annotations

import asyncio
import enum
import re
from dataclasses import dataclass, field

# The answer as shown: at most this much UTF-8, ellipsis included. The firmware
# relays a view on its local link, whose lines hold 128 KiB, so this leaves
# room to spare.
ANSWER_MAX_BYTES = 16 * 1024
ELLIPSIS = "…"

# Streamed answer text goes out at most this often, the latest winning
# (decision 8: 10 per second).
ANSWER_INTERVAL_S = 0.1

_LAST_WORD = re.compile(r"\s\S*$")


class ViewPhase(enum.StrEnum):
    """`turn.view` `phase`."""

    LISTENING = "listening"    # the turn's dialog_input started
    THINKING = "thinking"      # dispatched: `heard` is known, nothing answered yet
    ANSWERING = "answering"    # the answer so far (streamed), or the final speech
    DONE = "done"              # the reply played, or the turn ended without one


def cap_answer(text: str) -> str:
    """`text` within ANSWER_MAX_BYTES of UTF-8. A longer answer is cut at the
    last word that fits with ELLIPSIS after it (mid-word only when its first
    word alone does not fit), and ELLIPSIS appended."""
    data = text.encode()
    if len(data) <= ANSWER_MAX_BYTES:
        return text
    head = data[:ANSWER_MAX_BYTES - len(ELLIPSIS.encode())].decode("utf-8", "ignore")
    if not text[len(head)].isspace():          # the cut fell inside a word: drop it
        partial = _LAST_WORD.search(head)
        if partial is not None and partial.start() > 0:
            head = head[:partial.start()]
    return head.rstrip() + ELLIPSIS


@dataclass
class TurnView:
    """One turn's view as last decided; `body()` is what goes on the wire.
    `answer` is kept whole and capped only when sent. `sent_at` (loop time) and
    `trailing` (the send holding back a streamed answer) belong to the sender."""

    turn_id: str
    phase: ViewPhase = ViewPhase.LISTENING
    heard: str | None = None
    answer: str | None = None
    final: bool = False
    follow_up: bool = False
    sent_at: float | None = None
    trailing: asyncio.Task[None] | None = field(default=None, repr=False)

    def body(self) -> dict[str, object]:
        return {
            "turn_id": self.turn_id,
            "phase": self.phase,
            "heard": self.heard,
            "answer": None if self.answer is None else cap_answer(self.answer),
            "final": self.final,
            "follow_up": self.follow_up,
        }
