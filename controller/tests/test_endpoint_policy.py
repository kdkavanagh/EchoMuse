import pytest

from em_attribution import (
    BACKGROUND_SPEECH,
    CELL,
    COMMAND_SPEECH,
    GAP,
    NON_SPEECH,
    SELF_OUTPUT,
    ClassifiedCell,
)
from em_endpoint_policy import (
    COMMITTED,
    END_PENDING,
    LISTENING,
    ChoiceResult,
    EndpointReducer,
    ReplyChoice,
    StableText,
    TextStability,
    local_command_preconditions,
    redecode_differs,
    resolve_reply_choice,
)

def complete(text):
    if text in {"turn off lights", "set a timer for five minutes", "stop"}:
        return "complete"
    if text == "turn off":
        return "extendable"
    if text in {"set a timer for", "turn off lights in the"}:
        return "needs_more"
    return "unknown"


def classified(i, cls, *, speech=None, command=None):
    if speech is None:
        speech = cls in (COMMAND_SPEECH, BACKGROUND_SPEECH)
    if command is None:
        command = cls == COMMAND_SPEECH
    return ClassifiedCell(i * CELL, cls, "vad", speech, command)


def stable(
    prefix="turn off lights",
    *,
    prefix_sample=3_072,
    progress_sample=3_072,
    through_sample=20_000,
    blanks=10,
    tail="",
):
    return StableText(
        prefix=prefix,
        prefix_tokens=tuple(prefix.split()),
        tail=tail,
        latest_text=prefix if not tail else f"{prefix} {tail}",
        prefix_sample=prefix_sample,
        progress_sample=progress_sample,
        trailing_blank_frames=blanks,
        through_sample=through_sample,
    )


def reducer(*, esphome=False, extended=False):
    return EndpointReducer(
        utterance_start=0,
        trigger_sample=0,
        completeness=complete,
        esphome_reply=esphome,
        extended_utterances=extended,
    )


def test_text_stability_needs_three_results_spanning_240_ms_and_ignores_punctuation():
    tracker = TextStability(trigger_sample=123)
    assert tracker.push("Turn off lights", 0, 0).prefix == ""
    assert tracker.push("Turn off lights!", 1_920, 0).prefix == ""
    got = tracker.push("turn off lights.", 3_840, 4)
    assert got.prefix == "turn off lights"
    assert got.prefix_sample == 3_840
    assert got.progress_sample == 3_840
    assert got.trailing_blank_frames == 4


def test_route_a_pending_then_commit_with_immutable_192_ms_tail():
    r = reducer()
    r.observe([classified(i, COMMAND_SPEECH) for i in range(6)])
    r.observe([classified(i, NON_SPEECH, speech=False, command=False) for i in range(6, 25)])
    frontier = 25 * CELL  # exactly 608 ms after command boundary
    first = r.step(frontier=frontier, valid_audio_end=frontier, stable=stable(through_sample=frontier))
    assert first.state == END_PENDING and first.pending.route == "A"
    second_frontier = frontier + 3_072
    second = r.step(
        frontier=second_frontier,
        valid_audio_end=second_frontier - 200,
        stable=stable(through_sample=second_frontier),
    )
    assert second.state == COMMITTED
    assert second.commit.boundary == 6 * CELL
    assert second.commit.end == 6 * CELL + 3_072
    assert second.commit.route == "A"
    assert not second.commit.redecode_required
    with pytest.raises(RuntimeError, match="already committed"):
        r.finalize_once(6 * CELL, route="A", text="x", valid_audio_end=second_frontier)


def test_pending_is_revoked_by_four_new_command_cells_before_lookahead():
    r = reducer()
    r.observe([classified(i, COMMAND_SPEECH) for i in range(6)])
    r.observe([classified(i, NON_SPEECH, speech=False, command=False) for i in range(6, 25)])
    frontier = 25 * CELL
    pending = r.step(frontier=frontier, valid_audio_end=frontier, stable=stable(through_sample=frontier)).pending
    r.observe([classified(i, COMMAND_SPEECH) for i in range(25, 29)])
    got = r.step(frontier=29 * CELL, valid_audio_end=29 * CELL, stable=stable(through_sample=29 * CELL))
    assert got.state == LISTENING
    assert got.revoked == pending
    assert got.commit is None


def test_route_b_complete_command_under_background_speech():
    r = reducer()
    r.observe([classified(i, COMMAND_SPEECH) for i in range(6)])
    r.observe([classified(i, BACKGROUND_SPEECH, command=False) for i in range(6, 25)])
    frontier = 25 * CELL
    got = r.step(frontier=frontier, valid_audio_end=frontier, stable=stable(through_sample=frontier))
    assert got.pending.route == "B"
    committed = r.step(
        frontier=frontier + 3_072,
        valid_audio_end=frontier + 3_072,
        stable=stable(through_sample=frontier + 3_072),
    ).commit
    assert committed.boundary == 6 * CELL
    assert committed.route == "B" and committed.redecode_required


@pytest.mark.parametrize(("tail", "routed"), [("in the", False), ("and the news tonight", True)])
def test_route_b_requires_tail_that_does_not_extend_the_parse(tail, routed):
    # §16.6: route B holds only when prefix + tail parses `unknown` (TV words); an extending tail blocks it.
    r = reducer()
    r.observe([classified(i, COMMAND_SPEECH) for i in range(6)])
    r.observe([classified(i, BACKGROUND_SPEECH, command=False) for i in range(6, 25)])
    got = r.step(
        frontier=25 * CELL,
        valid_audio_end=25 * CELL,
        stable=stable(tail=tail, through_sample=25 * CELL, blanks=0),
    )
    if routed:
        assert got.pending.route == "B"
    else:
        assert got.pending is None and got.state == LISTENING


def test_route_r_replaces_routes_a_and_b_for_esphome_replies():
    r = reducer(esphome=True)
    r.observe([classified(0, COMMAND_SPEECH)])
    r.observe([classified(i, NON_SPEECH, speech=False, command=False) for i in range(1, 33)])
    frontier = CELL + 16_384
    got = r.step(
        frontier=frontier,
        valid_audio_end=frontier,
        stable=stable(prefix="free form answer", through_sample=frontier, blanks=0),
    )
    assert got.pending.route == "R"


def test_three_second_no_progress_fallback_commits_complete_prefix():
    r = reducer()
    r.observe([classified(i, COMMAND_SPEECH) for i in range(6)])
    frontier = 3_072 + 48_000
    s = stable(prefix_sample=3_072, progress_sample=3_072, through_sample=frontier, blanks=0)
    got = r.step(frontier=frontier, valid_audio_end=frontier, stable=s)
    assert got.commit.route == "fallback"
    assert got.commit.boundary == 6 * CELL
    assert got.commit.redecode_required


def test_three_second_no_progress_fallback_retries_unknown_text():
    r = reducer()
    r.observe([classified(i, COMMAND_SPEECH) for i in range(6)])
    frontier = 3_072 + 48_000
    s = stable(prefix="tell me a story", prefix_sample=3_072, progress_sample=3_072, through_sample=frontier, blanks=0)
    got = r.step(frontier=frontier, valid_audio_end=frontier, stable=s)
    assert got.close.reason == "retry"


def test_no_input_uses_frontier_not_number_or_wall_time_of_calls():
    r = reducer()
    s = stable(prefix="", prefix_sample=None, progress_sample=0, through_sample=0, blanks=0)
    for _ in range(100):
        got = r.step(frontier=79_999, valid_audio_end=79_999, stable=s)
        assert got.close is None
    got = r.step(frontier=80_000, valid_audio_end=80_000, stable=s)
    assert got.close.reason == "no_input"


@pytest.mark.parametrize(("extended", "frontier"), [(False, 240_000), (True, 480_000)])
def test_too_long_bounds(extended, frontier):
    r = reducer(extended=extended)
    r.observe([classified(0, COMMAND_SPEECH)])
    got = r.step(frontier=frontier, valid_audio_end=frontier, stable=stable(through_sample=frontier))
    assert got.close.reason == "too_long"


@pytest.mark.parametrize(
    ("kwargs", "expected"),
    [
        ({"muted": True, "session_lost": True, "gap": True, "overrun": True}, "muted"),
        ({"session_lost": True, "gap": True, "overrun": True}, "session_lost"),
        ({"gap": True, "overrun": True}, "interrupted"),
        ({"overrun": True}, "audio_overrun"),
    ],
)
def test_terminal_close_priority(kwargs, expected):
    r = reducer()
    got = r.step(frontier=1_280, valid_audio_end=1_280, stable=stable(through_sample=1_280), **kwargs)
    assert got.close.reason == expected


def test_gap_cell_closes_interrupted_without_external_flag():
    r = reducer()
    r.observe([classified(0, GAP, speech=False, command=False)])
    got = r.step(frontier=CELL, valid_audio_end=CELL, stable=stable(through_sample=CELL))
    assert got.close.reason == "interrupted"


@pytest.mark.parametrize(
    ("before", "after", "differs"),
    [
        ("turn off the lights", "turn off the lights", False),
        ("turn off the lig", "turn off the lights", False),
        ("turn off lights", "turn off the lights", True),
        ("turn off the lights", "turn off the kitchen lights", True),
        ("turn off", "turn off now", True),
    ],
)
def test_redecode_parse_difference_except_final_word_completion(before, after, differs):
    assert redecode_differs(before, after) is differs


def test_local_command_preconditions_are_stable_and_not_mostly_self_output():
    s = stable(prefix="stop", prefix_sample=1_000, through_sample=5_000, tail="")
    cells = [classified(i, COMMAND_SPEECH, speech=True) for i in range(5)]
    cells += [classified(i, SELF_OUTPUT, speech=True, command=False) for i in range(5, 7)]
    assert local_command_preconditions(
        normalized_text="Stop!",
        stable=s,
        first_command_token_sample=2_000,
        commit_boundary=7 * CELL,
        cells=cells,
    )
    mostly_echo = cells + [
        classified(i, SELF_OUTPUT, speech=True, command=False) for i in range(7, 10)
    ]
    assert not local_command_preconditions(
        normalized_text="stop",
        stable=s,
        first_command_token_sample=2_000,
        commit_boundary=10 * CELL,
        cells=mostly_echo,
    )
    assert not local_command_preconditions(
        normalized_text="stop now",
        stable=s,
        first_command_token_sample=2_000,
        commit_boundary=7 * CELL,
        cells=cells,
    )


def test_reply_choices_match_exact_alias_and_reprompt_once():
    choices = [
        ReplyChoice("am", ("am", "a m", "morning")),
        ReplyChoice("pm", ("pm", "p m", "at night")),
    ]
    assert resolve_reply_choice(" Morning! ", choices, already_reprompted=False) == ChoiceResult("selected", "am")
    assert resolve_reply_choice("seven", choices, already_reprompted=False) == ChoiceResult("reprompt")
    assert resolve_reply_choice("seven", choices, already_reprompted=True) == ChoiceResult("abandon")
