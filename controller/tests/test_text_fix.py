"""Transcript correction (em_text_fix, em_text_fix_service): candidate rules and acceptance
against HA's own sentence matcher.

HA's verdicts come from tests/fixtures/text_fix.json, recorded from the live HA 2026.8.1
debugger (non-executing) with the subset of trigger templates and vocabulary the fixture
names. A sentence the fixture lacks fails the test: re-record rather than invent a verdict.
"""

from __future__ import annotations

import asyncio
import functools
import json
from pathlib import Path

import pytest

pytest.importorskip("hassil")

import em_text_fix_service
from em_ha_client import (
    SentenceRecognition,
    SlotValue,
    VocabArea,
    VocabEntity,
    VocabFloor,
    Vocabulary,
    _sentence_recognition,
)
from em_text_fix import (
    AMBIGUITY_MARGIN,
    PREFERENCE_CAP,
    Candidate,
    FixConfig,
    HistoryEntry,
    Proposal,
    Reason,
    Request,
    Rule,
    Source,
    Status,
    TextFixMode,
    build_tables,
    decide,
    fix_config,
    fix_key,
    parse_list,
    parse_mode,
    propose,
    sent_text,
    similarity,
    user_vocabulary,
)
from em_text_fix_service import NotReady, TextFixService

FIXTURE = json.loads((Path(__file__).parent / "fixtures" / "text_fix.json").read_text())
DEVICE = FIXTURE["device_id"]
HISTORY = [HistoryEntry(text, turn_id, ts) for turn_id, ts, text in FIXTURE["history"]]


def vocabulary() -> Vocabulary:
    v = FIXTURE["vocabulary"]
    return Vocabulary(
        entities=tuple(VocabEntity(e, d, tuple(names), area) for e, d, names, area in v["entities"]),
        areas=tuple(VocabArea(a, tuple(names), floor) for a, names, floor in v["areas"]),
        floors=tuple(VocabFloor(f, tuple(names)) for f, names in v["floors"]))


@functools.cache
def tables(history: bool = True):
    return build_tables(vocabulary(), FIXTURE["templates"], HISTORY if history else [], 1)


def recognized(sentence: str) -> SentenceRecognition | None:
    assert sentence in FIXTURE["recognitions"], f"no recorded verdict for {sentence!r}: re-record the fixture"
    return _sentence_recognition(FIXTURE["recognitions"][sentence])


def correct(text: str, *, wake: str | None = None, terms=(), phrases=(), history: bool = True):
    key = fix_key(text)
    t = tables(history)
    original = recognized(key)
    proposal = propose(Request(key, wake), original, t, user_vocabulary(tuple(terms), tuple(phrases)))
    decision = decide(proposal, [recognized(c.text) for c in proposal.validate], original, t)
    return decision, proposal


def rejected(proposal: Proposal, text: str) -> str:
    """Why `text` was rejected before HA saw it."""
    for r in proposal.rejected:
        if r.candidate.text == text:
            assert r.status == Status.NOT_PRESERVED
            return r.detail
    raise AssertionError(f"{text!r} was not rejected: {[r.candidate.text for r in proposal.rejected]}")


# --- eligible corrections -------------------------------------------------------------------


@pytest.mark.parametrize("heard, sent, rule, source", [
    ("What time is", "what time is it", Rule.CUTOFF, Source.TRIGGER),           # turns 557, 560, 565, …
    ("watch you", "watch youtube tv", Rule.CUTOFF, Source.TRIGGER),             # turn 673
    ("Watch Flex.", "watch plex", Rule.PHRASE, Source.TRIGGER),                 # turn 770
    ("Plate cherub rock.", "play cherub rock", Rule.PHRASE, Source.HISTORY),    # turn 532
])
def test_a_misheard_command_becomes_the_known_phrase_ha_accepts(heard, sent, rule, source):
    decision, _ = correct(heard)
    assert decision.reason == Reason.APPLIED
    chosen = decision.selected
    assert chosen is not None and chosen.status == Status.OK and chosen.verdict is not None and chosen.verdict.match
    assert (chosen.candidate.text, chosen.candidate.rule, chosen.candidate.source) == (sent, rule, source)
    if rule == Rule.CUTOFF:
        assert chosen.candidate.coverage >= 0.5 and fix_key(heard) in chosen.candidate.text
    else:
        assert chosen.candidate.raw >= 0.85


def test_what_time_is_it_and_what_time_is_it_now_are_two_triggers_the_better_covered_one_wins():
    decision, _ = correct("what time is")
    accepted = [ch.candidate for ch in decision.checked if ch.status == Status.OK]
    assert {c.text for c in accepted} == {"what time is it", "what time is it now"}
    assert decision.selected.candidate.coverage - min(c.coverage for c in accepted) >= AMBIGUITY_MARGIN


def test_an_unmatched_scene_name_becomes_the_living_room_scene_ha_resolves():
    # Turn 747: "low" heard as "glow"; HA's closest template filled the area and domain.
    decision, _ = correct("set the living room lights to glow.")
    chosen = decision.selected
    assert decision.reason == Reason.APPLIED and chosen.candidate.rule == Rule.SLOT
    assert chosen.candidate.text == "set the living room lights to Low"
    assert chosen.verdict.details["name"].value == "Low" and chosen.verdict.details["area"].value == "Living Room"
    assert chosen.verdict.targets == ("scene.living_room_lights_low",)


def test_residual_wake_words_are_dropped_only_for_a_wake_turn():
    decision, _ = correct("Ophelia, what's the weather?", wake="ophelia")    # turn 433
    assert decision.reason == Reason.APPLIED and decision.selected.candidate.rule == Rule.LEAD_IN
    assert decision.selected.candidate.text == "what's the weather"
    decision, proposal = correct("Ophelia, what's the weather?")             # a button turn has no wake residue
    assert all(c.rule != Rule.LEAD_IN for c in proposal.validate)


# --- left as heard --------------------------------------------------------------------------


@pytest.mark.parametrize("heard", ["turn on the Sony TV", "say that again", "house"])
def test_text_with_no_safe_correction_is_left_alone(heard):
    decision, _ = correct(heard)
    assert decision.selected is None and decision.reason in (Reason.NO_CANDIDATE, Reason.NO_VALID_CANDIDATE)


def test_text_ha_already_understands_is_never_changed():
    original = recognized("what time is it")
    assert original is not None and original.match
    assert propose(Request("what time is it"), original, tables(), user_vocabulary((), ())).validate == ()
    assert decide(Proposal("what time is it", (), (), ()), [], original, tables()).reason == Reason.ORIGINAL_MATCHED


def test_shower_is_ambiguous_between_shower_mode_and_shower_time():
    # Turn 660: both are complete triggers HA accepts, with the same coverage.
    decision, _ = correct("shower")
    accepted = {ch.candidate.text for ch in decision.checked if ch.status == Status.OK}
    assert {"shower mode", "shower time"} <= accepted
    assert decision.reason == Reason.AMBIGUOUS and decision.selected is None


def test_near_empty_text_proposes_nothing():
    for text in ("st", "Ophelia."):
        proposal = propose(Request(fix_key(text)), None, tables(), user_vocabulary((), ()))
        assert proposal.validate == () and proposal.rejected == ()


# --- hard negatives -------------------------------------------------------------------------


def test_a_negation_is_never_dropped():
    decision, proposal = correct("don't disarm the security alarm please")
    assert rejected(proposal, "can you disarm the security alarm please") == "negation"
    assert decision.selected is None


def test_on_is_never_swapped_for_off():
    decision, proposal = correct("turn on the kitchen ceiling light")
    assert rejected(proposal, "turn off the kitchen ceiling lights") in ("drops an action", "adds a polarity")
    assert decision.selected.candidate.text == "turn on the Kitchen Ceiling Lights"


def test_lock_is_never_swapped_for_unlock():
    decision, proposal = correct("lock the front door")
    assert rejected(proposal, "unlock the front door") == "drops an action"
    assert decision.selected is None


@pytest.mark.parametrize("heard, detail", [("set a timer for 5 minuets", "unit"),
                                           ("set a timer for 6 minuets", "number")])
def test_a_number_or_unit_is_never_changed(heard, detail):
    decision, proposal = correct(heard)
    assert rejected(proposal, "set a timer for 5 minutes") == detail
    assert decision.selected is None


def test_a_room_ha_already_resolved_is_never_replaced():
    decision, proposal = correct("turn on the living room television in the bathroom")
    assert rejected(proposal, "turn on the living room television in the bedroom") == "drops 'bathroom'"
    assert decision.selected is None
    # Even a candidate that got past the words would fail on HA's own resolution.
    original = recognized("turn on the living room television in the bathroom")
    assert original.details["area"].value == "Bathroom"
    bedroom = SentenceRecognition(True, False, "builtin", "turn on [the] {name} in [the] {area}", "HassTurnOn",
                                  {"name": SlotValue("Home Theater", "living room television"),
                                   "area": SlotValue("Master Bedroom", "bedroom")}, {}, ("media_player.apple_tv",))
    candidate = Candidate("turn on the living room television in the bedroom", Rule.PHRASE, Source.TRIGGER, 0.95)
    decision = decide(Proposal(candidate.text, (candidate,), (), ()), [bedroom], original, tables())
    assert decision.checked[0].status == Status.CHANGED_RESOLVED and decision.selected is None


def test_rmv_alarm_is_ambiguous_between_arming_and_arming_away():
    # Turn 773: "arm the alarm" heard as "RMV alarm". HA accepts both modes' triggers.
    decision, _ = correct("RMV alarm.")
    templates = {ch.verdict.template for ch in decision.checked if ch.status == Status.OK}
    assert any("away" in t for t in templates) and any("away" not in t for t in templates)
    assert decision.reason == Reason.AMBIGUOUS and decision.selected is None


def test_a_catch_all_match_validates_a_full_phrase_but_not_a_name_insertion():
    play = SentenceRecognition(True, False, "trigger", "(play|played|plays) {query}")
    for rule, status in ((Rule.PHRASE, Status.OK), (Rule.NAME, Status.CATCH_ALL), (Rule.NUMBERS, Status.CATCH_ALL)):
        candidate = Candidate("play cherub rock", rule, Source.HISTORY, 0.9)
        decision = decide(Proposal("plate cherub rock", (candidate,), (), ()), [play], None, tables())
        assert decision.checked[0].status == status


# --- preferences ----------------------------------------------------------------------------


def test_a_term_alone_cannot_repair_a_command_word_but_a_full_phrase_can():
    decision, _ = correct("Plate cherub rock.", terms=["Cherub Rock"], history=False)
    assert decision.selected is None and decision.reason == Reason.NO_CANDIDATE
    decision, _ = correct("Plate cherub rock.", phrases=["play Cherub Rock"], history=False)
    assert decision.reason == Reason.APPLIED and decision.selected.candidate.source == Source.USER_PHRASE
    assert decision.selected.candidate.text == "play cherub rock" and decision.selected.candidate.preference > 0


def test_preference_never_changes_raw_evidence_and_is_capped():
    plain, _ = correct("watch flex")
    preferred, _ = correct("watch flex", terms=["Plex", "YouTube TV"], phrases=["watch Plex"])
    a, b = plain.selected.candidate, preferred.selected.candidate
    assert a.text == b.text == "watch plex" and a.raw == b.raw
    assert a.preference == 0 and 0 < b.preference <= PREFERENCE_CAP


def test_higher_entries_weigh_more_and_repeats_add_nothing():
    user = user_vocabulary(("Plex", "Netflix"), ())
    assert user.preference("watch plex") > user.preference("watch netflix") > 0
    assert user_vocabulary(("Netflix", "Plex"), ()).preference("watch netflix") == user.preference("watch plex")
    assert user.preference("plex plex plex plex") == user.preference("watch plex")
    many = user_vocabulary(tuple(f"term{i}" for i in range(50)), tuple(["term0 term1 term2"]))
    assert many.preference("term0 term1 term2") == PREFERENCE_CAP


def test_preference_orders_retrieval_of_equally_similar_phrases():
    # "watch alex" and "watch plex" are equally close to "watch flex": the user's order decides
    # which HA is asked about first.
    for phrases in (("watch Plex", "watch Alex"), ("watch Alex", "watch Plex")):
        proposal = propose(Request("watch flex"), recognized("watch flex"), tables(), user_vocabulary((), phrases))
        user = [c.text for c in proposal.validate if c.text in ("watch plex", "watch alex")]
        assert user == [fix_key(p) for p in phrases]


def test_preference_picks_among_spellings_of_one_interpretation_only():
    verdict = SentenceRecognition(True, False, "trigger", "(watch|put on) [the] plex")
    trigger = Candidate("watch plex", Rule.PHRASE, Source.TRIGGER, 0.90)
    user = user_vocabulary((), ("put on the Plex",))
    spelled = Candidate("put on the plex", Rule.PHRASE, Source.USER_PHRASE, 0.89,
                        preference=user.preference("put on the plex"))
    decision = decide(Proposal("wach plex", (trigger, spelled), (), ()), [verdict, verdict], None, tables())
    assert decision.selected.candidate is spelled and decision.preference_changed
    # Another interpretation within the margin stays ambiguous, preferred or not.
    other = SentenceRecognition(True, False, "trigger", "watch netflix")
    decision = decide(Proposal("wach plex", (trigger, spelled), (), ()), [verdict, other], None, tables())
    assert decision.reason == Reason.AMBIGUOUS and decision.selected is None


def test_preference_cannot_lift_a_phrase_below_the_threshold():
    phrase = "watch the Plex channel"
    assert similarity("watch flex", phrase) < 0.85
    proposal = propose(Request("watch flex"), recognized("watch flex"), tables(), user_vocabulary(("Plex",), (phrase,)))
    assert fix_key(phrase) not in [c.text for c in proposal.validate + proposal.held]


def test_preference_cannot_erase_ambiguity():
    decision, _ = correct("shower", phrases=["shower time"])
    assert decision.reason == Reason.AMBIGUOUS and decision.selected is None


def test_preference_cannot_bypass_protected_words():
    decision, proposal = correct("turn on the kitchen ceiling light", phrases=["turn off the kitchen ceiling lights"])
    assert rejected(proposal, "turn off the kitchen ceiling lights") in ("drops an action", "adds a polarity")
    assert "off" not in decision.selected.candidate.text.split()


# --- settings and history -------------------------------------------------------------------


def test_settings_parse_and_reject_what_the_api_rejects():
    assert fix_config({"transcriptFix": "on", "transcriptFixTerms": ["  Cherub   Rock "],
                       "transcriptFixPhrases": []}) == FixConfig(TextFixMode.ON, ("Cherub Rock",), ())
    assert fix_config({}) == FixConfig(TextFixMode.OFF)
    with pytest.raises(ValueError, match="one of"):
        parse_mode("maybe")
    with pytest.raises(ValueError, match="duplicates"):
        parse_list(["YouTube TV", "youtube t.v."])
    with pytest.raises(ValueError, match="letter or digit"):
        parse_list(["  ", "Plex"])
    with pytest.raises(ValueError, match="list of strings"):
        parse_list("Plex")


def test_history_is_what_was_sent_and_repetition_earns_nothing():
    applied = json.dumps({"applied": True, "sent": "play cherub rock"})
    shadow = json.dumps({"applied": False, "sent": "plate cherub rock", "proposed": "play cherub rock"})
    assert sent_text("Plate cherub rock.", applied) == "play cherub rock"
    assert sent_text("Plate cherub rock.", shadow) == "Plate cherub rock."
    layer = build_tables(vocabulary(), [], [HistoryEntry("watch plex", 1, 1.0), HistoryEntry("Watch Plex.", 9, 9.0)],
                         1).history
    assert len(layer) == 1 and layer.phrases[0].turn_id == 9


# --- service: revisions and stale results ---------------------------------------------------


class FakeHa:
    def __init__(self):
        self.connected = True
        self.generation = 1
        self.templates = list(FIXTURE["templates"])
        self.listing = asyncio.Event()
        self.listing.set()
        self.calls: list[tuple[tuple[str, ...], str | None, str | None]] = []
        self.disconnect = []

    def add_disconnect_listener(self, listener):
        self.disconnect.append(listener)

    def track_triggers(self, on_change):
        self.triggers_changed = on_change

    async def trigger_sentences(self):
        await self.listing.wait()
        return self.templates

    async def recognize(self, sentences, device_id, *, language=None, timeout=None):
        self.calls.append((tuple(sentences), device_id, language))
        return [recognized(s) for s in sentences]


async def ready_service(monkeypatch, ha=None, history=()):
    monkeypatch.setattr(em_text_fix_service, "REBUILD_DEBOUNCE_S", 0.0)
    ha = ha or FakeHa()

    async def load():
        return list(history)

    service = TextFixService(ha, load)
    service.start()
    service.on_vocabulary(vocabulary())
    await service._rebuild
    return service, ha


def test_the_service_corrects_with_the_pipeline_language_and_the_satellite(monkeypatch):
    async def main():
        service, ha = await ready_service(monkeypatch)
        config = FixConfig(TextFixMode.ON)
        context = service.context(config, DEVICE, "p1", "en", None)
        outcome = await service.correct("watch flex", context, config)
        assert outcome.decision.selected.candidate.text == "watch plex"
        assert all(device == DEVICE and language == "en" for _, device, language in ha.calls)
        service.close()
    asyncio.run(main())


@pytest.mark.parametrize("change", ["disconnect", "registry", "triggers", "history"])
def test_a_result_is_stale_after_any_change_until_new_tables_publish(monkeypatch, change):
    async def main():
        service, ha = await ready_service(monkeypatch)
        context = service.context(FixConfig(TextFixMode.ON), DEVICE, "p1", "en", None)
        assert service.valid(context)
        if change == "disconnect":
            ha.connected = False
            for listener in ha.disconnect:
                listener()
        elif change == "registry":
            service.on_registry_stale()
        elif change == "triggers":
            ha.triggers_changed()
        else:
            service.add_history(HistoryEntry("watch netflix", 700, 2.0))
            await service._history_build
        assert not service.valid(context)
        if change in ("disconnect", "registry"):
            # Reconnecting or rebuilding waits for the refreshed vocabulary: not ready till then.
            ha.connected = True
            ha.generation += change == "disconnect"
            await asyncio.sleep(0.01)
            assert service.current() is None
            with pytest.raises(NotReady):
                await service.correct("watch flex", context, FixConfig(TextFixMode.ON))
            service.on_vocabulary(vocabulary())
        if change != "history":
            await service._rebuild
        fresh = service.context(FixConfig(TextFixMode.ON), DEVICE, "p1", "en", None)
        assert service.valid(fresh) and fresh.tables > context.tables
        service.close()
    asyncio.run(main())


def test_a_build_invalidated_while_it_runs_never_publishes(monkeypatch):
    async def main():
        service, ha = await ready_service(monkeypatch)
        ha.listing.clear()
        ha.triggers_changed()                     # build 1 waits on the trigger listing
        first = service._rebuild
        await asyncio.sleep(0.01)
        ha.templates = [t for t in ha.templates if "plex" not in t]
        service.on_registry_stale()               # invalidates build 1 before it can publish
        ha.listing.set()
        await asyncio.gather(first, return_exceptions=True)
        assert service.current() is None
        service.on_vocabulary(vocabulary())
        await service._rebuild
        tables = service.current()
        assert tables is not None and tables.revision == service.revision
        assert "watch plex" not in {p.text for p in tables.triggers.phrases}
        service.close()
    asyncio.run(main())


def test_added_history_is_retrieved_without_a_full_rebuild(monkeypatch):
    async def main():
        service, ha = await ready_service(monkeypatch)
        before = service.current()
        service.add_history(HistoryEntry("play cherub rock", 532, 3.0))
        await service._history_build
        after = service.current()
        assert after.triggers is before.triggers and after.revision > before.revision
        assert [p.text for p in after.history.phrases] == ["play cherub rock"]
        service.close()
    asyncio.run(main())
