"""Per-utterance composition (em_utterance): routes, closes, local commands, reply onsets.

Evidence is synthetic: one level/VAD value per 32 ms cell and streaming-ASR
tokens with emission samples, fed in sample order exactly as the actor does.
"""

from __future__ import annotations

from echomuse_grammar import AMPM_CHOICES, CommandContext
from em_attribution import CELL, Cell, EchoResult
from em_audio_timeline import CELL_FLAG_GAP, build_cells
from em_utterance import (
    BLOCK,
    CellAssembler,
    Close,
    Commit,
    Evidence,
    Pending,
    ReplyWatch,
    Revoked,
    Utterance,
    UtteranceSpec,
)

SPEECH_DB, QUIET_DB, ROOM_DB = -30.0, -70.0, -70.0


def spec(kind="button", start=0, trigger=4800, **kw) -> UtteranceSpec:
    return UtteranceSpec("u1", kind, start, trigger, **kw)


def drive(u: Utterance, cell, tokens, until: int, *, gap_at: int | None = None):
    """Feed cells (`cell(start) -> (level, vad)`) and ASR results block by block."""
    decisions = []
    next_cell = u.spec.start - u.spec.start % CELL
    last_emission = None
    for block_end in range(BLOCK, until + BLOCK, BLOCK):
        while next_cell + CELL <= block_end:
            gap = gap_at is not None and next_cell <= gap_at < next_cell + CELL
            level, vad = cell(next_cell)
            u.push_cell(Evidence(Cell(next_cell, None if gap else level, None if gap else vad, None, gap), ROOM_DB))
            next_cell += CELL
        if block_end <= u.spec.start:
            continue
        emitted = [(t, s) for t, s in tokens if s <= block_end]
        if emitted:
            last_emission = emitted[-1][1]
        blanks = (block_end - (last_emission if last_emission is not None else u.spec.start)) // 640
        u.push_asr([t for t, _ in emitted], [s for _, s in emitted], blanks, block_end)
        decisions += u.advance(block_end)
        if u.done:
            break
    return decisions


def speech_between(*spans, level=SPEECH_DB, vad=0.95):
    def cell(start):
        for a, b in spans:
            if a <= start < b:
                return level, vad
        return QUIET_DB, 0.05
    return cell


KITCHEN = [("▁TURN", 6_000), ("▁ON", 7_000), ("▁THE", 8_000), ("▁KITCHEN", 9_500), ("▁LIGHTS", 11_000)]


def test_route_a_commits_a_complete_command_after_the_normal_pause():
    u = Utterance(spec(vocabulary=("kitchen lights",)))
    decisions = drive(u, speech_between((4_608, 12_800)), KITCHEN, 40_000)
    pending = [d for d in decisions if isinstance(d, Pending)]
    commit = decisions[-1]
    assert [p.route for p in pending] == ["A"]
    assert isinstance(commit, Commit)
    assert commit.route == "A" and commit.text == "turn on the kitchen lights"
    assert commit.boundary == 12_800                     # end of the last command cell
    assert commit.end == 12_800 + 3_072                  # fixed 192 ms tail
    assert commit.start == 0 and not commit.redecode_required
    assert pending[0].since - commit.boundary >= 9_728    # 608 ms pause for a complete parse
    assert u.trace()["endpoint"][-1]["event"] == "commit"
    # What the Activity page reports: the raw streaming transcript, the grammar
    # class that picked the pause, and the silence actually waited.
    assert u.heard == "TURN ON THE KITCHEN LIGHTS"
    assert commit.completeness == "complete"
    assert commit.decided_at - commit.boundary >= 9_728


def test_unknown_text_waits_for_the_longer_pause():
    words = [("▁WHAT", 6_000), ("▁TIME", 7_500), ("▁IS", 9_000), ("▁IT", 10_500)]
    u = Utterance(spec())
    decisions = drive(u, speech_between((4_608, 12_800)), words, 60_000)
    pending = next(d for d in decisions if isinstance(d, Pending))
    assert pending.since - 12_800 >= 28_672               # 1,792 ms for needs_more/unknown
    assert isinstance(decisions[-1], Commit) and decisions[-1].route == "A"
    assert decisions[-1].completeness == "unknown"
    assert decisions[-1].decided_at - 12_800 >= 28_672


def test_resumed_speech_revokes_a_pending_end():
    words = [("▁WHAT", 6_000), ("▁TIME", 7_500), ("▁IS", 9_000), ("▁IT", 10_500),
             ("▁IN", 43_000), ("▁PARIS", 45_000)]
    u = Utterance(spec())
    # Pending after the 1,792 ms pause; four command cells then arrive inside the 192 ms lookahead.
    decisions = drive(u, speech_between((4_608, 12_800), (42_496, 47_104)), words, 120_000)
    kinds = [type(d) for d in decisions]
    assert kinds.index(Pending) < kinds.index(Revoked)
    revoked = decisions[kinds.index(Revoked)]
    assert revoked.boundary == 12_800
    commit = decisions[-1]
    assert isinstance(commit, Commit) and commit.boundary == 47_104


def test_silence_after_the_trigger_closes_as_no_input():
    u = Utterance(spec())
    decisions = drive(u, speech_between(), [], 200_000)
    assert decisions == [Close("no_input")]
    # Decided at the first 80 ms block end at least 5 s of sample time after the trigger.
    assert u.trace()["endpoint"][-1]["at"] == 85_760


def test_an_endless_changing_request_closes_as_too_long():
    words = [(f"▁W{i}", 6_000 + i * 3_000) for i in range(200)]
    u = Utterance(spec())
    decisions = drive(u, speech_between((4_608, 10**7)), words, 400_000)
    assert decisions[-1] == Close("too_long")
    assert u.frontier - u.spec.trigger >= 15 * 16_000


def test_extended_utterances_allow_thirty_seconds():
    words = [(f"▁W{i}", 6_000 + i * 3_000) for i in range(400)]
    u = Utterance(spec(extended=True))
    decisions = drive(u, speech_between((4_608, 10**7)), words, 700_000)
    assert decisions[-1] == Close("too_long")
    assert u.frontier - u.spec.trigger >= 30 * 16_000


def test_speech_without_progress_or_a_complete_prefix_closes_as_retry():
    # The talker keeps going but the recognizer's text never moves: no stable progress.
    u = Utterance(spec())
    decisions = drive(u, speech_between((4_608, 10**7)), [("▁BLAH", 6_000)], 200_000)
    assert decisions[-1] == Close("retry")


def test_a_stable_complete_prefix_under_continuing_speech_falls_back_to_commit():
    u = Utterance(spec(vocabulary=("kitchen lights",)))
    decisions = drive(u, speech_between((4_608, 10**7)), KITCHEN, 200_000)
    commit = decisions[-1]
    assert isinstance(commit, Commit)
    assert commit.route == "fallback" and commit.redecode_required
    assert commit.text == "turn on the kitchen lights"


def test_a_gap_closes_the_utterance_as_interrupted():
    u = Utterance(spec())
    decisions = drive(u, speech_between((4_608, 12_800)), KITCHEN, 60_000, gap_at=8_192)
    assert decisions[-1] == Close("interrupted")


def test_route_b_commits_a_complete_command_over_quieter_background_speech():
    u = Utterance(spec(vocabulary=("kitchen lights",)))
    # Command at -30 dB, then continuous TV speech 15 dB lower that adds no words.
    def cell(start):
        if 4_608 <= start < 12_800:
            return SPEECH_DB, 0.95
        if start >= 12_800:
            return SPEECH_DB - 15, 0.9
        return QUIET_DB, 0.05
    decisions = drive(u, cell, KITCHEN, 200_000)
    commit = decisions[-1]
    assert isinstance(commit, Commit)
    assert commit.route == "B" and commit.redecode_required
    assert commit.boundary <= 12_800


def test_route_r_ends_an_esphome_reply_at_a_1024_ms_pause_whatever_the_text():
    words = [("▁BLUE", 6_000), ("▁ONE", 8_000)]
    u = Utterance(spec(kind="ha_reply"))
    decisions = drive(u, speech_between((4_608, 9_728)), words, 60_000)
    pending = next(d for d in decisions if isinstance(d, Pending))
    assert pending.route == "R"
    assert 16_384 <= pending.since - 9_728 < 16_384 + BLOCK
    assert isinstance(decisions[-1], Commit) and decisions[-1].route == "R"


# --- wake turns: wake-phrase cut and §16.2 local commands ---------------------------------

WAKE = dict(start=10_000 - 4_800, trigger=20_000, seed_start=10_000, wake_open=20_000, wake_phrase="ophelia")
WAKE_SPEECH = (9_728, 19_968)


def wake_commit(tokens, context, stop_span=(24_064, 28_160)):
    u = Utterance(spec("wake", context=context, **WAKE))
    decisions = drive(u, speech_between(WAKE_SPEECH, stop_span), tokens, 120_000)
    return u, decisions[-1]


def test_wake_stop_with_an_alert_context_is_a_local_dismiss():
    u, commit = wake_commit([("▁OPHELIA", 19_000), ("▁STOP", 26_000)],
                            CommandContext("alert", "alarm", "Wake up"))
    assert isinstance(commit, Commit)
    assert commit.text == "stop"                          # the wake word is never command text
    assert commit.local_action == "dismiss"
    assert commit.wake_window is not None and commit.wake_window.start == 0


def test_wake_snooze_on_a_ringing_timer_is_not_local():
    _, commit = wake_commit([("▁OPHELIA", 19_000), ("▁SNOOZE", 26_000)],
                            CommandContext("alert", "timer", "pasta"))
    assert isinstance(commit, Commit) and commit.local_action is None


def test_stop_without_a_command_context_goes_to_home_assistant():
    _, commit = wake_commit([("▁OPHELIA", 19_000), ("▁STOP", 26_000)], None)
    assert isinstance(commit, Commit) and commit.local_action is None


def test_negation_and_extra_words_are_never_local():
    _, commit = wake_commit([("▁OPHELIA", 19_000), ("▁DON'T", 25_000), ("▁STOP", 26_500)],
                            CommandContext("alert", "alarm", None))
    assert isinstance(commit, Commit) and commit.local_action is None


def test_the_wake_word_alone_is_not_command_speech():
    u = Utterance(spec("wake", context=CommandContext("alert", "alarm", None), **WAKE))
    decisions = drive(u, speech_between(WAKE_SPEECH), [("▁OPHELIA", 19_000)], 200_000)
    assert decisions[-1] == Close("no_input")


def test_reply_choices_make_a_bare_am_complete():
    u = Utterance(spec("reply", choices=AMPM_CHOICES))
    decisions = drive(u, speech_between((4_608, 8_704)), [("▁P", 6_000), ("▁M", 7_000)], 60_000)
    pending = next(d for d in decisions if isinstance(d, Pending))
    assert pending.since - 8_704 < 28_672                  # complete, not the unknown pause
    assert isinstance(decisions[-1], Commit) and decisions[-1].text == "p m"


# --- lease evidence assembly -------------------------------------------------------------------


def test_cells_release_in_order_once_vad_and_echo_are_known():
    a = CellAssembler()
    a.add_cells(0, [-60.0] * 4, [0] * 4)
    assert a.release() == []                              # no mic yet: VAD coverage unknown
    a.set_vad_from(1_024)
    released = a.release()
    assert [ev.start for ev in released] == [0, 512]      # before the lease: no VAD, still counted
    assert all(ev.cell.vad is None for ev in released)
    a.require_echo_from(1_024)
    a.add_vad(1_024, [0.1, 0.9])
    assert a.release() == []                              # waiting for echo labels
    assert [ev.start for ev in a.echo_pending()] == [1_024, 1_536]
    a.add_echo(1_024, [EchoResult.NO_REFERENCE, EchoResult.ECHO_ONLY])
    released = a.release()
    assert [(ev.cell.vad, ev.cell.echo) for ev in released] == [(0.1, EchoResult.NO_REFERENCE), (0.9, EchoResult.ECHO_ONLY)]


def test_a_transport_hole_and_device_gap_flags_become_gap_cells():
    a = CellAssembler()
    a.set_vad_from(0)
    a.add_cells(0, [-60.0] * 3, [0, CELL_FLAG_GAP, 0])
    a.mark_gap(1_100, 1_200)
    a.add_vad(0, [0.1])
    released = a.release()
    assert [(ev.start, ev.cell.gap) for ev in released] == [(0, False), (512, True), (1_024, True)]


def test_background_needs_one_second_of_quiet_cells():
    a = CellAssembler()
    a.set_vad_from(10**6)
    cells = 40
    a.add_cells(0, [-60.0] * cells, [0] * cells)
    released = a.release()
    assert released[0].background is None
    first = next(i for i, ev in enumerate(released) if ev.background is not None)
    assert first * CELL >= 16_000
    assert released[-1].background == -60.0
    assert len(build_cells([-6000], [0], [0])) == 4       # wire record size used by the actor


# --- reply expectations -------------------------------------------------------------------------


def test_an_early_answer_needs_fifteen_qualifying_near_end_cells():
    watch = ReplyWatch()
    onset = None
    for i in range(20):
        start = 100 * CELL + i * CELL
        echo = EchoResult.ECHO_ONLY if i < 3 else EchoResult.NEAR_END_PRESENT
        onset = watch.push(Evidence(Cell(start, -40.0, 0.9, echo), ROOM_DB)) or onset
    assert onset == 103 * CELL and watch.early
    assert watch.push(Evidence(Cell(200 * CELL, -40.0, 0.9, EchoResult.NEAR_END_PRESENT), ROOM_DB)) is None


def test_prompt_echo_is_not_an_early_answer():
    watch = ReplyWatch()
    for i in range(40):
        assert watch.push(Evidence(Cell(i * CELL, -40.0, 0.95, EchoResult.ECHO_ONLY), ROOM_DB)) is None


def test_a_short_answer_at_the_end_of_the_prompt_is_found_after_drain():
    drain = 200 * CELL
    retained = []
    for i in range(170, 200):
        speech = i >= 195                                   # starts 160 ms before the drain
        retained.append(Evidence(Cell(i * CELL, -40.0 if speech else -70.0,
                                      0.95 if speech else 0.05, EchoResult.NO_REFERENCE), ROOM_DB))
    watch = ReplyWatch()
    assert watch.drained(drain, retained) is None           # only 5 speech cells so far
    onset = None
    for i in range(200, 204):
        onset = watch.push(Evidence(Cell(i * CELL, -40.0, 0.95, EchoResult.NO_REFERENCE), ROOM_DB)) or onset
    assert onset == 195 * CELL and not watch.early


def test_speech_already_running_long_before_drain_is_not_an_onset():
    drain = 200 * CELL
    retained = [Evidence(Cell(i * CELL, -40.0, 0.95, EchoResult.NO_REFERENCE), ROOM_DB) for i in range(160, 200)]
    watch = ReplyWatch()
    assert watch.drained(drain, retained) is None
