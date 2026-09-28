"""
Dot-button gesture policy (em_button.decide).

The device stops a ringing alert itself and reports the press as handled; the
controller's policy covers everything else, with mute blocking only speech.
"""

import em_button

HOLD_MS = 750


def decide(held_ms=0, muted=False, turn_active=False, tap_event=False,
           active_occurrence_id=None):
    return em_button.decide(
        held_ms=held_ms,
        hold_ms=HOLD_MS,
        muted=muted,
        turn_active=turn_active,
        tap_event=tap_event,
        active_occurrence_id=active_occurrence_id,
    )


# ── The mute rule ────────────────────────────────────────────────────────────

def test_hold_fires_while_muted():
    """A hold is not speech, so the mute has no opinion about it."""
    assert decide(held_ms=800, muted=True) == em_button.HOLD


def test_hold_fires_while_muted_during_a_turn():
    """Mute cancels the turn on its own; the hold is still a hold."""
    assert decide(held_ms=800, muted=True, turn_active=True) == em_button.HOLD


def test_tap_starts_no_turn_while_muted():
    assert decide(held_ms=10, muted=True) == em_button.BLOCKED


def test_muted_tap_does_not_cancel_either():
    """Nothing about a muted press reaches the voice path."""
    assert decide(held_ms=10, muted=True, turn_active=True) == em_button.BLOCKED


# ── Unmuted behaviour is unchanged ───────────────────────────────────────────

def test_tap_starts_a_turn():
    assert decide(held_ms=10) == em_button.TURN


def test_tap_during_a_turn_cancels_it():
    assert decide(held_ms=10, turn_active=True) == em_button.CANCEL


def test_hold_fires_long():
    assert decide(held_ms=800) == em_button.HOLD


# ── The hold threshold ───────────────────────────────────────────────────────

def test_exactly_the_threshold_is_a_hold():
    assert decide(held_ms=HOLD_MS) == em_button.HOLD


def test_one_ms_under_is_a_tap():
    assert decide(held_ms=HOLD_MS - 1) == em_button.TURN


def test_absent_hold_time_reads_as_a_tap():
    """An unknown hold time must never be promoted to a gesture the user did
    not make."""
    assert decide(held_ms=0) == em_button.TURN
    assert decide(held_ms=0, muted=True) == em_button.BLOCKED


# ── buttonSingleTapEvent ─────────────────────────────────────────────────────

def test_tap_event_replaces_the_turn():
    assert decide(held_ms=10, tap_event=True) == em_button.TAP_EVENT


def test_tap_event_replaces_the_cancel():
    """The trade the setting makes: the button stops cancelling responses."""
    assert decide(held_ms=10, turn_active=True, tap_event=True) == em_button.TAP_EVENT


def test_tap_event_fires_while_muted():
    """Same reason a hold does — an event is not speech."""
    assert decide(held_ms=10, muted=True, tap_event=True) == em_button.TAP_EVENT


def test_hold_still_wins_over_tap_event():
    assert decide(held_ms=800, tap_event=True) == em_button.HOLD


def test_off_by_default_is_the_old_behaviour():
    """The caller ANDs the setting with button_hold capability, so an
    incapable device arrives here as tap_event=False and keeps its turn —
    rather than emitting to an entity HA was never offered."""
    assert decide(held_ms=10, tap_event=False) == em_button.TURN
    assert decide(held_ms=10, turn_active=True, tap_event=False) == em_button.CANCEL


# ── A press the device used to stop an alert ─────────────────────────────────

def test_a_device_stopped_alert_consumes_the_press():
    """The device already dismissed the occurrence; no turn, cancel or event."""
    for kw in ({}, {"turn_active": True}, {"muted": True}, {"tap_event": True}):
        assert decide(held_ms=10, active_occurrence_id="occ-1", **kw) == em_button.ALERT_STOPPED


def test_a_device_stopped_alert_outranks_a_hold():
    """Whatever the press length, the device acted on it; firing HA's `long`
    too would give one press two effects."""
    assert decide(held_ms=800, active_occurrence_id="occ-1") == em_button.ALERT_STOPPED


def test_without_an_occurrence_the_policy_is_unchanged():
    assert decide(held_ms=10, active_occurrence_id=None) == em_button.TURN
    assert decide(held_ms=10, muted=True, active_occurrence_id=None) == em_button.BLOCKED
