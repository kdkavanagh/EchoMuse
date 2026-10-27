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
import os
import signal
from dataclasses import replace
from pathlib import Path

import pytest

pytest.importorskip("hassil")

import em_phonetics
import em_text_fix_service
import em_text_fix_worker
from em_phonetics import Lexicon, PronunciationSource, best_similarity, parse_guides
from em_ha_client import (
    HaUnavailable,
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
    PHRASE_MIN,
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
    compile_phonetic,
    decide,
    fix_config,
    fix_key,
    parse_list,
    parse_mode,
    propose,
    sent_text,
    user_vocabulary,
)
from em_text_fix_service import NotReady, TextFixService
from em_text_fix_worker import Build, ChildWorker, Decide, Pronounce, Propose, Stale, Unknown, history_rows

from _text_fix_fakes import inline_workers

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


@functools.cache
def phonetic(history: bool = True, guides: tuple[tuple[str, str], ...] = ()):
    return compile_phonetic(tables(history), Lexicon(guides))


def recognized(sentence: str) -> SentenceRecognition | None:
    assert sentence in FIXTURE["recognitions"], f"no recorded verdict for {sentence!r}: re-record the fixture"
    return _sentence_recognition(FIXTURE["recognitions"][sentence])


def proposed(text: str, *, wake: str | None = None, terms=(), phrases=(), history: bool = True, guides=()):
    key = fix_key(text)
    guides = parse_guides(dict(guides))
    user = user_vocabulary(tuple(terms), tuple(phrases), Lexicon(guides))
    return propose(Request(key, wake), recognized(key), tables(history), phonetic(history, guides), user)


def correct(text: str, **kw):
    proposal = proposed(text, **kw)
    original = recognized(proposal.text)
    decision = decide(proposal, [recognized(c.text) for c in proposal.validate], original, tables(kw.get("history", True)))
    return decision, proposal


def rejected(proposal: Proposal, text: str) -> str:
    """Why `text` was rejected before HA saw it."""
    for r in proposal.rejected:
        if r.candidate.text == text:
            assert r.status == Status.NOT_PRESERVED
            return r.detail
    raise AssertionError(f"{text!r} was not rejected: {[r.candidate.text for r in proposal.rejected]}")


def unrecognized(text: str, *, phrases=()) -> Proposal:
    """The proposal for `text` if HA's matcher recognized nothing of it (no slot to fill):
    for near misses that preservation must reject before HA is ever asked."""
    lexicon = Lexicon()
    return propose(Request(fix_key(text)), None, tables(), phonetic(), user_vocabulary((), tuple(phrases), lexicon))


# --- eligible corrections -------------------------------------------------------------------


@pytest.mark.parametrize("heard, sent, rule, source", [
    ("What time is", "what time is it", Rule.CUTOFF, Source.TRIGGER),           # turns 557, 560, 565, …
    ("watch you", "watch youtube tv", Rule.CUTOFF, Source.TRIGGER),             # turn 673
    ("Watch Flex.", "watch plex", Rule.PHRASE, Source.TRIGGER),                 # turn 770
    ("Plate cherub rock.", "play cherub rock", Rule.PHRASE, Source.HISTORY),    # turn 532
    ("RMV alarm.", "arm the alarm", Rule.PHRASE, Source.TRIGGER),               # turn 773
    ("arm v alarm.", "arm the alarm", Rule.PHRASE, Source.TRIGGER),             # turn 777
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
        assert chosen.candidate.raw >= PHRASE_MIN


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
    assert proposed("what time is it").validate == ()
    assert decide(Proposal("what time is it", (), (), ()), [], original, tables()).reason == Reason.ORIGINAL_MATCHED


def test_shower_is_ambiguous_between_shower_mode_and_shower_time():
    # Turn 660: both are complete triggers HA accepts, with the same coverage.
    decision, _ = correct("shower")
    accepted = {ch.candidate.text for ch in decision.checked if ch.status == Status.OK}
    assert {"shower mode", "shower time"} <= accepted
    assert decision.reason == Reason.AMBIGUOUS and decision.selected is None


def test_near_empty_text_proposes_nothing():
    lexicon = Lexicon()
    for text in ("st", "Ophelia."):
        proposal = propose(Request(fix_key(text)), None, tables(), phonetic(), user_vocabulary((), (), lexicon))
        assert proposal.validate == () and proposal.rejected == ()


# --- hard negatives -------------------------------------------------------------------------


def test_a_negation_is_never_dropped_or_added():
    # "can" and "can't" sound alike (one segment apart): the phrase is proposed, and refused.
    proposal = unrecognized("you can stop the music", phrases=["you can't stop the music"])
    assert rejected(proposal, "you can't stop the music") == "negation"
    decision, _ = correct("don't disarm the security alarm please")
    assert decision.selected is None


def test_a_direction_is_never_dropped():
    # "louder" and "lower" sound alike; only the first names which way.
    proposal = unrecognized("make it louder", phrases=["make it lower"])
    assert rejected(proposal, "make it lower") == "drops an action"
    decision, _ = correct("turn on the kitchen ceiling light")
    assert decision.selected.candidate.text == "turn on the Kitchen Ceiling Lights"


def test_lock_is_never_swapped_for_unlock():
    decision, proposal = correct("lock the front door")
    assert decision.selected is None and not any("unlock" in c.text for c in proposal.validate)


def test_a_number_or_unit_is_never_changed():
    # "fifteen" and "fifty" differ in one segment; "minute" and "minutes" in one.
    proposal = unrecognized("set a timer for fifteen minutes", phrases=["set a timer for fifty minutes"])
    assert rejected(proposal, "set a timer for fifty minutes") == "number"
    proposal = unrecognized("set a timer for one minute", phrases=["set a timer for one minutes"])
    assert rejected(proposal, "set a timer for one minutes") == "unit"


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


def test_arming_at_home_is_heard_in_v_and_rmv_and_arming_away_is_not():
    # Turns 773 and 777: "arm the alarm" heard as "RMV alarm" and "arm v alarm". "v" is said
    # like "the" (DH IY, its CMUdict variant) but one voiced fricative apart; "away" was
    # never said, so the away trigger is far from both.
    lexicon = Lexicon()

    def alike(a: str, b: str) -> float:
        return best_similarity(lexicon.pronounce(a).variants(), lexicon.pronounce(b).variants())

    assert alike("v", "the") >= PHRASE_MIN and alike("rmv", "arm the") >= PHRASE_MIN
    assert alike("v", "the away") < PHRASE_MIN and alike("rmv", "arm the away") < PHRASE_MIN
    for heard in ("RMV alarm.", "arm v alarm."):
        texts = [c.text for c in proposed(heard).validate]
        assert "arm the alarm" in texts and not any("away" in t for t in texts)


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
    assert fix_key(a.text) == fix_key(b.text) == "watch plex" and a.raw == b.raw
    assert a.preference == 0 and 0 < b.preference <= PREFERENCE_CAP


def test_higher_entries_weigh_more_and_repeats_add_nothing():
    lexicon = Lexicon()
    user = user_vocabulary(("Plex", "Netflix"), (), lexicon)
    assert user.preference("watch plex") > user.preference("watch netflix") > 0
    assert user_vocabulary(("Netflix", "Plex"), (), lexicon).preference("watch netflix") == user.preference("watch plex")
    assert user.preference("plex plex plex plex") == user.preference("watch plex")
    many = user_vocabulary(tuple(f"term{i}" for i in range(50)), tuple(["term0 term1 term2"]), lexicon)
    assert many.preference("term0 term1 term2") == PREFERENCE_CAP


def test_preference_orders_retrieval_of_equally_similar_phrases():
    # "plecks", pronounced by a guide exactly as "plex", is exactly as close to "watch flex":
    # the user's order decides which HA is asked about first.
    guides = {"plecks": "P L EH K S"}
    for phrases in (("watch Plex", "watch Plecks"), ("watch Plecks", "watch Plex")):
        proposal = proposed("watch flex", phrases=phrases, guides=guides)
        user = [c.text for c in proposal.validate if c.text in ("watch plex", "watch plecks")]
        assert user == [fix_key(p) for p in phrases]


def test_preference_picks_among_spellings_of_one_interpretation_only():
    verdict = SentenceRecognition(True, False, "trigger", "(watch|put on) [the] plex")
    trigger = Candidate("watch plex", Rule.PHRASE, Source.TRIGGER, 0.90)
    user = user_vocabulary((), ("put on the Plex",), Lexicon())
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
    lexicon = Lexicon()
    changed = best_similarity(lexicon.pronounce("flex").variants(), lexicon.pronounce("the plex channel").variants())
    assert changed < PHRASE_MIN
    proposal = proposed("watch flex", terms=["Plex"], phrases=[phrase])
    assert fix_key(phrase) not in [c.text for c in proposal.validate + proposal.held]


def test_preference_cannot_erase_ambiguity():
    decision, _ = correct("shower", phrases=["shower time"])
    assert decision.reason == Reason.AMBIGUOUS and decision.selected is None


def test_preference_cannot_bypass_protected_words():
    # The user's own phrase, as close as it is, still may not drop the direction said.
    proposal = unrecognized("make it louder", phrases=["make it lower", "make it louder please"])
    assert rejected(proposal, "make it lower") == "drops an action"
    assert "make it lower" not in [c.text for c in proposal.validate + proposal.held]


# --- settings and history -------------------------------------------------------------------


def test_settings_parse_and_reject_what_the_api_rejects():
    assert fix_config({"transcriptFix": "on", "transcriptFixTerms": ["  Cherub   Rock "], "transcriptFixPhrases": [],
                       "transcriptFixPronunciations": {"YouTubeTV": "Y UW1 T UW2 B T IY1 V IY1"}}) == FixConfig(
        TextFixMode.ON, ("Cherub Rock",), (), (("youtubetv", "Y UW T UW B T IY V IY"),))
    assert fix_config({}) == FixConfig(TextFixMode.OFF)
    with pytest.raises(ValueError, match="one of"):
        parse_mode("maybe")
    with pytest.raises(ValueError, match="duplicates"):
        parse_list(["YouTube TV", "youtube t.v."])
    with pytest.raises(ValueError, match="letter or digit"):
        parse_list(["  ", "Plex"])
    with pytest.raises(ValueError, match="list of strings"):
        parse_list("Plex")
    with pytest.raises(ValueError, match="unknown phoneme 'Q'"):
        parse_guides({"plex": "P L EH Q S"})
    with pytest.raises(ValueError, match="one word"):
        parse_guides({"youtube tv": "Y UW T UW B T IY V IY"})
    with pytest.raises(ValueError, match="listed twice"):
        parse_guides({"Plex": "P L EH K S", "plex": "P L EH K S"})


# --- pronunciations -------------------------------------------------------------------------


def test_a_word_is_said_by_its_guide_then_the_dictionary_then_its_letters():
    guided = Lexicon(parse_guides({"the": "DH IY", "youtubetv": "Y UW T UW B T IY V IY"}))
    assert Lexicon().word("youtubetv") is None                       # not in CMUdict, has vowels
    assert guided.word("youtubetv").source == PronunciationSource.GUIDE
    assert Lexicon().word("the").arpabet == ("DH AH", "DH IY")       # CMUdict: AH however stressed
    assert guided.word("the").arpabet == ("DH IY",)                  # a guide replaces the dictionary
    assert Lexicon().word("rmv").source == PronunciationSource.LETTERS
    assert Lexicon().word("rmv").arpabet == ("AA R EH M V IY",)


def test_text_with_a_word_that_has_no_pronunciation_is_never_compared_by_sound():
    # "youtubetv" (a trigger's spelling) has no pronunciation until a guide gives it one: its
    # phrases are listed as unknown and have no pronunciation to retrieve them by.
    plain = phonetic()
    texts = [p.text for p in tables().triggers.phrases]
    owners = [i for i, t in enumerate(texts) if "youtubetv" in t.split()]
    assert owners and plain.unknown["youtubetv"] == len(owners)
    assert not set(owners) & set(plain.triggers.layer.owners.tolist())
    guided = phonetic(guides=(("youtubetv", "Y UW T UW B T IY V IY"),))
    assert "youtubetv" not in guided.unknown and set(owners) <= set(guided.triggers.layer.owners.tolist())


def test_words_the_heard_text_and_a_phrase_share_never_dilute_the_evidence():
    # "turn off the other tv" against "turn off the office tv" or "turn office tv off": the
    # whole texts sound alike, the one word that differs does not.
    lexicon = Lexicon()
    whole = best_similarity(lexicon.pronounce("turn off the other tv").variants(),
                            lexicon.pronounce("turn off the office tv").variants())
    assert whole >= PHRASE_MIN
    texts = [c.text for c in proposed("turn off the other tv").validate]
    assert not any("office" in t for t in texts)


def test_the_batched_distance_is_the_scalar_one():
    rng = __import__("random").Random(7)
    n = len(em_phonetics.SEGMENTS)
    rows = [bytes(rng.randrange(n) for _ in range(rng.randint(1, 14))) for _ in range(200)]
    layer = em_phonetics.PhoneticLayer.build([[r] for r in rows])
    for minimum in (0.0, 0.5, 0.75):
        for _ in range(15):
            query = bytes(rng.randrange(n) for _ in range(rng.randint(1, 14)))
            found = layer.search([query], minimum)
            for owner, row in enumerate(rows):
                expected = em_phonetics.similarity(query, row)
                if expected >= minimum + 1e-4:
                    assert found[owner] == pytest.approx(expected, abs=1e-4)
                elif expected < minimum - 1e-4:
                    assert owner not in found


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
        self.failure: Exception | None = None

    def add_disconnect_listener(self, listener):
        self.disconnect.append(listener)

    def track_triggers(self, on_change):
        self.triggers_changed = on_change

    async def trigger_sentences(self):
        await self.listing.wait()
        if self.failure is not None:
            raise self.failure
        return self.templates

    async def recognize(self, sentences, device_id, *, language=None, timeout=None):
        self.calls.append((tuple(sentences), device_id, language))
        return [recognized(s) for s in sentences]


async def ready_service(monkeypatch, ha=None, history=(), worker=None):
    """A service whose tables are published; its jobs run inline unless a different `worker`
    factory is given. Returns the service, HA and the worker it made."""
    monkeypatch.setattr(em_text_fix_service, "REBUILD_DEBOUNCE_S", 0.0)
    ha = ha or FakeHa()
    factory, made = inline_workers()
    workers = []

    def make(on_lost):
        workers.append((worker or factory)(on_lost))
        return workers[-1]

    async def load():
        return list(history)

    service = TextFixService(ha, load, worker=make)
    service.start()
    service.on_vocabulary(vocabulary())
    await service._rebuild
    assert service.current() is not None
    return service, ha, workers[0]


def renamed(vocab: Vocabulary) -> Vocabulary:
    """`vocab` with one more area: "Potting Shed"."""
    return replace(vocab, areas=(*vocab.areas, VocabArea("potting_shed", ("Potting Shed",), None)))


def test_the_service_corrects_with_the_pipeline_language_and_the_satellite(monkeypatch):
    async def main():
        service, ha, _ = await ready_service(monkeypatch)
        config = FixConfig(TextFixMode.ON)
        context = service.context(config, DEVICE, "p1", "en", None)
        outcome = await service.correct("watch flex", context, config)
        assert outcome.decision.selected.candidate.text == "watch plex"
        assert all(device == DEVICE and language == "en" for _, device, language in ha.calls)
        await service.close()
    asyncio.run(main())


@pytest.mark.parametrize("change", ["disconnect", "vocabulary", "triggers", "history", "worker"])
def test_a_result_is_stale_after_any_change_until_new_tables_publish(monkeypatch, change):
    async def main():
        service, ha, worker = await ready_service(monkeypatch)
        context = service.context(FixConfig(TextFixMode.ON), DEVICE, "p1", "en", None)
        assert service.valid(context)
        if change == "disconnect":
            ha.connected = False
            for listener in ha.disconnect:
                listener()
        elif change == "vocabulary":
            service.on_vocabulary(renamed(vocabulary()))
        elif change == "triggers":
            ha.triggers_changed()
        elif change == "history":
            service.add_history(HistoryEntry("watch netflix", 700, 2.0))
            await service._history_build
        else:
            worker.lose()
        assert not service.valid(context)
        if change == "disconnect":
            # Reconnecting waits for the refreshed vocabulary, unchanged or not: not ready till then.
            ha.connected = True
            ha.generation += 1
            await asyncio.sleep(0.01)
            assert service.current() is None
            with pytest.raises(NotReady):
                await service.correct("watch flex", context, FixConfig(TextFixMode.ON))
            service.on_vocabulary(vocabulary())
        if change != "history":
            await service._rebuild
        fresh = service.context(FixConfig(TextFixMode.ON), DEVICE, "p1", "en", None)
        assert service.valid(fresh) and fresh.tables > context.tables
        names = {n.text for n in worker.engine.tables(fresh.tables).names}
        assert ("Potting Shed" in names) == (change == "vocabulary")
        await service.close()
    asyncio.run(main())


def test_a_refresh_that_changed_nothing_keeps_the_tables_and_builds_nothing(monkeypatch):
    async def main():
        service, _, worker = await ready_service(monkeypatch)
        published, asked = service.current(), len(worker.asked)
        context = service.context(FixConfig(TextFixMode.ON), DEVICE, "p1", "en", None)
        service.on_vocabulary(vocabulary())              # equal, though built anew from HA's replies
        await asyncio.sleep(0.01)
        assert service.current() is published and service.revision == published.revision
        assert service.valid(context) and not any(isinstance(job, Build) for job in worker.asked[asked:])
        outcome = await service.correct("watch flex", context, FixConfig(TextFixMode.ON))
        assert outcome.decision.selected.candidate.text == "watch plex"
        await service.close()
    asyncio.run(main())


def test_after_a_build_that_failed_an_unchanged_refresh_builds_again(monkeypatch):
    async def main():
        service, ha, _ = await ready_service(monkeypatch)
        ha.failure = HaUnavailable("connection lost")
        ha.triggers_changed()
        await service._rebuild
        assert service.current() is None
        ha.failure = None
        service.on_vocabulary(vocabulary())
        await service._rebuild
        assert service.current() is not None
        await service.close()
    asyncio.run(main())


def test_a_build_invalidated_while_it_runs_never_publishes(monkeypatch):
    async def main():
        service, ha, worker = await ready_service(monkeypatch)
        ha.listing.clear()
        ha.triggers_changed()                     # build 1 waits on the trigger listing
        first = service._rebuild
        await asyncio.sleep(0.01)
        ha.templates = [t for t in ha.templates if "plex" not in t]
        service.on_vocabulary(renamed(vocabulary()))    # invalidates build 1 before it can publish
        ha.listing.set()
        await asyncio.gather(first, return_exceptions=True)
        await service._rebuild
        tables = service.current()
        assert tables is not None and tables.revision == service.revision
        built = worker.engine.tables(tables.revision)
        assert "watch plex" not in {p.text for p in built.triggers.phrases}
        assert "Potting Shed" in {n.text for n in built.names}
        await service.close()
    asyncio.run(main())


def test_added_history_is_retrieved_without_a_full_rebuild(monkeypatch):
    async def main():
        service, ha, worker = await ready_service(monkeypatch)
        before = service.current()
        service.add_history(HistoryEntry("play cherub rock", 532, 3.0))
        await service._history_build
        after = service.current()
        assert after.revision > before.revision
        old, new = worker.engine.tables(before.revision), worker.engine.tables(after.revision)
        assert new.triggers is old.triggers and new.names is old.names
        assert [p.text for p in new.history.phrases] == ["play cherub rock"]
        assert not any(isinstance(job, Build) for job in worker.asked[1:])
        await service.close()
    asyncio.run(main())


# --- the child process ----------------------------------------------------------------------


def test_the_child_decides_exactly_as_the_functions_do_in_process():
    """Build, propose and decide round trip through the child: the same Decision, the same
    pronunciations and unknown words as computed here."""
    texts = ["Watch Flex.", "RMV alarm.", "What time is", "Plate cherub rock.", "watch you"]

    async def main():
        lost = []
        worker = ChildWorker(lambda: lost.append(True))
        worker.start()
        lexicon = Lexicon()
        built = await worker.ask(Build(7, vocabulary(), tuple(FIXTURE["templates"]), history_rows(HISTORY)))
        assert built.revision == 7 and built.summary == {**tables().summary, "revision": 7}
        decisions = []
        for text in texts:
            key = fix_key(text)
            proposal = await worker.ask(Propose(7, Request(key), recognized(key), (), (), lexicon))
            verdicts = tuple(recognized(c.text) for c in proposal.validate)
            decisions.append((proposal, await worker.ask(Decide(7, proposal, verdicts, recognized(key)))))
        unknown = await worker.ask(Unknown(7, lexicon))
        said = await worker.ask(Pronounce(lexicon, ("rmv alarm", "youtubetv")))
        with pytest.raises(Stale):
            await worker.ask(Propose(6, Request("watch flex"), None, (), (), lexicon))
        await worker.close()
        return decisions, unknown, said, lost

    decisions, unknown, said, lost = asyncio.run(main())
    for text, (proposal, decision) in zip(texts, decisions):
        here, proposal_here = correct(text)
        assert proposal == proposal_here and decision == here
    assert [d.reason for _, d in decisions][:2] == [Reason.APPLIED, Reason.APPLIED]
    assert unknown == phonetic().unknown
    assert said == (Lexicon().pronounce("rmv alarm"), Lexicon().pronounce("youtubetv"))
    assert lost == []                             # a close is no loss


def test_a_child_that_dies_is_replaced_and_its_loss_withdraws_the_tables(monkeypatch):
    monkeypatch.setattr(em_text_fix_worker, "RESTART_MIN_S", 0.05)

    async def main():
        service, ha, worker = await ready_service(monkeypatch, worker=ChildWorker)
        config = FixConfig(TextFixMode.ON)
        context = service.context(config, DEVICE, "p1", "en", None)
        pid = worker.pid
        # A job in flight when the child dies fails, rather than waiting forever.
        pending = asyncio.ensure_future(service.pronounce(Lexicon(), ["play cherub rock"] * 300_000))
        await asyncio.sleep(0.05)
        os.kill(pid, signal.SIGKILL)
        with pytest.raises(NotReady):
            await pending
        for _ in range(500):
            if service.current() is None:
                break
            await asyncio.sleep(0.01)
        assert service.current() is None          # its tables went with it: not ready
        with pytest.raises(NotReady):
            await service.correct("watch flex", context, config)
        rebuilt = service._rebuild
        await asyncio.wait_for(rebuilt, 30)
        fresh = service.context(config, DEVICE, "p1", "en", None)
        assert fresh.tables > context.tables and worker.pid not in (None, pid)
        outcome = await service.correct("watch flex", fresh, config)
        assert outcome.decision.selected.candidate.text == "watch plex"
        await service.close()
        assert worker.pid is None

    asyncio.run(main())


# --- trying a transcript (the dashboard's tester) ------------------------------------------


class TrialHa(FakeHa):
    """FakeHa plus the pipeline lookups a trial makes."""

    async def resolve_pipeline(self, pipeline_id):
        return "preferred"

    async def satellite_pipeline_id(self, device_id):
        return f"pipeline-of-{device_id}"

    async def pipeline_language(self, pipeline_id, *, timeout=None):
        return "en"


@pytest.fixture
def controller(tmp_path, monkeypatch):
    """em_controller with one speaker, DOT, matched to the fixture's HA satellite, and HA
    answered by the fixture's recorded verdicts. The correction service starts in the test's
    own event loop (`start`)."""
    import em_controller
    import em_db

    em_db.init(str(tmp_path / "t.db"))
    em_db.register_new_device("DOT", "10.0.0.9", "fw", None)
    em_db.approve_device("DOT", "Office")
    ha = TrialHa()
    monkeypatch.setattr(em_controller, "_ha", ha)
    monkeypatch.setattr(em_controller, "_ha_identity", {"DOT": em_controller.HaIdentity(DEVICE, None)})
    monkeypatch.setattr(em_controller, "_registry", type("R", (), {
        "for_config": lambda self, config: type("M", (), {"wake_phrase": "ophelia"})()})())

    async def start():
        # The real child: the tester's pronunciations come from it too.
        service, _, _ = await ready_service(monkeypatch, ha=ha, worker=ChildWorker)
        monkeypatch.setattr(em_controller, "_text_fix", service)
        return service

    yield em_controller, ha, start
    em_db._conn.close()
    em_db._conn = None


def test_a_tried_transcript_shows_what_on_would_send_in_the_speakers_context(controller):
    em_controller, ha, start = controller

    async def main():
        service = await start()
        plex = await em_controller.trial_transcript("DOT", "Watch Flex.", True)
        dot_calls = list(ha.calls)
        ha.calls.clear()
        alarm = await em_controller.trial_transcript("DOT", "RMV alarm.", False)
        ha.calls.clear()
        fleet = await em_controller.trial_transcript(None, "Watch Flex.", True)
        await service.close()
        return plex, dot_calls, alarm, fleet, list(ha.calls)

    plex, dot_calls, alarm, fleet, fleet_calls = asyncio.run(main())
    assert plex.mode == TextFixMode.SHADOW            # the speaker's own setting, reported beside it
    record = plex.record.as_dict()
    assert (record["applied"], record["reason"], record["sent"]) == (True, "applied", "watch plex")
    assert (plex.satellite, plex.pipeline, plex.language, plex.wake_phrase) == (
        DEVICE, f"pipeline-of-{DEVICE}", "en", "ophelia")
    assert dot_calls and all((device, language) == (DEVICE, "en") for _, device, language in dot_calls)
    # "RMV alarm" is "arm the alarm"; the tester shows how each word was pronounced.
    record = alarm.record.as_dict(candidates=30)
    assert (record["applied"], record["reason"], record["sent"]) == (True, "applied", "arm the alarm")
    assert not any("away" in (c.get("matched") or "") for c in record["candidates"] if c["status"] == "ok")
    assert [(w, f.source if f else None) for w, f in zip(alarm.heard.words, alarm.heard.found)] == [
        ("rmv", "letters"), ("alarm", "dictionary")]
    assert alarm.proposed is not None and alarm.proposed.words == ("arm", "the", "alarm")
    assert alarm.wake_phrase is None
    assert (fleet.satellite, fleet.pipeline) == (None, "preferred")
    assert fleet_calls and all(device is None for _, device, _ in fleet_calls)


@pytest.mark.parametrize("text, route", [("set an alarm for 7 am", "alarm"), ("say that again", "repeat")])
def test_a_transcript_an_echomuse_route_takes_is_never_corrected(controller, text, route):
    em_controller, ha, start = controller

    async def main():
        service = await start()
        ha.calls.clear()
        trial = await em_controller.trial_transcript("DOT", text, True)
        await service.close()
        return trial

    trial = asyncio.run(main())
    assert (trial.route, trial.record) == (route, None) and ha.calls == []


def test_the_trial_api_reports_what_stops_a_trial():
    aiohttp = pytest.importorskip("aiohttp")
    from aiohttp.test_utils import make_mocked_request

    import em_api
    from em_ha_client import HaUnavailable

    async def call(body: dict, failure: Exception | None = None):
        async def trial_transcript(device_id, text, wake):
            raise failure

        app = aiohttp.web.Application()
        app[em_api.SERVICES] = type("S", (), {"trial_transcript": lambda self, *a: trial_transcript(*a)})()
        request = make_mocked_request("POST", "/api/text_fix/trial", app=app)
        request._read_bytes = json.dumps(body).encode()
        response = await em_api._post_text_fix_trial.__wrapped__(request)
        return response.status, json.loads(response.text)["code"]

    assert asyncio.run(call({"text": "rmv alarm", "wake": "yes"})) == (400, "invalid_input")
    assert asyncio.run(call({"text": "rmv alarm"}, NotReady("tables not ready"))) == (503, "not_ready")
    assert asyncio.run(call({"text": "rmv alarm"}, HaUnavailable("down"))) == (502, "ha_unavailable")
