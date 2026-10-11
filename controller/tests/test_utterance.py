"""Per-utterance composition (em_utterance): routes, closes, local commands, reply windows.

Evidence is synthetic: one level/VAD value per 32 ms cell and streaming-ASR
tokens with emission samples, fed in sample order exactly as the actor does.
"""

from __future__ import annotations

from echomuse_grammar import AMPM_CHOICES, CommandContext, GrammarClass
from em_pause_asr import PauseDecode, WyomingServer
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
    TextSource,
    Utterance,
    UtteranceSpec,
)

SPEECH_DB, QUIET_DB, ROOM_DB = -30.0, -70.0, -70.0


def spec(kind="button", start=0, trigger=4800, **kw) -> UtteranceSpec:
    return UtteranceSpec("u1", kind, start, trigger, **kw)


def drive(u: Utterance, cell, tokens, until: int, *, gap_at: int | None = None, after_block=None, pause=None):
    """Feed cells (`cell(start) -> (level, vad)`) and ASR results block by block;
    `after_block(block_end)` runs after each evaluation, where the actor would.
    `pause` = (finalize, arrive, text): the block through `finalize` is Kroko's
    finalized result, decoded in 90 ms, and from the block through `arrive` on the
    pause server's transcript stands, answered in 240 ms, as the worker reports them."""
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
        pause_text = pause[2] if pause is not None and block_end >= pause[1] else None
        arrived = pause_text is not None and block_end - BLOCK < pause[1]
        finalized = pause is not None and block_end - BLOCK < pause[0] <= block_end
        u.push_asr([t for t, _ in emitted], [s for _, s in emitted], blanks, block_end,
                   finalize_ms=90 if finalized else None, pause_text=pause_text,
                   pause_decode=PauseDecode(pause[0], pause_text, 240, None) if arrived else None)
        decisions += u.advance(block_end)
        if after_block is not None:
            after_block(block_end)
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
    assert commit.source == TextSource.STREAMING and u.trace()["pauses"] == []   # no pause decode
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


WHAT_TIME = [("▁WHAT", 6_000), ("▁TIME", 7_500), ("▁IS", 9_000), ("▁IT", 10_500)]


def ha_answers(u: Utterance, classes: dict[str, GrammarClass], *, from_sample: int = 0):
    """The actor's side: ask about each new prefix, answer once `from_sample` is evaluated."""
    asked: list[str] = []
    waiting: list[str] = []

    def after_block(block_end: int) -> None:
        text = u.recognizer_query()
        if text is not None:
            asked.append(text)
            waiting.append(text)
        if block_end >= from_sample:
            for text in waiting:
                u.recognized(text, classes.get(text, GrammarClass.UNKNOWN), ms=30)
            waiting.clear()
    return asked, after_block


def test_ha_complete_answer_ends_unknown_text_after_the_short_pause():
    u = Utterance(spec())
    asked, after_block = ha_answers(u, {"what time is it": GrammarClass.COMPLETE})
    decisions = drive(u, speech_between((4_608, 12_800)), WHAT_TIME, 60_000, after_block=after_block)
    assert asked[-1] == "what time is it" and len(asked) == len(set(asked))   # each new prefix, once
    pending = next(d for d in decisions if isinstance(d, Pending))
    assert 9_728 <= pending.since - 12_800 < 28_672      # 608 ms, not the 1,792 ms of `unknown`
    commit = decisions[-1]
    assert isinstance(commit, Commit) and commit.completeness == "complete"
    assert [(r["text"], r["klass"]) for r in u.trace()["recognizer"]][-1] == ("what time is it", "complete")


def test_ha_answer_counts_only_from_the_block_after_it_arrived():
    u = Utterance(spec())
    late = 12_800 + 16_000                                # 1,000 ms into the pause, past the 608 ms
    _, after_block = ha_answers(u, {"what time is it": GrammarClass.COMPLETE}, from_sample=late)
    decisions = drive(u, speech_between((4_608, 12_800)), WHAT_TIME, 60_000, after_block=after_block)
    at = u.trace()["recognizer"][-1]["at"]
    assert at - 12_800 > 9_728                            # it arrived after the 608 ms had passed
    pending = next(d for d in decisions if isinstance(d, Pending))
    assert pending.since == at + BLOCK                    # never retroactively


def test_local_grammar_class_is_never_replaced_by_ha():
    u = Utterance(spec(vocabulary=("kitchen", "kitchen lights")))
    words = [("▁TURN", 6_000), ("▁ON", 7_000), ("▁THE", 8_000), ("▁KITCHEN", 9_500)]
    asked, after_block = ha_answers(u, {})

    def answer_complete_anyway(block_end: int) -> None:
        after_block(block_end)
        u.recognized("turn on the kitchen", GrammarClass.COMPLETE, ms=30)
    decisions = drive(u, speech_between((4_608, 11_000)), words, 60_000, after_block=answer_complete_anyway)
    assert "turn on the kitchen" not in asked             # the local grammar knows it: HA is not asked
    commit = decisions[-1]
    assert isinstance(commit, Commit) and commit.completeness == "extendable"
    assert commit.decided_at - commit.boundary >= 19_456  # the 1,216 ms pause still applies


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


# --- finalize at the pause (§16.6) --------------------------------------------------------

CHUNK_FIRST, CHUNK = 23_040, 20_480   # Kroko: text at 1.44 s of stream audio, then every 1.28 s


def drive_chunked(u: Utterance, cell, tokens, until: int, *, speech_end: int, finalize_at: int | None):
    """Feed ASR the way the worker does: the live stream only knows tokens its
    last 1.28 s chunk covered and counts blanks to that chunk edge; from
    `finalize_at` a finalized result carries every token and counts blanks in
    real audio since `speech_end` while the live tokens are a prefix of it."""
    decisions = []
    next_cell = 0
    for block_end in range(BLOCK, until + BLOCK, BLOCK):
        while next_cell + CELL <= block_end:
            level, vad = cell(next_cell)
            u.push_cell(Evidence(Cell(next_cell, level, vad, None), ROOM_DB))
            next_cell += CELL
        coverage = 0 if block_end < CHUNK_FIRST else (1 + (block_end - CHUNK_FIRST) // CHUNK) * CHUNK
        live = [(t, s) for t, s in tokens if s < coverage]
        if finalize_at is not None and block_end >= finalize_at and len(live) < len(tokens):
            emitted, blanks = tokens, (block_end - speech_end) // 640
        else:
            emitted = live
            blanks = (coverage - (live[-1][1] if live else u.spec.start)) // 640
        u.push_asr([t for t, _ in emitted], [s for _, s in emitted], blanks, block_end,
                   finalize_ms=150 if block_end == finalize_at else None)
        decisions += u.advance(block_end)
        if u.done:
            break
    return decisions


# "lights" ends just past the first chunk edge; Kroko emits it late, inside the finalize flush.
LATE_END = 22_528                       # last speech cell
KITCHEN_LATE = [("▁TURN", 6_000), ("▁ON", 9_000), ("▁THE", 12_000), ("▁KITCHEN", 16_000), ("▁LIGHTS", 29_500)]


def test_a_finalized_complete_command_goes_pending_at_the_608_ms_pause():
    finalize_at = LATE_END + 5_120 + (-(LATE_END + 5_120)) % BLOCK   # first block 320 ms into the pause
    u = Utterance(spec(vocabulary=("kitchen lights",)))
    decisions = drive_chunked(u, speech_between((4_608, LATE_END)), KITCHEN_LATE, 80_000,
                              speech_end=LATE_END, finalize_at=finalize_at)
    pending = next(d for d in decisions if isinstance(d, Pending))
    assert pending.route == "A" and pending.boundary == LATE_END
    assert 9_728 <= pending.since - LATE_END < 9_728 + BLOCK         # the complete pause, not a chunk edge
    commit = decisions[-1]
    assert isinstance(commit, Commit) and commit.text == "turn on the kitchen lights"
    assert [p["through"] for p in u.trace()["pauses"]] == [finalize_at]


def test_without_finalize_the_last_word_waits_for_the_next_chunk_edge():
    u = Utterance(spec(vocabulary=("kitchen lights",)))
    decisions = drive_chunked(u, speech_between((4_608, LATE_END)), KITCHEN_LATE, 80_000,
                              speech_end=LATE_END, finalize_at=None)
    pending = next(d for d in decisions if isinstance(d, Pending))
    assert pending.since >= CHUNK_FIRST + CHUNK                      # "lights" arrives with the second chunk
    assert u.trace()["pauses"] == []


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


def test_the_pause_servers_words_stand_in_for_misheard_streaming_text():
    """Kroko hears "Ophelia, stop" as "full filly ate stopped"; the pause server's words,
    arriving four blocks after Kroko's, are judged instead, cut at the wake phrase, so
    the stop is still local."""
    u = Utterance(spec("wake", context=CommandContext("alert", "alarm", "Wake up"), **WAKE))
    misheard = [("▁FULL", 19_000), ("▁FILLY", 19_400), ("▁ATE", 19_800), ("▁STOPPED", 26_000)]
    decisions = drive(u, speech_between(WAKE_SPEECH, (24_064, 28_160)), misheard, 120_000,
                      pause=(28_160 + 5_120, 28_160 + 10_240, "Ophelia, stop."))
    commit = decisions[-1]
    assert isinstance(commit, Commit)
    assert (commit.text, commit.source, commit.local_action) == ("stop", TextSource.SERVER, "dismiss")
    trace = u.trace()
    assert trace["heard"] == "Ophelia, stop."
    [pause] = trace["pauses"]
    assert pause["through"] == 33_280
    assert (pause["kroko"]["text"], pause["kroko"]["ms"], pause["kroko"]["at"]) == ("FULL FILLY ATE STOPPED", 90, 33_280)
    assert pause["server"] == {"text": "Ophelia, stop.", "ms": 240, "judged": "stop", "at": 38_400, "error": None}
    assert trace["endpoint"][-1]["source"] == "server"


PAUSE_SERVER = WyomingServer("stt", 10300, "", "en")


def test_kroko_wins_when_the_turn_is_decided_before_the_server_answers():
    """A complete command commits on Kroko's pause words 608 ms into the pause; the
    server's request is still out, which the trace shows as no answer and no error."""
    u = Utterance(spec(vocabulary=("kitchen lights",), pause_server=PAUSE_SERVER))
    decisions = drive(u, speech_between((4_608, 12_800)), KITCHEN, 40_000,
                      pause=(17_920, 60_000, "Turn on the kitchen lights."))
    commit = decisions[-1]
    assert isinstance(commit, Commit) and commit.source == TextSource.KROKO
    assert u.trace()["pauses"] == [{
        "through": 17_920,
        "kroko": {"text": "TURN ON THE KITCHEN LIGHTS", "ms": 90, "judged": "turn on the kitchen lights",
                  "at": 17_920, "error": None},
        "server": {"text": None, "ms": None, "judged": None, "at": None, "error": None},
    }]


def test_server_words_already_in_at_krokos_decode_are_judged_instead():
    """The server answered before Kroko's decode was reported: its words ride that result,
    so Kroko's are never judged."""
    u = Utterance(spec(vocabulary=("kitchen lights",), pause_server=PAUSE_SERVER))
    decisions = drive(u, speech_between((4_608, 12_800)), KITCHEN, 40_000,
                      pause=(17_920, 17_920, "Turn on the kitchen lights."))
    assert isinstance(decisions[-1], Commit) and decisions[-1].source == TextSource.SERVER
    [pause] = u.trace()["pauses"]
    assert (pause["kroko"]["judged"], pause["kroko"]["at"]) == (None, None)
    assert (pause["server"]["judged"], pause["server"]["at"]) == ("turn on the kitchen lights", 17_920)


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
    assert onset == 103 * CELL
    assert watch.push(Evidence(Cell(200 * CELL, -40.0, 0.9, EchoResult.NEAR_END_PRESENT), ROOM_DB)) is None


def test_prompt_echo_is_not_an_early_answer():
    watch = ReplyWatch()
    for i in range(40):
        assert watch.push(Evidence(Cell(i * CELL, -40.0, 0.95, EchoResult.ECHO_ONLY), ROOM_DB)) is None


# A reply turn opened at the drain: trigger T there, utterance from T − 780 ms.
DRAIN = 24_000
REPLY_START = DRAIN - 12_480


def reply_spec(**kw) -> UtteranceSpec:
    return spec("reply", start=REPLY_START, trigger=DRAIN, **kw)


def test_turn_617s_answer_after_the_chime_commits_without_an_onset_gate():
    # Turn 617's cell VAD after the reply chime (test_attribution.TURN_617_*): the reply's run
    # opens on the answer's second cell, and the normal reducer ends it after the long pause.
    answer = [0.53, 0.80, 0.86, 0.90, 0.91, 0.90, 0.88, 0.86, 0.72, 0.73, 0.81, 0.77, 0.86, 0.98, 0.72,
              0.37, 0.43, 0.51, 0.51, 0.99, 0.99, 0.99, 0.97, 0.84, 0.98, 0.99, 0.99, 0.99, 0.99, 0.99,
              0.99, 0.93, 0.98]
    a0 = 59 * CELL                                          # 12 cells after the drain

    def cell(start):
        i = (start - a0) // CELL
        return (SPEECH_DB, answer[i]) if 0 <= i < len(answer) else (QUIET_DB, 0.04)

    words = [("▁I", 32_000), ("▁DEMAND", 34_000), ("▁THAT", 37_000), ("▁YOU", 39_000), ("▁ASK", 41_000),
             ("▁ME", 43_000), ("▁A", 44_000), ("▁QUESTION", 46_500)]
    u = Utterance(reply_spec())
    decisions = drive(u, cell, words, 120_000)
    commit = decisions[-1]
    assert isinstance(commit, Commit) and commit.text == "i demand that you ask me a question"
    assert commit.boundary == a0 + len(answer) * CELL
    # HA hears the answer from 300 ms before its run, not the chime and the wait from the utterance start.
    assert commit.start == a0 + CELL - 4_800 > u.spec.start


def test_an_answer_begun_300_ms_before_the_drain_is_taken_from_its_start():
    words = [("▁THE", 21_000), ("▁LIVING", 24_500), ("▁ROOM", 28_000)]
    u = Utterance(reply_spec())
    decisions = drive(u, speech_between((19_456, 30_208)), words, 120_000)
    commit = decisions[-1]
    assert isinstance(commit, Commit) and commit.text == "the living room"
    assert commit.start == 19_456 - 4_800 and commit.boundary == 30_208


def test_speech_under_way_at_the_drain_is_not_the_answer_but_a_new_run_after_it_is():
    # Talking since before the utterance start runs on 1 s past the drain, pauses 288 ms, then answers.
    words = [("▁THE", 47_000), ("▁LIVING", 50_000), ("▁ROOM", 54_000)]
    u = Utterance(reply_spec())
    decisions = drive(u, speech_between((11_264, 40_960), (45_568, 56_320)), words, 140_000)
    commit = decisions[-1]
    assert isinstance(commit, Commit) and commit.text == "the living room"
    assert commit.start == 45_568 - 4_800 and commit.boundary == 56_320
    segments = u.trace()["segments"]
    assert ("background_speech", "under_way", 23_552, 40_960) in segments
    assert not any(cls == "command_speech" and b > DRAIN and a < 40_960 for cls, _, a, b in segments)


def test_a_silent_reply_window_closes_no_input_seven_seconds_after_it_opened():
    u = Utterance(reply_spec())
    assert drive(u, speech_between(), [], 300_000) == [Close("no_input")]
    assert u.trace()["endpoint"][-1]["at"] == 136_960      # first block end ≥ the drain + 7 s


def test_a_reply_counts_too_long_from_its_first_command_speech():
    words = [(f"▁W{i}", 105_000 + i * 3_000) for i in range(150)]
    u = Utterance(reply_spec())
    decisions = drive(u, speech_between((104_448, 10**7)), words, 600_000)
    assert decisions[-1] == Close("too_long")
    assert u.trace()["endpoint"][-1]["at"] == 345_600      # first block end ≥ 104,448 + 15 s


def test_a_gap_before_any_answer_stops_a_reply_for_reopening_instead_of_ending_it():
    u = Utterance(reply_spec())
    gap = 40_000                                            # inside cell 78: [39,936, 40,448)
    decisions = drive(u, speech_between(), [], 60_000, gap_at=gap)
    assert decisions == [] and not u.done                    # no `interrupted`
    assert u.reopen_at == 40_448                            # after the gap cell, once audio followed it

    # Reopened there with the window's own end, it still closes no_input at the drain + 7 s.
    again = Utterance(UtteranceSpec("u2", "reply", 40_448, 40_448, no_input_at=u.no_input_at))
    assert drive(again, speech_between(), [], 300_000) == [Close("no_input")]
    assert again.trace()["endpoint"][-1]["at"] == 136_960


def test_a_gap_after_the_answer_began_ends_a_reply_as_for_any_turn():
    u = Utterance(reply_spec())
    decisions = drive(u, speech_between((25_088, 40_960)), [("▁THE", 27_000)], 60_000, gap_at=40_000)
    assert decisions[-1] == Close("interrupted") and u.reopen_at is None
