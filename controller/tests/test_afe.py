"""em_afe: native AFE span summaries, playback onset, turn evidence, and
wake.stats `afe` — absent data is "no data", never zeros."""

import json

import pytest

import em_afe
import em_audio_timeline as at


def rec(start, frames=10, **kw):
    fields = dict(frames=frames, flags=0, playback=0, erle_max=0, erle_mean=0, erle_frames=0, dtd_max=0,
                  dtd_frames=0, rms_max=0, rms_mean=0, vad_max=0, vad_frames=0, volume=70, lost=0)
    fields.update(kw)
    return at.AfeRecord(start=start, **fields)


def empty(start, **kw):
    """A period the device decoded no frame for: no data, its value bytes zero."""
    return rec(start, frames=0, volume=0, **kw)


# --- summaries --------------------------------------------------------------------------


def test_a_span_without_records_has_no_summary():
    assert em_afe.summarize([], 0, 12_800) is None


def test_a_span_whose_records_all_have_zero_frames_has_no_summary_not_a_zero_one():
    records = [empty(0), empty(1280, lost=4, flags=at.AFE_FLAG_GAP), empty(2560)]
    assert em_afe.summarize(records, 0, 3840) is None


def test_records_outside_the_span_do_not_count():
    assert em_afe.summarize([rec(0), rec(5120)], 1280, 5120) is None
    assert em_afe.summarize([rec(0)], 500, 500) is None                 # an empty span


def test_a_summary_covers_every_period_overlapping_the_span():
    records = [
        rec(0, volume=60),                                                # before the span
        rec(1280, playback=4, erle_max=20, erle_mean=10, erle_frames=2, dtd_max=31, dtd_frames=1,
            rms_max=216, rms_mean=206, vad_max=2, vad_frames=3, flags=at.AFE_FLAG_GAP),
        empty(2560, lost=3),                                              # received, no data
        # period 3 (3840) never arrived
        rec(5120, frames=8, playback=5, erle_max=26, erle_mean=16, erle_frames=4, vad_max=1,
            vad_frames=1, volume=71, flags=at.AFE_FLAG_OUTPUT_CLIPPED | at.AFE_FLAG_SYNC),
        rec(6400, volume=99),                                             # after the span
    ]
    s = em_afe.summarize(records, 2000, 6000)
    assert s is not None
    assert (s.start, s.end) == (2000, 6000)
    assert (s.periods_expected, s.periods_received, s.periods_with_frames) == (4, 3, 2)
    assert (s.frames, s.frames_expected, s.playback_frames) == (18, 40, 9)
    assert (s.erle_max, s.erle_frames) == (26, 6)
    assert s.erle_mean == pytest.approx((10 * 2 + 16 * 4) / 6)          # non-zero mean, frame-weighted
    assert (s.dtd_max, s.dtd_frames) == (1.0, 1)
    assert (s.rms_max_db, s.rms_mean_db) == (-40, -50.0)                 # only period 1 computed RMS
    assert (s.vad_max, s.vad_frames) == (0.5, 4)
    assert s.volume == 71                                                # the last period with frames
    assert (s.gaps, s.syncs, s.output_clipped, s.mic_clipped, s.aec_diverged, s.device_mute) == (1, 1, 1, 0, 0, 0)
    assert s.lost_frames == 3


def test_without_far_end_energy_or_rms_those_values_are_absent_not_zero():
    s = em_afe.summarize([rec(0, vad_max=3, vad_frames=10)], 0, 1280)
    assert s is not None
    assert s.erle_mean is None and s.erle_frames == 0
    assert s.rms_max_db is None and s.rms_mean_db is None
    assert s.vad_max == 0.75


def test_a_summary_round_trips_through_its_wire_form():
    s = em_afe.summarize([rec(1280, playback=10, erle_max=9, erle_mean=5, erle_frames=10, rms_max=200,
                              rms_mean=190)], 1280, 2560)
    assert em_afe.AfeSummary.from_wire(json.loads(json.dumps(s.wire()))) == s
    with pytest.raises(ValueError):
        em_afe.AfeSummary.from_wire({**s.wire(), "frames": -1})


# --- playback onset -------------------------------------------------------------------


def test_the_onset_is_the_last_rise_placed_where_the_periods_playback_frames_begin():
    records = [rec(0), rec(1280, playback=3), rec(2560, playback=10), rec(3840), rec(5120),
               rec(6400, playback=6), rec(7680, playback=10)]
    # Second rise: period 6400 has its last 6 of 10 frames in playback.
    assert em_afe.playback_onset(records, 9000) == 6400 + 4 * 128
    assert em_afe.playback_onset(records, 6000) == 1280 + 7 * 128       # only the first rise is before 6000


def test_a_rise_after_the_support_start_is_not_the_onset():
    records = [rec(0), rec(1280, playback=3), rec(2560), rec(3840, playback=2)]
    # The second rise lands at 3840 + 1024, after 4000: the last rise at or before it is the first.
    assert em_afe.playback_onset(records, 4000) == 1280 + 7 * 128


@pytest.mark.parametrize("records", [
    [rec(0, playback=10), rec(1280, playback=10)],          # playing since before the first record
    [rec(0), empty(1280), rec(2560, playback=5)],           # the period before the rise had no data
    [rec(0), rec(2560, playback=5)],                        # the period before the rise never arrived
    [rec(0), rec(1280)],                                    # no playback at all
    [],
])
def test_an_onset_that_was_not_received_is_no_data(records):
    assert em_afe.playback_onset(records, 10_000) is None


def test_a_run_whose_start_was_lost_hides_an_older_rise():
    records = [rec(0), rec(1280, playback=10), rec(2560), empty(3840), rec(5120, playback=10)]
    assert em_afe.playback_onset(records, 7000) is None


# --- turn evidence --------------------------------------------------------------------


def _store(records):
    store = at.AfeTimeline()
    for r in records:
        rows = at.parse_afe(at.build_afe([r]))
        store.write(r.start, rows)
    return store


def test_turn_evidence_brackets_the_support_window_and_the_utterance():
    support_start, support_end = 48_000, 60_800
    store = _store([rec(k * 1280, playback=10 if k >= 30 else 0, erle_max=12 if k >= 30 else 0,
                        erle_mean=8 if k >= 30 else 0, erle_frames=10 if k >= 30 else 0)
                    for k in range(15, 70)])
    ev = em_afe.turn_evidence(store, (support_start, support_end), (43_200, 80_000))
    assert ev.support_start == support_start
    assert ev.pre is not None and (ev.pre.start, ev.pre.end) == (32_000, 48_000)
    assert ev.wake is not None and (ev.wake.start, ev.wake.end) == (support_start, support_end)
    assert ev.playback_onset == 30 * 1280                     # all 10 frames of period 30 in playback
    assert ev.utterance is not None and ev.utterance.periods_expected == 63 - 33
    assert em_afe.AfeEvidence.loads(ev.dumps()) == ev


def test_turn_evidence_without_a_candidate_or_records_is_no_data():
    ev = em_afe.turn_evidence(_store([]), None, (0, 12_800))
    assert ev == em_afe.AfeEvidence(None, None, None, None, None)
    assert json.loads(ev.dumps()) == {"support_start": None, "pre": None, "wake": None,
                                      "playback_onset": None, "utterance": None}
    ev = em_afe.turn_evidence(_store([empty(0), empty(1280)]), (0, 2560), None)
    assert (ev.support_start, ev.pre, ev.wake, ev.playback_onset) == (0, None, None, None)


def test_stored_evidence_with_absent_fields_reads_as_no_data():
    assert em_afe.AfeEvidence.loads('{"wake": null}') == em_afe.AfeEvidence(None, None, None, None, None)
    with pytest.raises(ValueError):
        em_afe.AfeEvidence.loads("[]")
    with pytest.raises(ValueError):
        em_afe.AfeEvidence.loads('{"wake": {"start": 0}}')


# --- wake.stats afe -------------------------------------------------------------------


AFE_STATS = {"periods": 375, "frames": 3740, "invalid": 1, "syncs": 0, "gaps": 2, "lost_frames": 10}


def test_wake_stats_afe_parses_into_counters():
    stats = em_afe.parse_stats({"hops_scored": 187, "afe": AFE_STATS})
    assert stats == em_afe.AfeStats(375, 3740, 1, 0, 2, 10)
    assert stats.wire() == {**AFE_STATS, "frames_expected": 3750}


def test_absent_wake_stats_afe_is_no_data_not_zeros():
    assert em_afe.parse_stats({"hops_scored": 187}) is None
    assert em_afe.parse_stats({"afe": None}) is None


@pytest.mark.parametrize("afe", [
    "375", [], {**AFE_STATS, "frames": -1}, {**AFE_STATS, "gaps": True}, {**AFE_STATS, "invalid": 1.5},
    {k: v for k, v in AFE_STATS.items() if k != "lost_frames"},
])
def test_malformed_wake_stats_afe_is_refused(afe):
    with pytest.raises(ValueError):
        em_afe.parse_stats({"afe": afe})
