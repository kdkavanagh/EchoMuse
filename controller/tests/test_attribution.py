import numpy as np
import pytest

from em_attribution import (
    CELL,
    MAX_LAG,
    REPLY_OVERLAP,
    Attributor,
    BackgroundTracker,
    Cell,
    CellClass,
    Coverage,
    EarlyAnswerDetector,
    EchoResult,
    EchoTracker,
    PlaybackVerdict,
    Rule,
    WakeHop,
    compare_reference,
    estimate_lag,
    self_playback_verdict,
    wake_trigger_sample,
)


def classify(attributor, specs, *, background=-60.0, start=0):
    out = []
    for i, (level, vad, echo) in enumerate(specs):
        out.append(attributor.classify(Cell(start + i * CELL, level, vad, echo), background))
    return out


def test_background_is_prior_10_seconds_and_needs_one_second_support():
    tracker = BackgroundTracker()
    for i in range(32):
        cell = Cell(i * CELL, -60.0, None)  # pre-lease: no VAD, so valid cells count
        assert tracker.value(cell.start) is None
        tracker.observe(cell)
    cell = Cell(32 * CELL, -30.0, 0.9)
    assert tracker.value(cell.start) == pytest.approx(-60.0)
    tracker.observe(cell)  # known speech is excluded
    assert tracker.value(33 * CELL) == pytest.approx(-60.0)

    # Current-cell E is never in its own floor; enough quieter cells lower later B.
    first_low = Cell(33 * CELL, -80.0, 0.1)
    assert tracker.value(first_low.start) == pytest.approx(-60.0)
    for i in range(33, 45):
        tracker.observe(Cell(i * CELL, -80.0, 0.1))
    assert tracker.value(45 * CELL) == pytest.approx(-80.0)

    # Cells older than 10 s leave the window.
    later = 45 * CELL + 10 * 16_000
    assert tracker.value(later) is None


def test_wake_trigger_uses_last_raw_hop_at_threshold():
    assert wake_trigger_sample([WakeHop(2_560, 0.91), WakeHop(5_120, None), WakeHop(7_680, 0.90)], 0.90) == 7_680
    with pytest.raises(ValueError):
        wake_trigger_sample([WakeHop(2_560, 0.89)], 0.90)


def test_wake_seed_sets_foreground_only_after_trigger_and_never_relabels():
    trigger = 10 * CELL
    attr = Attributor(trigger_sample=trigger, seed_start=0)
    before = classify(attr, [(-30.0, 0.7, None)] * 10, start=0)
    assert attr.foreground is None
    assert all(c.cls == CellClass.COMMAND_SPEECH and not c.command for c in before)
    first_command = attr.classify(Cell(trigger, -30.0, 0.7), -60.0)
    assert attr.foreground == pytest.approx(-30.0)
    assert first_command.cls == CellClass.COMMAND_SPEECH and first_command.command
    assert all(not c.command for c in before)


def test_without_ten_wake_cells_f_waits_for_ten_command_cells():
    trigger = 5 * CELL
    attr = Attributor(trigger_sample=trigger, seed_start=0)
    classify(attr, [(-30.0, 0.7, None)] * 5)
    for i in range(9):
        attr.classify(Cell(trigger + i * CELL, -32.0, 0.7), -60.0)
        assert attr.foreground is None
    attr.classify(Cell(trigger + 9 * CELL, -32.0, 0.7), -60.0)
    assert attr.foreground == pytest.approx(-32.0)


def test_fan_tv_equal_tv_soft_syllable_and_earcon_sequences():
    trigger = 10 * CELL

    # Fan/dishwasher remains non-speech after a command.
    fan = Attributor(trigger_sample=trigger, seed_start=0)
    classify(fan, [(-30.0, 0.7, None)] * 11)
    assert classify(fan, [(-35.0, 0.2, None)] * 3, start=11 * CELL)[0].cls == CellClass.NON_SPEECH

    # A real pause closes hysteresis; TV ≥10 dB down becomes background speech.
    quiet_tv = Attributor(trigger_sample=trigger, seed_start=0)
    classify(quiet_tv, [(-30.0, 0.7, None)] * 11)
    classify(quiet_tv, [(-60.0, 0.1, None)], start=11 * CELL)
    tv = classify(quiet_tv, [(-41.0, 0.8, None)] * 2, start=12 * CELL)
    assert [c.cls for c in tv] == [CellClass.BACKGROUND_SPEECH, CellClass.BACKGROUND_SPEECH]

    # Equal-level TV cannot be separated: it opens another command run.
    equal_tv = Attributor(trigger_sample=trigger, seed_start=0)
    classify(equal_tv, [(-30.0, 0.7, None)] * 11)
    classify(equal_tv, [(-60.0, 0.1, None)], start=11 * CELL)
    assert classify(equal_tv, [(-30.0, 0.8, None)], start=12 * CELL)[0].cls == CellClass.COMMAND_SPEECH

    # A soft final syllable stays in a command run through hysteresis.
    soft = Attributor(trigger_sample=trigger, seed_start=0)
    classify(soft, [(-30.0, 0.7, None)] * 11)
    final = classify(soft, [(-41.0, 0.45, None)], start=11 * CELL)[0]
    assert final.cls == CellClass.COMMAND_SPEECH and final.rule == "hysteresis"

    # An earcon echo wins before VAD/level and breaks a command run.
    earcon = Attributor(trigger_sample=0)
    c = earcon.classify(Cell(0, -25.0, 0.99, EchoResult.ECHO_ONLY), -60.0)
    assert c.cls == CellClass.SELF_OUTPUT and c.rule == "echo"


def test_gap_and_mute_are_first_match_and_trace_is_run_length_encoded():
    attr = Attributor(trigger_sample=0)
    a = attr.classify(Cell(0, -60.0, 0.1), -70.0)
    b = attr.classify(Cell(CELL, -60.0, 0.1), -70.0)
    g = attr.classify(Cell(2 * CELL, None, gap=True), None)
    m = attr.classify(Cell(3 * CELL, None, muted=True), None)
    assert [a.cls, b.cls, g.cls, m.cls] == [CellClass.NON_SPEECH, CellClass.NON_SPEECH, CellClass.GAP, CellClass.GAP]
    assert [(s.cls, s.rule, s.cells) for s in attr.trace] == [
        (CellClass.NON_SPEECH, "vad", 2),
        (CellClass.GAP, "gap", 1),
        (CellClass.GAP, "mute", 1),
    ]


def echo_fixture(seed=2, cells=12, lag=731):
    rng = np.random.default_rng(seed)
    reference = rng.normal(0, 3_000, MAX_LAG + cells * CELL).astype(np.int16)
    offset = MAX_LAG - lag
    mic = np.round(reference[offset : offset + cells * CELL].astype(np.float64) * 0.45).astype(np.int16)
    valid = np.ones(cells, dtype=bool)
    vad = np.full(cells, 0.9)
    background = np.full(cells, -55.0)
    return mic, reference, valid, vad, background, lag


def test_reference_comparison_synthetic_echo_only_and_lag():
    mic, reference, valid, vad, background, lag = echo_fixture()
    assert estimate_lag(mic, reference, valid) == lag
    got = compare_reference(
        mic,
        reference,
        cell_valid=valid,
        vad=vad,
        background=background,
        coverage=Coverage.FULL,
    )
    assert got.lag == lag
    assert got.result == EchoResult.ECHO_ONLY


def test_reference_comparison_double_talk_is_near_end_present():
    mic, reference, valid, vad, background, lag = echo_fixture()
    rng = np.random.default_rng(99)
    near = rng.normal(0, 1_500, 2 * CELL)
    mic = mic.copy()
    mic[5 * CELL : 7 * CELL] = np.clip(
        mic[5 * CELL : 7 * CELL].astype(np.float64) + near, -32768, 32767
    ).astype(np.int16)
    got = compare_reference(
        mic,
        reference,
        cell_valid=valid,
        vad=vad,
        background=background,
        coverage=Coverage.FULL,
    )
    assert got.lag == lag
    assert got.result == EchoResult.NEAR_END_PRESENT


def test_reference_comparison_requires_full_coverage_and_valid_support():
    mic, reference, valid, vad, background, _ = echo_fixture()
    assert compare_reference(
        mic,
        reference,
        cell_valid=valid,
        vad=vad,
        background=background,
        coverage="partial",
    ).result == EchoResult.UNKNOWN
    valid[:3] = False  # 75% valid < required 80%
    assert compare_reference(
        mic,
        reference,
        cell_valid=valid,
        vad=vad,
        background=background,
        coverage=Coverage.FULL,
    ).result == EchoResult.UNKNOWN


def test_echo_tracker_reports_no_reference_and_unknown_without_lag():
    cells = 6
    mic = np.zeros(cells * CELL, dtype=np.int16)
    reference = np.zeros(MAX_LAG + cells * CELL, dtype=np.int16)
    tracker = EchoTracker(open_sample=0, lease_mic_start=0)
    got = tracker.label(
        mic,
        reference,
        cell_valid=np.ones(cells, bool),
        vad=[0.9] * cells,
        background=[-60.0] * cells,
        coverage=Coverage.FULL,
    )
    assert got == EchoResult.NO_REFERENCE
    reference[:] = 2_000
    assert tracker.label(
        mic,
        reference,
        cell_valid=np.ones(cells, bool),
        vad=[0.9] * cells,
        background=[-60.0] * cells,
        coverage=Coverage.FULL,
    ) == EchoResult.UNKNOWN


def test_echo_tracker_lag_window_uses_preceding_second_or_first_lease_second():
    preceding = EchoTracker(open_sample=32_000, lease_mic_start=0)
    start, end = preceding.estimate_window(32_000)
    assert end <= 32_000
    assert end - start >= 16_000
    preceding.update_lag(123)
    assert preceding.lag == 123
    assert preceding.estimate_window(end + 15_999) is None

    no_history = EchoTracker(open_sample=32_000, lease_mic_start=32_000)
    assert no_history.estimate_window(48_383) is None
    start, end = no_history.estimate_window(48_384)
    assert start == 32_000 and end == 48_384


def test_candidate_steps_one_and_two_order_and_double_talk_rescue():
    assert self_playback_verdict(reference_candidate_overlaps=True, comparison=EchoResult.UNKNOWN) == PlaybackVerdict.SELF_OUTPUT
    assert self_playback_verdict(reference_candidate_overlaps=True, comparison=EchoResult.NEAR_END_PRESENT) is None
    assert self_playback_verdict(reference_candidate_overlaps=False, comparison=EchoResult.ECHO_ONLY) == PlaybackVerdict.ECHO_ONLY
    # Echo-only still rejects after a reference-candidate double-talk rescue did not apply.
    assert self_playback_verdict(reference_candidate_overlaps=True, comparison=EchoResult.ECHO_ONLY) == PlaybackVerdict.SELF_OUTPUT


def test_early_answer_needs_fifteen_qualifying_consecutive_cells():
    detector = EarlyAnswerDetector()
    for i in range(14):
        assert detector.push(Cell(i * CELL, -35.0, 0.9, EchoResult.NEAR_END_PRESENT), -50.0) is None
    assert detector.push(Cell(14 * CELL, -35.0, 0.9, EchoResult.NEAR_END_PRESENT), -50.0) == 0

    broken = EarlyAnswerDetector()
    for i in range(10):
        broken.push(Cell(i * CELL, -35.0, 0.9, EchoResult.NO_REFERENCE), -50.0)
    broken.push(Cell(10 * CELL, -35.0, 0.9, EchoResult.ECHO_ONLY), -50.0)
    for i in range(11, 20):
        assert broken.push(Cell(i * CELL, -35.0, 0.9, EchoResult.NO_REFERENCE), -50.0) is None


def _vads(attributor, vads, first=0):
    """Classify one cell per VAD value from cell `first` (no level evidence against F)."""
    return classify(attributor, [(-40.0, v, EchoResult.NO_REFERENCE) for v in vads], start=first * CELL)


# Turn 617 (Kitchen, 2026-10-09), cell VAD from the device link after the reply chime: the
# answer's first word holds 0.85 for 6 cells, then dips. The onset gate never fired on it.
TURN_617_PAUSE = [0.04] * 12
TURN_617_ANSWER = [0.53, 0.80, 0.86, 0.90, 0.91, 0.90, 0.88, 0.86, 0.72, 0.73, 0.81, 0.77, 0.86, 0.98, 0.72,
                   0.37, 0.43, 0.51, 0.51, 0.99, 0.99, 0.99, 0.97, 0.84, 0.98, 0.99, 0.99, 0.99, 0.99, 0.99,
                   0.99, 0.93, 0.98]


def test_turn_617s_answer_is_command_speech_from_its_second_cell():
    attr = Attributor(trigger_sample=4 * CELL, reply=True)
    cells = _vads(attr, TURN_617_PAUSE + TURN_617_ANSWER)
    answer = cells[len(TURN_617_PAUSE):]
    assert answer[0].cls == CellClass.UNKNOWN                           # 0.53: below a run's opening
    assert all(c.cls == CellClass.COMMAND_SPEECH and c.command for c in answer[1:])
    assert attr.answer_start == (len(TURN_617_PAUSE) + 1) * CELL


@pytest.mark.parametrize("reply", [True, False])
def test_reply_speech_under_way_before_its_window_is_background_until_it_pauses(reply):
    trigger = 40 * CELL
    attr = Attributor(trigger_sample=trigger, reply=reply)
    under_way = _vads(attr, [0.9] * 50)                                 # from 40 cells before the trigger
    assert all(c.cls == CellClass.COMMAND_SPEECH and not c.command for c in under_way[:40])
    if reply:
        assert all(c.cls == CellClass.BACKGROUND_SPEECH and c.rule == Rule.UNDER_WAY and not c.command
                   for c in under_way[40:])
        assert attr.answer_start is None
    else:                                                               # wake and button: unaffected
        assert all(c.cls == CellClass.COMMAND_SPEECH and c.command for c in under_way[40:])
    _vads(attr, [0.1] * 4, first=50)
    answer = _vads(attr, [0.9] * 10, first=54)
    assert all(c.cls == CellClass.COMMAND_SPEECH and c.command for c in answer)
    assert attr.answer_start == 54 * CELL


@pytest.mark.parametrize("lead, taken", [(REPLY_OVERLAP, True), (REPLY_OVERLAP + CELL, False)])
def test_an_answer_may_start_up_to_480_ms_before_the_reply_window(lead, taken):
    trigger = 40 * CELL
    attr = Attributor(trigger_sample=trigger, reply=True)
    first = (trigger - lead) // CELL
    _vads(attr, [0.05] * first)
    cells = _vads(attr, [0.9] * 30, first=first)
    after = [c for c in cells if c.end > trigger]
    assert all(c.command for c in after) is taken and any(c.command for c in after) is taken
    assert attr.answer_start == (first * CELL if taken else None)
