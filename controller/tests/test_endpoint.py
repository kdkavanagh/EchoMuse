"""Relative endpointing must beat a television without cutting a real talker.

The failure these guard against: for a wake turn, HA's VAD is the only thing
that ends the mic stream, and it asks "is there speech?". With a TV on the
answer never becomes no, so the turn runs to the 20s hard cap.

The scenario tests are written in dBFS levels rather than synthesised audio,
because the module's whole decision is a level trajectory and a WAV would only
add a Fourier transform between the test and what it is asserting.
"""
import math

import pytest

import em_endpoint as ep

SUB_PER_S = 1000 // ep.SUBFRAME_MS  # 50 sub-frames per second


def run(endpointer, db, seconds):
    """Feed a constant level for `seconds`. Returns the stop reason or None."""
    for _ in range(int(SUB_PER_S * seconds)):
        if endpointer.push_db(db):
            return endpointer.stop_reason
    return endpointer.stop_reason


# ── the scenario this exists for ───────────────────────────────────────────

def test_a_television_does_not_hold_the_turn_open():
    # Near-field speaker at -30 dBFS, TV bed at -48. The user says their
    # command and stops; the TV carries on. This is the reported bug.
    e = ep.Endpointer()
    e.seed(-30.0)
    assert run(e, -30.0, 1.5) is None, "must not endpoint while they are talking"
    assert run(e, -48.0, 2.0) == "relative_silence"


def test_it_endpoints_on_true_silence_too():
    # The ordinary quiet-room turn still has to end, and by the same path.
    e = ep.Endpointer()
    e.seed(-30.0)
    run(e, -30.0, 1.5)
    assert run(e, -75.0, 2.0) == "relative_silence"


def test_a_talker_who_stays_at_their_own_level_is_never_cut():
    # 30 seconds of continuous speech at a steady level. The maximum tracker
    # hunts down, so the threshold follows the speaker rather than overtaking
    # them — the property that makes the decay safe.
    e = ep.Endpointer()
    e.seed(-30.0)
    assert run(e, -30.0, 30.0) is None


def test_a_quiet_talker_in_a_quiet_room_is_never_cut():
    # No TV. The speaker is 25 dB down on the last one; every threshold is
    # relative, so nothing about this case may differ.
    e = ep.Endpointer()
    e.seed(-55.0)
    assert run(e, -55.0, 10.0) is None


def test_mid_sentence_pauses_do_not_end_the_turn():
    # "Set a timer for … five minutes". A 600ms gap is under the 1.2s window,
    # and the N-of-M vote must not carry it over into the next pause.
    e = ep.Endpointer()
    e.seed(-30.0)
    for _ in range(4):
        assert run(e, -30.0, 1.0) is None
        assert run(e, -70.0, 0.6) is None, "a 600ms pause is not an endpoint"


# ── the guards ─────────────────────────────────────────────────────────────

def test_min_speech_vetoes_an_immediate_stop():
    # Wake word, then nothing. The no-speech path in em_turnclock owns this
    # case; the endpointer must not race it with a 200ms decision.
    e = ep.Endpointer()
    e.seed(-30.0)
    for _ in range(int(SUB_PER_S * (ep.DEFAULT_CONFIG.min_speech_ms / 1000.0)) - 1):
        assert e.push_db(-90.0) is None
    assert e.elapsed_ms < ep.DEFAULT_CONFIG.min_speech_ms


def test_the_window_keeps_filling_during_the_min_speech_veto():
    # The veto must not also reset the vote — otherwise the guard costs a full
    # window of latency on every turn instead of none. Silence from the very
    # first sub-frame should endpoint at min_speech, not min_speech + window.
    e = ep.Endpointer()
    e.seed(-30.0)
    cfg = ep.DEFAULT_CONFIG
    fired_at = None
    for _ in range(int(SUB_PER_S * 5)):
        if e.push_db(-90.0):
            fired_at = e.elapsed_ms
            break
    assert fired_at is not None
    assert fired_at <= cfg.min_speech_ms + cfg.stop_window * ep.SUBFRAME_MS * 0.25, (
        f"endpointed at {fired_at}ms — the vote was reset by the min_speech veto"
    )


def test_the_stop_reason_latches():
    # The caller starts a backporch on the first truthy return and must not be
    # handed a second decision when it keeps feeding the tail through.
    e = ep.Endpointer()
    e.seed(-30.0)
    run(e, -30.0, 1.5)
    assert run(e, -80.0, 2.0) == "relative_silence"
    assert e.push_db(-10.0) == "relative_silence", "a loud tail must not un-stop it"


def test_seed_only_ever_raises_the_anchor():
    # The tail of a wake word is quiet. Seeding down to it would place the
    # threshold below the speaker's real level for the whole turn.
    e = ep.Endpointer()
    e.seed(-30.0)
    e.seed(-60.0)
    assert e.max_db == -30.0


def test_an_unanchored_turn_still_works():
    # Button turns have no wake word. The trackers converge on the turn's own
    # audio instead; this must not crash or endpoint instantly.
    e = ep.Endpointer()
    assert run(e, -30.0, 3.0) is None
    assert run(e, -55.0, 2.0) == "relative_silence"


# ── the maths ──────────────────────────────────────────────────────────────

def test_threshold_sits_at_low_per_mil_of_the_range():
    # With a real range the two anchorings agree exactly — measuring 40% up
    # from the floor and 60% down from the peak are the same point.
    e = ep.Endpointer(ep.EndpointConfig(low_per_mil=400, min_range_db=1.0))
    e.max_db, e.min_db = -20.0, -60.0
    assert e.low_threshold == pytest.approx(-60.0 + 0.4 * 40.0)
    assert e.low_threshold == pytest.approx(-20.0 - 0.6 * 40.0)


def test_a_clamped_range_is_measured_down_from_the_peak():
    # Where they disagree, the peak is the correct anchor. Measured up from a
    # floor that has converged onto the signal, the threshold lands ABOVE it
    # and the speaker endpoints themselves — the bug
    # test_a_talker_who_stays_at_their_own_level_is_never_cut found.
    e = ep.Endpointer(ep.EndpointConfig(low_per_mil=400, min_range_db=12.0))
    e.max_db, e.min_db = -50.0, -51.0
    assert e.low_threshold == pytest.approx(-50.0 - 0.6 * 12.0)
    assert e.low_threshold < e.max_db


def test_the_trackers_cannot_cross():
    # The two hunts move independently. A minimum above the maximum inverts
    # the threshold and would stop the turn instantly.
    e = ep.Endpointer()
    for _ in range(SUB_PER_S * 60):
        e.push_db(-45.0)
        assert e.min_db <= e.max_db


def test_silence_reads_as_a_number_not_negative_infinity():
    import numpy as np
    assert ep.rms_dbfs(np.zeros(320, dtype=np.int16)) == ep.DB_FLOOR
    assert math.isfinite(ep.rms_dbfs(np.zeros(320, dtype=np.int16)))


def test_full_scale_reads_near_zero_dbfs():
    import numpy as np
    full = np.full(320, 32767, dtype=np.int16)
    assert ep.rms_dbfs(full) == pytest.approx(0.0, abs=0.01)


def test_push_pcm_consumes_whole_sub_frames():
    import numpy as np
    e = ep.Endpointer()
    # One 80ms frame at 16kHz mono S16LE = 1280 samples = 4 sub-frames.
    e.push_pcm(np.zeros(1280, dtype=np.int16).tobytes(), np)
    assert e.elapsed_ms == 4 * ep.SUBFRAME_MS


# ── config validation ──────────────────────────────────────────────────────

@pytest.mark.parametrize("kwargs", [
    {"low_per_mil": -1},
    {"low_per_mil": 1001},
    {"stop_window": 0},
    {"stop_count": 0},
    {"stop_count": 999},          # > stop_window
    {"min_range_db": 0.0},
    {"max_hunt_db_per_s": 1.0},   # must be negative
    {"min_hunt_db_per_s": -1.0},  # must be positive
])
def test_nonsense_config_is_refused(kwargs):
    # libpryon asserts the same relationships on its own config. A bad
    # combination here does not crash, it silently never endpoints — which is
    # the exact failure this module exists to fix, so it must be loud.
    with pytest.raises(ValueError):
        ep.EndpointConfig(**kwargs)


def test_defaults_are_slower_than_home_assistants_vad():
    # The whole safety argument: this is a ceiling, not a competitor. HA
    # endpoints on ~0.5-1.0s of silence, so a quiet room must still be HA's
    # turn to call. If this window ever drops below ~1s, that stops being true
    # and every turn's behaviour changes, not just the stuck ones.
    cfg = ep.DEFAULT_CONFIG
    window_ms = cfg.stop_window * ep.SUBFRAME_MS
    assert window_ms >= 1000, "endpointer would start beating HA on normal turns"
    assert cfg.stop_count / cfg.stop_window >= 0.75


def test_config_for_defaults_reproduce_the_module_defaults():
    # The dashboard's defaults and DEFAULT_CONFIG must be the same endpointer.
    # They are written down in three places (em_db, dashboard.jsx, here), and
    # a silent disagreement would mean the shipped behaviour is not the one
    # any of the reasoning above was done against.
    cfg = ep.config_for(silence_ms=1200, low_per_mil=400)
    assert cfg.stop_window == ep.DEFAULT_CONFIG.stop_window
    assert cfg.stop_count == ep.DEFAULT_CONFIG.stop_count
    assert cfg.low_per_mil == ep.DEFAULT_CONFIG.low_per_mil


@pytest.mark.parametrize("silence_ms", [800, 1000, 1200, 1800, 2500, 3000])
def test_config_for_holds_the_ratio_and_stays_valid(silence_ms):
    cfg = ep.config_for(silence_ms=silence_ms, low_per_mil=400)
    assert cfg.stop_window * ep.SUBFRAME_MS == pytest.approx(silence_ms, abs=ep.SUBFRAME_MS)
    assert cfg.stop_count / cfg.stop_window == pytest.approx(ep.STOP_RATIO, abs=0.05)


def test_config_for_survives_a_nonsense_setting():
    # A slider cannot produce these, but the fleet config is a JSON blob an
    # operator can edit. The constructor's assertions must not be reachable
    # from a value someone typed — refusing here would cost the turn its
    # endpointing entirely, which is the broken behaviour.
    cfg = ep.config_for(silence_ms=0, low_per_mil=400)
    assert cfg.stop_window >= 1
    assert 1 <= cfg.stop_count <= cfg.stop_window


def test_a_longer_pause_setting_tolerates_a_longer_pause():
    # The knob has to do what its label says: at 3000ms a 2s mid-sentence
    # pause must survive, where the 1200ms default would have ended the turn.
    slow = ep.Endpointer(ep.config_for(silence_ms=3000, low_per_mil=400))
    slow.seed(-30.0)
    run(slow, -30.0, 1.5)
    assert run(slow, -70.0, 2.0) is None

    quick = ep.Endpointer(ep.config_for(silence_ms=1200, low_per_mil=400))
    quick.seed(-30.0)
    run(quick, -30.0, 1.5)
    assert run(quick, -70.0, 2.0) == "relative_silence"


def test_backporch_is_not_zero():
    # A voting window has by construction already spent part of itself on
    # audio the user was still producing. Without a tail the last word is
    # clipped on exactly the turns this feature is meant to rescue.
    assert ep.BACKPORCH_MS >= 100
