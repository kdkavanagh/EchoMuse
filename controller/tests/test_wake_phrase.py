import pytest

from em_wake_phrase import (
    StreamingTranscript,
    command_text,
    final_command_text,
    locate_streaming,
    verify_wake,
    words,
    words_after,
)


def stream(text_tokens, samples=None):
    samples = samples or [i * 640 for i in range(len(text_tokens))]
    return StreamingTranscript.from_tokens(text_tokens, samples)


def located(tokens, wake="ophelia", deadline=10_000):
    transcript = stream(tokens)
    return transcript, locate_streaming(transcript, wake, deadline)


def strip(final, tokens, *, wake_initiated=True):
    """Steps 2–3: locate the wake phrase in the streaming `tokens`, then strip `final`."""
    transcript, found = located(tokens)
    return final_command_text(
        final,
        wake_initiated=wake_initiated,
        streaming_window=found,
        streaming_tail=words_after(transcript, found),
        wake_phrase="ophelia",
    )


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


def test_words_after_counts_the_streaming_tail():
    transcript, found = located(["Sure.", " Pophilia.", " What", " time", " is", " it?"])
    assert found is not None and found.start == 1
    assert words_after(transcript, found) == 4
    assert words_after(transcript, None) == 6


def test_preamble_and_first_wake_are_removed_verbatim():
    tokens = ["Damn", " it.", " Oh", " hell", " yeah,", " Ophelia.", " Turn", " off", " the", " lights."]
    transcript, found = located(tokens)
    assert found is not None and found.start == 5
    assert strip("Damn it. Oh hell yeah, Ophelia. Turn off the lights.", tokens) == "Turn off the lights."
    cmd = command_text(transcript, found)
    assert cmd.text == "turn off the lights"
    assert cmd.first_token_sample == 6 * 640


@pytest.mark.parametrize(
    ("final", "tokens", "expected"),
    [
        ("Ophelia, what does Ophelia mean?", ["Ophelia,", " what", " does", " Ophelia", " mean?"],
         "what does Ophelia mean?"),
        ("Yeah. Ophelia. What was the score of the White Sox game?",
         ["Sure.", " Pophilia.", " What", " was", " the", " score", " of", " the", " White", " socks", " game?"],
         "What was the score of the White Sox game?"),
    ],
)
def test_wake_near_the_streaming_match_is_removed(final, tokens, expected):
    assert strip(final, tokens) == expected


def test_speech_before_the_wake_word_dropped_by_streaming_is_removed():
    # Turn 552: the STT kept "I gotta go faster." that Kroko dropped, so the final
    # match sits at index 4 > s + 1; the four words after it match Kroko's four.
    final = "I gotta go faster. Ophelia What time is it?"
    assert strip(final, ["Ophilia.", " What", " time", " is", " it?"]) == "What time is it?"


@pytest.mark.parametrize(
    ("final", "tokens"),
    [
        # STT dropped the leading wake word: one word follows the mention, Kroko heard four.
        ("what does Ophelia mean?", ["Ophelia,", " what", " does", " Ophelia", " mean?"]),
        ("what does Ophelia mean", ["Ophelia", " what", " does", " Ophelia", " mean"]),
        # "opera" matches the wake phrase, but one word follows it, not four.
        ("play some opera music", ["Ophelia", " play", " some", " opera", " music"]),
    ],
)
def test_later_mention_is_never_removed(final, tokens):
    assert strip(final, tokens) == final


def test_dropped_wake_word_leaves_plain_command_unchanged():
    assert strip("Turn off the lights.", ["Ophelia", " turn", " off", " the", " lights"]) == "Turn off the lights."


@pytest.mark.parametrize(
    ("final", "expected"),
    [
        ("Ofelia, turn off the lights.", "turn off the lights."),
        ("Sophia, turn off the lights.", "turn off the lights."),
    ],
)
def test_final_fuzzy_spellings_strip(final, expected):
    assert strip(final, ["Ophelia", " turn", " off", " the", " lights"]) == expected


def test_button_and_reply_turns_never_strip():
    assert strip("Ophelia, stop.", ["Ophelia", " stop"], wake_initiated=False) == "Ophelia, stop."


@pytest.mark.parametrize(
    ("final", "tokens", "expected"),
    [
        ("Ophelia, what's the weather?", ["What's", " the", " weather?"], "what's the weather?"),
        ("Okay Ophelia, what's the weather?", ["What's", " the", " weather?"], "what's the weather?"),
        # A longer preamble: stripped when the words after the match agree with Kroko's.
        ("Damn it, Ophelia, turn off the lights.", ["Turn", " off", " the", " lights."], "turn off the lights."),
        ("Damn it, Ophelia, turn off the lights.",
         ["Damn", " it,", " oh,", " feel", " ya,", " turn", " off", " the", " lights."],
         "Damn it, Ophelia, turn off the lights."),
    ],
)
def test_missing_streaming_match(final, tokens, expected):
    # Kroko dropped the quiet wake word (turn 433): no streaming window, so s = 0.
    assert locate_streaming(stream(tokens), "ophelia", 10_000) is None
    assert strip(final, tokens) == expected


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
