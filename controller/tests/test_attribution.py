import numpy as np
import pytest

from em_attribution import (
    BACKGROUND_SPEECH,
    CELL,
    COMMAND_SPEECH,
    COVERAGE_FULL,
    ECHO_ONLY,
    GAP,
    MAX_LAG,
    NEAR_END_PRESENT,
    NON_SPEECH,
    NO_REFERENCE,
    SELF_OUTPUT,
    UNKNOWN,
    Attributor,
    BackgroundTracker,
    Cell,
    EarlyAnswerDetector,
    EchoTracker,
    ReplyOnsetScanner,
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
    assert wake_trigger_sample([(2_560, 0.91), (5_120, None), (7_680, 0.90)], 0.90) == 7_680
    assert wake_trigger_sample([{"end_sample": "2560", "raw": 0.91}], 0.90) == 2_560
    with pytest.raises(ValueError):
        wake_trigger_sample([(2_560, 0.89)], 0.90)


def test_wake_seed_sets_foreground_only_after_trigger_and_never_relabels():
    trigger = 10 * CELL
    attr = Attributor(trigger_sample=trigger, seed_start=0)
    before = classify(attr, [(-30.0, 0.7, None)] * 10, start=0)
    assert attr.foreground is None
    assert all(c.cls == COMMAND_SPEECH and not c.command for c in before)
    first_command = attr.classify(Cell(trigger, -30.0, 0.7), -60.0)
    assert attr.foreground == pytest.approx(-30.0)
    assert first_command.cls == COMMAND_SPEECH and first_command.command
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
    assert classify(fan, [(-35.0, 0.2, None)] * 3, start=11 * CELL)[0].cls == NON_SPEECH

    # A real pause closes hysteresis; TV ≥10 dB down becomes background speech.
    quiet_tv = Attributor(trigger_sample=trigger, seed_start=0)
    classify(quiet_tv, [(-30.0, 0.7, None)] * 11)
    classify(quiet_tv, [(-60.0, 0.1, None)], start=11 * CELL)
    tv = classify(quiet_tv, [(-41.0, 0.8, None)] * 2, start=12 * CELL)
    assert [c.cls for c in tv] == [BACKGROUND_SPEECH, BACKGROUND_SPEECH]

    # Equal-level TV cannot be separated: it opens another command run.
    equal_tv = Attributor(trigger_sample=trigger, seed_start=0)
    classify(equal_tv, [(-30.0, 0.7, None)] * 11)
    classify(equal_tv, [(-60.0, 0.1, None)], start=11 * CELL)
    assert classify(equal_tv, [(-30.0, 0.8, None)], start=12 * CELL)[0].cls == COMMAND_SPEECH

    # A soft final syllable stays in a command run through hysteresis.
    soft = Attributor(trigger_sample=trigger, seed_start=0)
    classify(soft, [(-30.0, 0.7, None)] * 11)
    final = classify(soft, [(-41.0, 0.45, None)], start=11 * CELL)[0]
    assert final.cls == COMMAND_SPEECH and final.rule == "hysteresis"

    # An earcon echo wins before VAD/level and breaks a command run.
    earcon = Attributor(trigger_sample=0)
    c = earcon.classify(Cell(0, -25.0, 0.99, ECHO_ONLY), -60.0)
    assert c.cls == SELF_OUTPUT and c.rule == "echo"


def test_gap_and_mute_are_first_match_and_trace_is_run_length_encoded():
    attr = Attributor(trigger_sample=0)
    a = attr.classify(Cell(0, -60.0, 0.1), -70.0)
    b = attr.classify(Cell(CELL, -60.0, 0.1), -70.0)
    g = attr.classify(Cell(2 * CELL, None, gap=True), None)
    m = attr.classify(Cell(3 * CELL, None, muted=True), None)
    assert [a.cls, b.cls, g.cls, m.cls] == [NON_SPEECH, NON_SPEECH, GAP, GAP]
    assert [(s.cls, s.rule, s.cells) for s in attr.trace] == [
        (NON_SPEECH, "vad", 2),
        (GAP, "gap", 1),
        (GAP, "mute", 1),
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
        coverage=COVERAGE_FULL,
    )
    assert got.lag == lag
    assert got.result == ECHO_ONLY


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
        coverage=COVERAGE_FULL,
    )
    assert got.lag == lag
    assert got.result == NEAR_END_PRESENT


def test_reference_comparison_requires_full_coverage_and_valid_support():
    mic, reference, valid, vad, background, _ = echo_fixture()
    assert compare_reference(
        mic,
        reference,
        cell_valid=valid,
        vad=vad,
        background=background,
        coverage="partial",
    ).result == UNKNOWN
    valid[:3] = False  # 75% valid < required 80%
    assert compare_reference(
        mic,
        reference,
        cell_valid=valid,
        vad=vad,
        background=background,
        coverage=COVERAGE_FULL,
    ).result == UNKNOWN


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
        coverage=COVERAGE_FULL,
    )
    assert got == NO_REFERENCE
    reference[:] = 2_000
    assert tracker.label(
        mic,
        reference,
        cell_valid=np.ones(cells, bool),
        vad=[0.9] * cells,
        background=[-60.0] * cells,
        coverage=COVERAGE_FULL,
    ) == UNKNOWN


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
    assert self_playback_verdict(reference_candidate_overlaps=True, comparison=UNKNOWN) == SELF_OUTPUT
    assert self_playback_verdict(reference_candidate_overlaps=True, comparison=NEAR_END_PRESENT) is None
    assert self_playback_verdict(reference_candidate_overlaps=False, comparison=ECHO_ONLY) == ECHO_ONLY
    # Echo-only still rejects after a reference-candidate double-talk rescue did not apply.
    assert self_playback_verdict(reference_candidate_overlaps=True, comparison=ECHO_ONLY) == SELF_OUTPUT


def test_early_answer_needs_fifteen_qualifying_consecutive_cells():
    detector = EarlyAnswerDetector()
    for i in range(14):
        assert detector.push(Cell(i * CELL, -35.0, 0.9, NEAR_END_PRESENT), -50.0) is None
    assert detector.push(Cell(14 * CELL, -35.0, 0.9, NEAR_END_PRESENT), -50.0) == 0

    broken = EarlyAnswerDetector()
    for i in range(10):
        broken.push(Cell(i * CELL, -35.0, 0.9, NO_REFERENCE), -50.0)
    broken.push(Cell(10 * CELL, -35.0, 0.9, ECHO_ONLY), -50.0)
    for i in range(11, 20):
        assert broken.push(Cell(i * CELL, -35.0, 0.9, NO_REFERENCE), -50.0) is None


def test_reply_onset_after_drain_accepts_tail_answer_and_requires_quiet_prefix():
    drain = 20 * CELL
    scanner = ReplyOnsetScanner(drain)
    # 10 quiet cells; answer starts 4 cells before drain, within the retained-tail bound.
    for i in range(6, 16):
        scanner.push(Cell(i * CELL, -60.0, 0.1, NO_REFERENCE))
    for i in range(16, 23):
        assert scanner.push(Cell(i * CELL, -30.0, 0.9, NEAR_END_PRESENT)) is None
    assert scanner.push(Cell(23 * CELL, -30.0, 0.9, NEAR_END_PRESENT)) == 16 * CELL
