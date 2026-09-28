"""The STT copy is amplified; the rail must clamp, never wrap.

This hardware's mic stream peaks around -40 dBFS and faster-whisper fails on it
by silently dropping the quiet leading words -- "How many ounces are in a cup?"
arrives as "ounces are in a cup." (2026-09-20, EchoMuse turns 398-402). The fix
raises only the copy HA hears, so every room measurement keeps its calibration.

Amplifying int16 invites the one failure that would be worse than the quiet
audio it fixes: a wrapped sample is a full-scale sign flip, i.e. an impulse
injected into exactly the frames that were loud enough to matter. These pin the
clamp and the gain's actual magnitude.
"""
import math
import struct

import numpy as np

import em_stt_copy as es


def pcm(*samples: int) -> bytes:
    return struct.pack(f"<{len(samples)}h", *samples)


def unpack(raw: bytes) -> list[int]:
    return list(struct.unpack(f"<{len(raw) // 2}h", raw))


def test_gain_moves_the_level_by_the_decibels_asked_for():
    # 20 dB is a 10x linear scale -- the property the constant's measurements
    # were taken against.
    out, clipped = es.apply_asr_gain(pcm(100, -250, 37), gain_db=20.0)
    assert unpack(out) == [1000, -2500, 370]
    assert clipped == 0


def test_a_sample_past_the_rail_clamps_and_keeps_its_sign():
    # Wrapping would turn +30000 into a large negative number: a full-scale
    # discontinuity, which reads to a decoder as a click.
    out, clipped = es.apply_asr_gain(pcm(30000, -30000), gain_db=20.0)
    assert unpack(out) == [32767, -32768]
    assert clipped == 2


def test_clipped_count_is_only_the_samples_that_needed_clamping():
    # It drives an operator-facing warning, so an inflated count would send
    # someone lowering a gain that was fine.
    out, clipped = es.apply_asr_gain(pcm(30000, 10, -20, 25000), gain_db=20.0)
    assert clipped == 2
    assert unpack(out)[1:3] == [100, -200]


def test_zero_gain_is_an_untouched_passthrough():
    # A zero gain (a close talker at the band edge) must hand
    # back the original bytes, not a re-encoded copy.
    raw = pcm(1, -1, 500)
    out, clipped = es.apply_asr_gain(raw, gain_db=0.0)
    assert out is raw
    assert clipped == 0


def test_the_shipped_constant_leaves_headroom_for_this_hardware():
    # Worst peak measured across ten consecutive saved utterances was
    # -37.6 dBFS. The shipped gain must not put that on the rail, or every
    # loud syllable of a normal turn arrives clamped.
    worst_peak = int(32768 * 10 ** (-37.6 / 20))
    out, clipped = es.apply_asr_gain(pcm(worst_peak, -worst_peak))
    assert clipped == 0
    peak_dbfs = 20 * math.log10(max(abs(s) for s in unpack(out)) / 32768)
    assert -20.0 < peak_dbfs < -15.0


# ── the wake word as a level anchor ────────────────────────────────────────
#
# Peak frame-RMS of the wake clips for the eight turns that have both a wake
# clip and an utterance clip (2026-09-20). The band exists to be a no-op
# across this range: normalising against these 1:1 was measured to WIDEN the
# post-gain spread from 7.3 dB to 11.7 dB, because at a fixed listening
# position the wake word's scatter is uncorrelated noise (r = -0.17).
MEASURED_WAKE_DBFS = [-52.8, -49.6, -52.9, -46.2, -50.0, -49.5, -49.6, -52.7]


def test_every_turn_measured_so_far_gets_the_nominal_gain():
    # If this starts failing, the band moved under the fleet it was fitted to
    # and turns that were verified by ear are now being treated differently.
    for wake_db in MEASURED_WAKE_DBFS:
        assert es.asr_gain_for(wake_db) == es.ASR_GAIN_NOMINAL_DB, wake_db


def test_a_close_talker_is_backed_off_instead_of_clipped():
    # Someone speaking into the device: nominal gain would put them past the
    # rail, and a clamped transcript is worse than a quiet one.
    gain = es.asr_gain_for(-25.0)
    assert gain < es.ASR_GAIN_NOMINAL_DB
    assert -25.0 + gain == es.ASR_TARGET_MAX_DBFS


def test_a_distant_talker_is_pushed_toward_the_band():
    # Across the room, nominal still leaves them under the level that decodes.
    gain = es.asr_gain_for(-62.0)
    assert gain > es.ASR_GAIN_NOMINAL_DB
    assert -62.0 + gain <= es.ASR_TARGET_MIN_DBFS


def test_the_push_stops_at_the_ceiling():
    # A wake word detected in near-silence must not turn the room's noise
    # floor into the loudest thing HA hears.
    assert es.asr_gain_for(-90.0) == es.ASR_GAIN_CEIL_DB


def test_a_turn_with_no_wake_word_falls_back_to_nominal():
    # Button, continuation and announce-driven turns have no anchor; they get
    # the gain the fleet was measured at rather than no gain at all.
    assert es.asr_gain_for(None) == es.ASR_GAIN_NOMINAL_DB


# ── the STT copy as a whole ────────────────────────────────────────────────


def test_stt_copy_is_the_span_with_the_gain_and_nothing_else():
    # Without NS the copy is exactly the gained canonical span: same length,
    # nothing trimmed (§8.4: no audio is ever cut), clamped not wrapped.
    span = np.array([100, -250, 30000, 0], dtype=np.int16)
    out = es.stt_copy(span, gain=20.0, ns=False)
    assert unpack(out) == [1000, -2500, 32767, 0]


def test_stt_copy_leaves_the_canonical_span_untouched():
    # The same span feeds attribution and the wake clip; the copy must not
    # scale it in place.
    span = np.array([100, -100], dtype=np.int16)
    es.stt_copy(span, gain=20.0, ns=False)
    assert span.tolist() == [100, -100]
