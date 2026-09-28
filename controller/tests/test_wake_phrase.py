import pytest

from em_wake_phrase import (
    StreamingTranscript,
    command_text,
    final_command_text,
    locate_streaming,
    verify_wake,
    words,
)


def stream(text_tokens, samples=None):
    samples = samples or [i * 640 for i in range(len(text_tokens))]
    return StreamingTranscript.from_tokens(text_tokens, samples)


def located(tokens, wake="ophelia", deadline=10_000):
    transcript = stream(tokens)
    return transcript, locate_streaming(transcript, wake, deadline)


def test_words_keep_original_character_spans():
    text = "  Don't—call OPHELIA_2!"
    got = words(text)
    assert [(w.text, text[w.start : w.end]) for w in got] == [
        ("don't", "Don't"),
        ("call", "call"),
        ("ophelia", "OPHELIA"),
        ("2", "2"),
    ]


def test_streaming_location_uses_deadline_lowest_distance_then_earliest():
    transcript = stream(["Sophia", " Ophelia", " stop"], [100, 9_000, 9_500])
    # The exact second mention misses the lookahead, so the earlier fuzzy one wins.
    found = locate_streaming(transcript, "ophelia", 8_000)
    assert found is not None and found.start == 0
    # Both are now eligible; exact distance wins.
    found = locate_streaming(transcript, "ophelia", 10_000)
    assert found is not None and found.start == 1


def test_preamble_and_first_wake_are_removed_verbatim():
    transcript, found = located(
        ["Damn", " it.", " Oh", " hell", " yeah,", " Ophelia.", " Turn", " off", " the", " lights."]
    )
    assert found is not None and found.start == 5
    final = "Damn it. Oh hell yeah, Ophelia. Turn off the lights."
    assert final_command_text(final, wake_initiated=True, streaming_window=found, wake_phrase="ophelia") == (
        "Turn off the lights."
    )
    cmd = command_text(transcript, found)
    assert cmd.text == "turn off the lights"
    assert cmd.first_token_sample == 6 * 640


def test_later_mention_is_never_removed():
    transcript, found = located(["Ophelia,", " what", " does", " Ophelia", " mean?"])
    assert found is not None and found.start == 0
    assert final_command_text(
        "Ophelia, what does Ophelia mean?",
        wake_initiated=True,
        streaming_window=found,
        wake_phrase="ophelia",
    ) == "what does Ophelia mean?"


def test_dropped_leading_wake_word_does_not_strip_later_mention():
    _, found = located(["Ophelia,", " what", " does", " Ophelia", " mean?"])
    assert found is not None and found.start == 0
    final = "what does Ophelia mean?"
    assert final_command_text(final, wake_initiated=True, streaming_window=found, wake_phrase="ophelia") == final


def test_dropped_wake_word_leaves_plain_command_unchanged():
    _, found = located(["Ophelia", " turn", " off", " the", " lights"])
    final = "Turn off the lights."
    assert final_command_text(final, wake_initiated=True, streaming_window=found, wake_phrase="ophelia") == final


@pytest.mark.parametrize(
    ("final", "expected"),
    [
        ("Ofelia, turn off the lights.", "turn off the lights."),
        ("Sophia, turn off the lights.", "turn off the lights."),
    ],
)
def test_final_fuzzy_spellings_strip(final, expected):
    _, found = located(["Ophelia", " turn", " off", " the", " lights"])
    assert final_command_text(final, wake_initiated=True, streaming_window=found, wake_phrase="ophelia") == expected


def test_button_and_reply_turns_never_strip():
    _, found = located(["Ophelia", " stop"])
    text = "Ophelia, stop."
    assert final_command_text(text, wake_initiated=False, streaming_window=found, wake_phrase="ophelia") == text


def test_missing_streaming_match_never_strips():
    transcript = stream(["turn", " off", " the", " lights"])
    found = locate_streaming(transcript, "ophelia", 10_000)
    assert found is None
    assert final_command_text(
        "Ophelia, turn off the lights.", wake_initiated=True, streaming_window=found, wake_phrase="ophelia"
    ) == "Ophelia, turn off the lights."


def test_oh_feel_ya_is_not_a_wake_match():
    transcript, found = located(["Oh,", " feel", " ya.", " Stop."])
    assert found is None
    assert command_text(transcript, found).text == "oh, feel ya. stop"


@pytest.mark.parametrize("alias", ["Ophili", "Pophili", "Apheli", "Ophi"])
def test_wake_verification_aliases_pass(alias):
    result = verify_wake(alias, "ophel")
    assert result.passed
    assert result.distance is not None and result.distance <= 0.40


@pytest.mark.parametrize("not_alias", ["Eight", "I need to", "fear", ""])
def test_wake_verification_self_output_examples_fail(not_alias):
    result = verify_wake(not_alias, "ophel")
    assert not result.passed
