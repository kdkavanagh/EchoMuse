"""
em_button.py — what a dot-button release means, as a pure decision.

The device is sovereign over two things this module never overrides:

- **Alert stop (SPEC §6.3, WIRE §4.6).** A tap while an alert rings or is
  backgrounded stops it on the device, with no controller in the loop, and the
  device reports `handled: "alert_stopped"` with the `occurrence_id` it stopped.
  That press is consumed: it starts no turn and fires no HA event.
- **Privacy mute (SPEC §3.2 rule 10).** While muted a press never starts a
  voice turn. The device also refuses every uplink lease while muted, so a
  controller with a stale mute state can at worst open a turn that hears
  nothing.

Everything else about a press is controller policy and lives here.
"""

from __future__ import annotations

from enum import StrEnum


class ButtonAction(StrEnum):
    """What `decide` makes of a release."""

    # The device already stopped an alert with this press (`handled:"alert_stopped"`).
    ALERT_STOPPED = "alert_stopped"
    # A hold, forwarded to HA as the `long` event. Not speech, so mute does not block it.
    HOLD = "hold"
    # A tap forwarded to HA as an event instead of starting a turn
    # (buttonSingleTapEvent). Not speech, so mute does not block it.
    TAP_EVENT = "tap_event"
    # A tap while muted. Does nothing.
    BLOCKED = "blocked"
    # A tap during an active turn cancels it.
    CANCEL = "cancel"
    # A tap otherwise starts a turn.
    TURN = "turn"


class ButtonEvent(StrEnum):
    """HA event types of the Action-button event entity. Declaration order is
    the order advertised to HA at connect time."""

    LONG = "long"
    SINGLE = "single"
    DOUBLE = "double"
    TRIPLE = "triple"


def decide(
    *,
    held_ms: int,
    hold_ms: int,
    muted: bool,
    turn_active: bool,
    tap_event: bool = False,
    active_occurrence_id: str | None = None,
) -> ButtonAction:
    """
    Classify a dot-button release.

    `active_occurrence_id` is the occurrence the device reports having stopped
    with this press (`button.action` with `handled:"alert_stopped"`); None
    otherwise. It outranks every other meaning: the device has already acted
    on the press.

    `held_ms` is measured on the device; 0 reads as a tap, so an unknown hold
    time is never promoted to a gesture the user did not make.

    `tap_event` is buttonSingleTapEvent ANDed with the `button_hold`
    capability by the caller: the HA event entity exists only for a
    hold-capable device, and an ungated flag would turn every tap into an
    event nothing receives.
    """
    if active_occurrence_id is not None:
        return ButtonAction.ALERT_STOPPED
    if held_ms >= hold_ms:
        return ButtonAction.HOLD
    if tap_event:
        return ButtonAction.TAP_EVENT
    if muted:
        return ButtonAction.BLOCKED
    if turn_active:
        return ButtonAction.CANCEL
    return ButtonAction.TURN
