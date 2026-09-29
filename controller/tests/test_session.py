"""Session actor (em_session) against fake link, renderer, speech worker, HA, and alerts.

The device is simulated at the wire: `stream.open`, EMA1 mic/cell/reference
frames with consistent clocks, and WIRE control messages. The fake worker
turns scripted per-sample VAD and ASR tokens into observations, so every
endpoint decision runs through the real CellAssembler/Attributor/reducer.
"""

from __future__ import annotations

import asyncio
import uuid
from dataclasses import dataclass

import numpy as np
import pytest

import em_session
from em_arbiter import WakeArbiter
from em_audio_timeline import (
    FLAG_DIGITAL_SILENCE,
    Kind,
    build_cells,
    build_packet,
)
from em_device_link import AckStatus, CommandAck, Envelope, MessageType
from em_ha_client import HaUnavailable, IntentEnded, RunEnded, TtsReady
from em_session import ActorDeps, SessionActor
from em_speech_worker import (
    AsrPayload,
    EchoPayload,
    Observation,
    ReferenceScorePayload,
    VadPayload,
    VerificationPayload,
)
from em_wake_scorer import ReferenceCandidate

MIC_EPOCH, REF_EPOCH = 11, 22
BLOCK = 1_280
CELL = 512
MONO_BASE = 10**12
SPEECH_DB, QUIET_DB = -30.0, -70.0


@pytest.fixture(autouse=True)
def fast_deadlines(monkeypatch):
    monkeypatch.setattr(em_session, "_stream_url", lambda url: url)
    monkeypatch.setattr(em_session, "RESPONSE_STALL_S", 60.0)
    monkeypatch.setattr(em_session, "WAKE_VERIFY_S", 0.3)
    monkeypatch.setattr(em_session, "REPLY_S", 0.3)


def run(coro):
    return asyncio.run(asyncio.wait_for(coro, 30))


def envelope(msg_type, body, message_id="m"):
    return Envelope(MessageType(msg_type), "s", message_id, "dev1", 0, body)


# --- fakes --------------------------------------------------------------------------------


class FakeLink:
    closed = False
    render: "FakeRender | None" = None

    def __init__(self):
        self.sent: list[tuple[str, dict, int]] = []
        self.acks: list[tuple[str, str, str | None]] = []
        self.requests: list[tuple[str, dict]] = []

    async def send(self, msg_type, body, *, generation=0):
        self.sent.append((msg_type, body, generation))
        if msg_type == "focus.acquire" and body["focus"] == "dialog_input" and self.render is not None:
            # The device's focus policy (WIRE §4.3, SPEC §6.2): dialog input cancels current dialog output.
            for playback in self.render.streams:
                playback.finish("cancelled")
        return str(uuid.uuid4())

    async def request(self, msg_type, body, *, generation=0, timeout=2.0):
        self.requests.append((msg_type, body))
        return CommandAck("x", AckStatus.DURABLE, None)

    async def ack(self, message_id, status, error=None):
        self.acks.append((message_id, status, error))

    def of(self, msg_type):
        return [(b, g) for t, b, g in self.sent if t == msg_type]


class FakePlayback:
    def __init__(self, source, generation, announcement):
        loop = asyncio.get_running_loop()
        self.playback_id = str(uuid.uuid4())
        self.source = source
        self.generation = generation
        self.announcement = announcement
        self.started = loop.create_future()
        self.finished = loop.create_future()
        self.last_progress = None
        self.completed_frames = 0
        self.sent_frames = 0

    @property
    def done(self):
        return self.finished.done()

    def start(self):
        if not self.started.done():
            self.started.set_result(None)
            self.last_progress = {"event": "start"}

    def finish(self, reason):
        if not self.done:
            self.finished.set_result({"reason": reason})
        if not self.started.done():
            self.started.set_exception(RuntimeError(reason))
            self.started.exception()

    async def cancel(self, reason):
        self.finish("cancelled")


class FakeRender:
    def __init__(self):
        self.streams: list[FakePlayback] = []
        self.local: list[tuple[str, str]] = []
        self.failed = None

    async def play_stream(self, source_class, pcm48, *, generation, gain_db=0.0, announcement=False):
        assert source_class == "dialog_output"
        playback = FakePlayback(pcm48, generation, announcement)
        playback.start()
        self.streams.append(playback)
        return playback

    async def play_local(self, source_class, asset, *, generation, gain_db=0.0):
        self.local.append((source_class, asset))

    def fail_all(self, reason):
        self.failed = reason
        for p in self.streams:
            p.finish("failed")

    def urls(self):
        return [p.source for p in self.streams]


class FakeRun:
    def __init__(self, events=()):
        self.queue: asyncio.Queue = asyncio.Queue()
        self.abandoned = False
        for e in events:
            self.queue.put_nowait(e)

    def __aiter__(self):
        return self

    async def __anext__(self):
        if self.abandoned:
            raise StopAsyncIteration
        event = await self.queue.get()
        if event is None or self.abandoned:
            raise StopAsyncIteration
        return event

    def abandon(self):
        self.abandoned = True
        self.queue.put_nowait(None)


class FakeHa:
    def __init__(self):
        self.stt_text = "What time is it?"
        self.stt_calls: list[bytes] = []
        self.intents: list[tuple[str, str | None]] = []
        self.tts: list[str] = []
        self.next_run = FakeRun()
        self.handled: list[tuple[str, dict, str | None]] = []
        self.intent_responses: dict[str, object] = {}   # intent name → response dict or exception

    async def handle_intent(self, name, slots, device_id):
        self.handled.append((name, slots, device_id))
        response = self.intent_responses.get(name, {"response_type": "action_done"})
        if isinstance(response, Exception):
            raise response
        return response

    async def run_stt(self, pipeline_id, device_id, pcm):
        self.stt_calls.append(pcm)
        return self.stt_text

    async def run_intent_tts(self, pipeline_id, device_id, text, conversation_id):
        self.intents.append((text, conversation_id))
        return self.next_run

    async def run_tts(self, pipeline_id, text, device_id=None):
        self.tts.append(text)
        return f"tts://{text}"

    async def time_zone(self):
        return "UTC"


@dataclass(frozen=True)
class Model:
    graph_sha256: str = "g" * 64
    wake_phrase: str = "ophelia"
    verify_core: str = "ophel"


class FakeRegistry:
    def get(self, sha):
        return Model()

    def for_config(self, config):
        return Model()


class FakeWorker:
    """Scripted evidence: `vad(sample)`, `tokens` [(token, emission sample)], verification result."""

    policy_hash = "post_afe_1"

    def __init__(self):
        self.actor: SessionActor | None = None
        self.available = True
        self.vad = lambda s: 0.05
        self.tokens: list[tuple[str, int]] = []
        self.verification: str | None = "pass"
        self.reference_candidates: list[ReferenceCandidate] = []
        self.leases: dict[str, dict] = {}
        self.closed: list[str] = []
        self.verifications: list[str] = []

    def post(self, lease_id, kind, through, payload, utterance_id=None, stream="mic"):
        self.actor.on_observation(Observation(
            self.actor.device_id, MIC_EPOCH, stream, "controller", kind, through, utterance_id,
            None, self.policy_hash, payload, 0, lease_id))

    async def load_wake_graph(self, sha):
        return None

    def open_lease(self, device_id, epoch, lease_id, sha):
        self.leases[lease_id] = {"next_cell": None, "utt": None, "mic_through": None}

    def close_lease(self, lease_id):
        if self.leases.pop(lease_id, None) is not None:
            self.closed.append(lease_id)

    def open_utterance(self, lease_id, utterance_id, start, trigger, mic, **_):
        lease = self.leases[lease_id]
        fed = max(lease["mic_through"] or start, start)
        lease["utt"] = [utterance_id, start, fed]
        if fed > start:
            self._asr(lease_id)

    def close_utterance(self, lease_id):
        if lease_id in self.leases:
            self.leases[lease_id]["utt"] = None

    def _asr(self, lease_id):
        utt_id, start, fed = self.leases[lease_id]["utt"]
        emitted = [(t, s) for t, s in self.tokens if start <= s <= fed]
        last = emitted[-1][1] if emitted else start
        self.post(lease_id, "asr", fed, AsrPayload(
            tuple(t for t, _ in emitted), tuple(s for _, s in emitted), "", None, (fed - last) // 640, None),
            utt_id)

    def submit_mic_block(self, lease_id, first, pcm, *, flags=0):
        lease = self.leases[lease_id]
        end = first + pcm.size
        lease["mic_through"] = end
        if lease["next_cell"] is None:
            lease["next_cell"] = -(-first // CELL) * CELL
        cells = []
        start = lease["next_cell"]
        while lease["next_cell"] + CELL <= end:
            cells.append(self.vad(lease["next_cell"]))
            lease["next_cell"] += CELL
        if cells:
            self.post(lease_id, "vad", lease["next_cell"], VadPayload(start, tuple(cells)))
        if lease["utt"] is not None and end > lease["utt"][2]:
            lease["utt"][2] = end
            self._asr(lease_id)

    def submit_echo(self, lease_id, first_cell_sample, through_sample, compare, utterance_id=None):
        results, lag = compare()
        self.post(lease_id, "echo", through_sample, EchoPayload(first_cell_sample, tuple(results), lag), utterance_id)

    def submit_reference(self, lease_id, view):
        self.post(lease_id, "reference_score", view.frontier, ReferenceScorePayload(
            view.first_sample, REF_EPOCH, (), (), tuple(self.reference_candidates)), stream="reference")

    def submit_verification(self, lease_id, candidate_id, mic, support_start, open_sample, core):
        self.verifications.append(candidate_id)
        if self.verification is not None:
            self.post(lease_id, "verification", open_sample + 7_680,
                      VerificationPayload(candidate_id, "ophilia", 0.2, self.verification))

    async def decode_span(self, pcm):
        return tuple(t for t, _ in self.tokens), tuple(0.0 for _ in self.tokens)


class FakeAlerts:
    def __init__(self):
        self.calls = []
        self.alarms: list[dict] = []
        self.set_result: dict = {"ok": True}

    async def set_alarm(self, *args, **kwargs):
        self.calls.append(("set_alarm", args, kwargs))
        return self.set_result

    def list_alarms(self, endpoint_id):
        return {"ok": True, "alarms": self.alarms}


class Device:
    """Wire-level device: monotonic uplink streams with a shared capture/reference clock."""

    def __init__(self, actor: SessionActor):
        self.actor = actor
        self.seq = {Kind.MIC: 0, Kind.CELLS: 0, Kind.REFERENCE: 0}
        self.pos: dict[Kind, int] = {}
        self.level = lambda s: QUIET_DB
        self.mic_pcm = lambda a, b: np.zeros(b - a, dtype=np.int16)
        self.ref_pcm = None           # None: digital silence

    def open_streams(self):
        for stream_id, kind, epoch, fmt in (("mic", 1, MIC_EPOCH, 1), ("cells", 4, MIC_EPOCH, 2),
                                            ("reference", 2, REF_EPOCH, 1)):
            self.actor.on_message(envelope("stream.open", {
                "stream_id": stream_id, "epoch": str(epoch), "kind": kind, "sample_rate": 16000,
                "format": fmt, "reason": "start"}))

    def _packet(self, kind, first, **kw):
        self.seq[kind] += 1
        epoch = REF_EPOCH if kind == Kind.REFERENCE else MIC_EPOCH
        mono = 0 if kind == Kind.CELLS else MONO_BASE + first * 62_500
        return build_packet(kind, epoch=epoch, sequence=self.seq[kind], first_sample=first,
                            mono_ns=mono, uncertainty_us=1_000, **kw)

    def feed(self, start: int, until: int):
        """Send every stream from `start` (or where it stopped) through `until`, 80 ms at a time."""
        for kind in self.seq:
            self.pos.setdefault(kind, start - start % CELL)
        while self.pos[Kind.MIC] < until:
            a = self.pos[Kind.MIC]
            b = a + BLOCK
            self.actor.on_audio(self._packet(Kind.MIC, a, pcm=self.mic_pcm(a, b)))
            self.pos[Kind.MIC] = b
            c = self.pos[Kind.CELLS]
            n = (b - c) // CELL
            if n:
                levels = [int(self.level(c + i * CELL) * 100) for i in range(n)]
                self.actor.on_audio(self._packet(Kind.CELLS, c, cells=build_cells(levels, [0] * n, [0] * n)))
                self.pos[Kind.CELLS] = c + n * CELL
            r = self.pos[Kind.REFERENCE]
            if self.ref_pcm is None:
                self.actor.on_audio(self._packet(Kind.REFERENCE, r, frame_count=b - r,
                                                 flags=FLAG_DIGITAL_SILENCE))
            else:
                self.actor.on_audio(self._packet(Kind.REFERENCE, r, pcm=self.ref_pcm(r, b)))
            self.pos[Kind.REFERENCE] = b

    def backfill_cells(self, start: int):
        """Cells ride 10 s ahead of mic for `B` (§16.6): send them before the mic stream starts."""
        self.pos[Kind.CELLS] = start - start % CELL


class Harness:
    def __init__(self, config=None):
        self.link = FakeLink()
        self.render = FakeRender()
        self.worker = FakeWorker()
        self.ha = FakeHa()
        self.alerts = FakeAlerts()
        self.arbiter = WakeArbiter()
        self.rows: list[dict] = []
        self.replies: list[bytes] = []
        self.reply_events: list = []
        self.events = []
        self.config = {"wakeSound": True, "wakeArbitrationMs": 700, **(config or {})}
        self.link.render = self.render

        async def persist(row):
            self.rows.append(row)
            return len(self.rows)

        async def record_continuation(row_id, outcome):
            self.rows[row_id - 1]["continuation"] = outcome

        async def pipeline():
            return "p1"

        async def esphome_reply(pcm):
            self.replies.append(pcm)
            for e in self.reply_events:
                yield e

        self.actor = SessionActor("dev1", ActorDeps(
            ha=self.ha, worker=self.worker, registry=FakeRegistry(), alerts=self.alerts, arbiter=self.arbiter,
            config=lambda: self.config, ha_device_id=lambda: "hadev", pipeline_id=pipeline,
            vocabulary=lambda: None, esphome_reply=esphome_reply, persist_turn=persist,
            record_continuation=record_continuation))
        self.worker.actor = self.actor
        self.actor.add_listener(self.events.append)
        self.device = Device(self.actor)

    async def start(self):
        await self.actor.start()
        self.actor.attach(self.link, self.render)
        self.device.open_streams()
        await self.settle()

    async def settle(self, rounds=400):
        idle = 0
        for _ in range(rounds):
            await asyncio.sleep(0)
            idle = idle + 1 if self.actor._queue.empty() else 0
            if idle > 20:
                return

    async def feed(self, start, until, step=BLOCK * 4):
        pos = start
        while pos < until:
            nxt = min(until, pos + step)
            self.device.feed(start, nxt)
            await self.settle()
            pos = nxt

    def terminals(self):
        return [e.reason for e in self.events if e.kind == "terminal"]

    def cues(self):
        return [e.reason for e in self.events if e.kind == "cue"]

    async def wait_for(self, predicate, timeout=5.0):
        loop = asyncio.get_running_loop()
        end = loop.time() + timeout
        while not predicate():
            if loop.time() > end:
                raise AssertionError("condition not reached")
            await asyncio.sleep(0.01)
        await self.settle()


S = 48_000                  # candidate support start
OPEN = S + 12_800           # opening hop end = trigger


def candidate(*, producing_sound=False, active_alert=None, lease="L1", cid="C1"):
    hops = [{"end_sample": str(S + 2_560 * k), "raw": 0.95 if k == 5 else 0.3,
             "smoothed": 0.92 if k == 5 else 0.3, "profile": "playback" if producing_sound else "idle"}
            for k in range(1, 6)]
    return envelope("wake.candidate", {
        "candidate_id": cid, "lease_id": lease, "capture_epoch": str(MIC_EPOCH), "graph_sha256": "g" * 64,
        "scorer_revision": 3, "profile": hops[0]["profile"], "threshold": 0.9 if not producing_sound else 0.65,
        "producing_sound": producing_sound, "first_crossing_end": str(OPEN), "support_start": str(S),
        "mono_ns": "1", "active_alert": active_alert, "hops": hops}, message_id=f"msg-{cid}")


def speech(*spans):
    return lambda s: 0.95 if any(a <= s < b for a, b in spans) else 0.05


def levels(*spans):
    return lambda s: SPEECH_DB if any(a <= s < b for a, b in spans) else QUIET_DB


# --- wake acceptance ----------------------------------------------------------------------------


def test_idle_wake_is_accepted_and_its_candidate_lease_becomes_the_turn_lease():
    async def main():
        h = Harness()
        await h.start()
        h.actor.on_message(candidate())
        await h.settle()
        assert h.link.acks == [("msg-C1", "accepted", None)]
        renew = h.link.of("uplink.renew")
        assert renew[0][1] == 2 and renew[0][0]["reason"] == "turn" and renew[0][0]["lease_id"] == "L1"
        focus = h.link.of("focus.acquire")
        assert focus[0][0]["focus"] == "dialog_input" and focus[0][0]["owner"] == renew[0][0]["owner"]
        assert h.render.local == [("earcon", "builtin:wake_chime")]
        assert h.actor.state == "ARMED" and h.actor.turn_active
        assert h.arbiter.claim("other", 0.7) == "dev1"
        await h.actor.close()
    run(main())


def test_a_wake_while_speech_is_unavailable_is_refused_with_the_error_cue():
    async def main():
        h = Harness()
        await h.start()
        h.worker.available = False
        h.actor.on_message(candidate())
        await h.settle()
        assert h.link.acks == [("msg-C1", "rejected", "speech_unavailable")]
        assert h.cues() == ["error_anim"] and h.actor.state == "IDLE"
        assert not h.link.of("focus.acquire")
        await h.actor.close()
    run(main())


def test_a_wake_lost_to_another_speaker_closes_as_arbitration_lost():
    async def main():
        h = Harness()
        await h.start()
        h.arbiter.claim("kitchen", 0.7)
        h.actor.on_message(candidate())
        await h.settle()
        assert h.link.of("uplink.close")[0][0] == {"lease_id": "L1", "reason": "arbitration_lost"}
        assert h.terminals() == ["arbitration_lost"]
        assert not h.link.of("focus.acquire") and not h.render.local
        await h.wait_for(lambda: h.rows)
        assert h.rows[0]["terminal_reason"] == "arbitration_lost"
        await h.actor.close()
    run(main())


async def producing_sound_case(h: Harness):
    await h.start()
    h.actor.on_message(candidate(producing_sound=True))
    await h.settle()
    await h.feed(S - 4_800 - 20_000, OPEN + 9_000)
    await h.wait_for(lambda: h.terminals())
    return h.terminals()


def test_a_wake_like_reference_candidate_rejects_as_self_output():
    async def main():
        h = Harness()
        h.worker.reference_candidates = [ReferenceCandidate(OPEN, S, OPEN + 1_000, 0.6)]
        assert await producing_sound_case(h) == ["self_output"]
        assert h.link.of("uplink.close")[0][0] == {"lease_id": "L1", "reason": "rejected"}
        assert not h.link.of("focus.acquire")
        assert not [b for b, _ in h.link.of("uplink.renew") if b.get("reason") == "turn"]
        await h.actor.close()
    run(main())


def test_mic_audio_explained_by_the_final_mix_rejects_as_echo_only():
    async def main():
        h = Harness()
        rng = np.random.default_rng(3)
        ref = rng.normal(0, 3000, 400_000).astype(np.int16)
        lag = 1_600
        h.device.ref_pcm = lambda a, b: ref[a:b]
        h.device.mic_pcm = lambda a, b: (ref[a - lag:b - lag] // 2).astype(np.int16)
        h.device.level = lambda s: SPEECH_DB if S <= s < OPEN + 6_000 else QUIET_DB
        h.worker.vad = speech((S, OPEN + 6_000))
        h.worker.verification = "pass"
        assert await producing_sound_case(h) == ["echo_only"]
        await h.actor.close()
    run(main())


def test_a_candidate_the_verifier_does_not_hear_as_the_wake_word_is_unverified():
    async def main():
        h = Harness()
        h.worker.verification = "fail"
        assert await producing_sound_case(h) == ["unverified_wake"]
        assert h.worker.verifications == ["C1"]
        await h.actor.close()
    run(main())


def test_a_verification_missing_the_deadline_rejects_and_never_accepts():
    async def main():
        h = Harness()
        h.worker.verification = None
        assert await producing_sound_case(h) == ["verifier_timeout"]
        assert not h.link.of("focus.acquire")
        await h.wait_for(lambda: h.rows)
        assert h.rows[0]["wake_attribution"] == "verifier_timeout"
        await h.actor.close()
    run(main())


# --- wake + local command -------------------------------------------------------------------


def test_wake_then_stop_with_a_ringing_alarm_dismisses_it_locally_without_home_assistant():
    async def main():
        h = Harness()
        stop = (OPEN + 3_200, OPEN + 7_296)
        h.worker.vad = speech((S, OPEN - 512), stop)
        h.device.level = levels((S, OPEN - 512), stop)
        h.worker.tokens = [("▁OPHELIA", OPEN - 1_800), ("▁STOP", OPEN + 5_200)]
        await h.start()
        alert = {"id": "occ-1", "kind": "alarm", "name": "Wake up", "foreground": False}
        h.actor.on_message(candidate(active_alert=alert))
        await h.settle()
        assert h.render.local == []                        # the ring stopping is the acknowledgement
        await h.feed(S - 4_800 - 20_000, OPEN + 40_000)
        await h.wait_for(lambda: h.link.requests)
        msg, body = h.link.requests[0]
        assert msg == "alert.act" and body["target_id"] == "occ-1" and body["action"] == "dismiss"
        assert h.ha.stt_calls == [] and h.ha.intents == []
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        assert row["outcome"] == "local_command" and row["terminal_reason"] == "completed"
        assert row["commit_route"] == "A" and row["stt_text"] == "stop"
        await h.actor.close()
    run(main())


def test_a_wake_turn_records_each_stage_what_asr_heard_what_ha_transcribed_and_what_it_answered():
    async def main():
        h = Harness()
        question = (OPEN + 3_200, OPEN + 11_392)
        h.worker.vad = speech((S, OPEN - 512), question)
        h.device.level = levels((S, OPEN - 512), question)
        h.worker.tokens = [("▁OPHELIA", OPEN - 1_800), ("▁WHAT", OPEN + 4_000), ("▁TIME", OPEN + 6_000),
                           ("▁IS", OPEN + 8_000), ("▁IT", OPEN + 10_000)]
        h.ha.stt_text = "Ophelia, what time is it?"
        h.ha.next_run = FakeRun([IntentEnded("It is noon.", "conv-1", False, "query_answer", True),
                                 TtsReady("http://ha/tts/1", False), RunEnded()])
        await h.start()
        h.actor.on_message(candidate())
        await h.settle()
        await h.feed(S - 4_800 - 20_000, OPEN + 60_000)
        await h.wait_for(lambda: h.render.streams)
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        # The wake word is cut from the text sent to intent, never from what was heard.
        assert row["asr_text"] == "OPHELIA WHAT TIME IS IT"
        assert row["stt_raw"] == "Ophelia, what time is it?"
        assert row["stt_text"] == "what time is it?"
        assert h.ha.intents == [("what time is it?", None)]
        assert row["commit_route"] == "A" and row["endpoint_class"] == "unknown"
        assert row["endpoint_ms"] >= 1_792                 # unknown text waits the longest pause
        assert row["response_text"] == "It is noon." and row["response_type"] == "query_answer"
        assert row["intent_local"] is True
        assert row["intent_ms"] is not None and row["tts_url_ms"] is not None
        await h.actor.close()
    run(main())


# --- button turns and Home Assistant --------------------------------------------------------------

PRESS = 80_000
QUESTION = (PRESS + 1_536, PRESS + 9_728)
WHAT_TIME = [("▁WHAT", PRESS + 2_500), ("▁TIME", PRESS + 4_000), ("▁IS", PRESS + 6_000), ("▁IT", PRESS + 8_000)]


def button(press=PRESS):
    return {"click_type": 138, "capture_epoch": str(MIC_EPOCH), "capture_sample": str(press),
            "physical_seq": 1, "occurrence_id": None, "handled": None}


async def button_turn(h: Harness, until: int):
    h.worker.vad = speech(QUESTION)
    h.device.level = levels(QUESTION)
    h.worker.tokens = WHAT_TIME
    await h.start()
    h.actor.button_turn(button())
    await h.settle()
    await h.feed(PRESS - 4_800, until)


def test_a_button_turn_opens_its_own_lease_from_the_press_minus_300_ms():
    async def main():
        h = Harness()
        await h.start()
        h.actor.button_turn(button())
        await h.settle()
        body, gen = h.link.of("uplink.open")[0]
        assert gen == 1 and body["reason"] == "turn"
        assert int(body["streams"]["mic"]) == (PRESS - 4_800) // CELL * CELL
        assert int(body["streams"]["cells"]) < int(body["streams"]["mic"])
        assert h.actor.state == "ARMED" and h.render.local == []
        await h.actor.close()
    run(main())


def test_a_timer_cancel_home_assistant_may_have_received_is_never_resent():
    async def main():
        h = Harness()
        h.ha.stt_text = "Cancel all timers."
        h.ha.intent_responses["HassTimerStatus"] = timer_status(
            ha_timer("a", 5, 192), ha_timer("b", 10, 480), ha_timer("k", 5, 30, device="kitchen"))
        h.ha.intent_responses["HassCancelTimer"] = HaUnavailable("timed out")
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.rows)
        assert h.rows[0]["terminal_reason"] == "outcome_unknown"
        assert [n for n, _, _ in h.ha.handled] == ["HassTimerStatus", "HassCancelTimer"] and h.ha.intents == []
        await h.wait_for(lambda: h.ha.tts)
        assert h.ha.tts == [em_session.LINE_UNKNOWN]
        await h.actor.close()
    run(main())


def test_alarm_questions_are_answered_from_the_alert_engine_and_timer_questions_go_to_home_assistant():
    async def main():
        h = Harness()
        h.ha.stt_text = "When's my next alarm?"
        h.alerts.alarms = [{"name": "Alarm", "kind": "alarm", "due": "2030-01-02T07:00:00+00:00",
                            "repeats": [], "schedule_id": "s1"}]
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        assert h.render.urls() == ["tts://Your next alarm is 7 AM on January 2."]
        assert h.ha.handled == [] and h.ha.intents == []
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.rows)
        assert h.rows[0]["outcome"] == "alarm_query"
        await h.actor.close()

        # HA's own local HassTimerStatus intent answers timer questions.
        h = Harness()
        h.ha.stt_text = "How much time is left?"
        h.ha.next_run = FakeRun([IntentEnded("Your 5 minute timer has 3 minutes left.", "c", False,
                                             "action_done", True), TtsReady("http://ha/tts/1", False), RunEnded()])
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        assert h.ha.intents == [("How much time is left?", None)] and h.ha.handled == []
        await h.actor.close()
    run(main())


def ha_timer(id_, minutes, left, name="", device="hadev", seconds=0):
    return {"id": id_, "name": name, "device_id": device, "is_active": True, "total_seconds_left": left,
            "start_hours": 0, "start_minutes": minutes, "start_seconds": seconds}


def timer_status(*timers):
    return {"response_type": "action_done", "speech_slots": {"timers": list(timers)}}


def cancels(h: Harness) -> list[tuple[str, dict]]:
    return [(n, s) for n, s, _ in h.ha.handled if n != "HassTimerStatus"]


def test_cancel_the_timer_names_the_one_it_cancelled_which_home_assistant_cannot():
    async def main():
        h = Harness()
        h.ha.stt_text = "Cancel the timer."
        h.ha.intent_responses["HassTimerStatus"] = timer_status(
            ha_timer("a", 0, 4, seconds=5), ha_timer("k", 5, 30, device="kitchen"))
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        assert cancels(h) == [("HassCancelTimer", {"start_seconds": 5})] and h.ha.intents == []
        assert h.render.urls() == ["tts://5 second timer cancelled."]
        await h.actor.close()
    run(main())


def test_a_timer_cancel_by_length_goes_to_home_assistants_local_intent():
    async def main():
        h = Harness()
        h.ha.stt_text = "Cancel the 10 minute timer."
        h.ha.next_run = FakeRun([IntentEnded("10 minute timer cancelled.", "c", False, "action_done", True),
                                 TtsReady("http://ha/tts/1", False), RunEnded()])
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        assert h.ha.intents == [("Cancel the 10 minute timer.", None)] and h.ha.handled == []
        await h.actor.close()
    run(main())


def test_cancel_all_timers_cancels_and_names_only_this_speakers_timers():
    async def main():
        for timers, expected in (
            # HassCancelAllTimers would cancel the kitchen's timer too.
            ((ha_timer("a", 5, 192), ha_timer("b", 10, 480), ha_timer("k", 5, 30, device="kitchen")),
             [("HassCancelTimer", {"start_minutes": 5}), ("HassCancelTimer", {"start_minutes": 10})]),
            ((ha_timer("a", 5, 192), ha_timer("b", 10, 480)), [("HassCancelAllTimers", {})]),
        ):
            h = Harness()
            h.ha.stt_text = "Cancel all my timers."
            h.ha.intent_responses["HassTimerStatus"] = timer_status(*timers)
            await button_turn(h, PRESS + 50_000)
            await h.wait_for(lambda: h.render.streams)
            assert cancels(h) == expected and h.ha.intents == []
            assert h.render.urls() == ["tts://5 minute and 10 minute timers cancelled."]
            await h.actor.close()
    run(main())


def test_cancel_the_timer_with_several_running_asks_which_and_the_reply_picks_it_without_a_wake_word():
    async def main():
        h = Harness()
        h.ha.stt_text = "Cancel the timer."
        h.ha.intent_responses["HassTimerStatus"] = timer_status(ha_timer("a", 5, 192), ha_timer("b", 10, 480))
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        assert h.render.urls() == ["tts://Which one? Your 5 minute timer or your 10 minute timer?"]
        assert [name for name, _, _ in h.ha.handled] == ["HassTimerStatus"]
        base = PRESS + 50_000
        await h.feed(base, base + 24_000)
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.actor.state == "EXPECT_REPLY")
        answer = (base + 26_112, base + 35_840)
        h.worker.vad = speech(answer)
        h.device.level = levels(answer)
        h.worker.tokens = [("▁THE", base + 27_000), ("▁TEN", base + 29_000), ("▁MINUTE", base + 31_000),
                           ("▁ONE", base + 33_000)]
        h.ha.stt_text = "The ten minute one."
        await h.feed(base + 24_000, base + 80_000)
        await h.wait_for(lambda: len(h.render.streams) == 2)
        assert h.ha.handled[-1] == ("HassCancelTimer", {"start_minutes": 10}, "hadev")
        assert h.render.urls()[1] == "tts://10 minute timer cancelled."
        h.render.streams[1].finish("drained")
        await h.wait_for(lambda: len(h.rows) == 2)
        question, reply = h.rows
        assert question["outcome"] == "clarification" and question["continuation"] == "answered"
        assert reply["trigger"] == "reply" and reply["outcome"] == "timer"
        assert h.ha.intents == []                        # no conversation agent at any step
        await h.actor.close()
    run(main())


def test_a_voice_alarm_is_confirmed_with_the_time_the_alert_engine_scheduled():
    async def main():
        h = Harness()
        h.ha.stt_text = "Set an alarm for 7 AM."
        h.alerts.set_result = {"ok": True, "first_due": "2030-01-02T07:00:00+00:00", "days": []}
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        assert [c[0] for c in h.alerts.calls] == ["set_alarm"] and h.ha.intents == []
        assert h.render.urls() == ["tts://Alarm set for 7 AM on January 2."]
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.rows)
        assert h.rows[0]["outcome"] == "alarm" and h.rows[0]["response_text"] == "Alarm set for 7 AM on January 2."
        await h.actor.close()
    run(main())


def reply_focus(h: Harness, owner: str) -> list[dict]:
    return [b for b, _ in h.link.of("focus.acquire") if b["owner"] == owner and b["focus"] == "dialog_input"]


def test_ha_turn_with_continue_conversation_opens_a_reply_window_that_times_out():
    async def main():
        h = Harness()
        h.ha.next_run = FakeRun([
            IntentEnded("Which TV?", "conv-1", True, "action_done", False),
            TtsReady("http://ha/tts/1", False), RunEnded()])
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        assert h.ha.intents == [("What time is it?", None)]
        response = h.render.streams[0]
        assert response.source == "http://ha/tts/1" and h.actor.state == "SPEAKING"
        reply_open = [b for b, _ in h.link.of("uplink.open") if b["reason"] == "reply"]
        assert reply_open and reply_open[0]["streams"] == {"mic": "live", "cells": "live", "reference": "live"}
        # The question keeps playing: input focus, which the device answers by cancelling
        # dialog output, waits for the drain.
        assert not response.done and reply_focus(h, reply_open[0]["owner"]) == []
        response.finish("drained")
        await h.wait_for(lambda: h.actor.state == "EXPECT_REPLY")
        assert len(reply_focus(h, reply_open[0]["owner"])) == 1
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        assert row["terminal_reason"] == "completed" and row["commit_route"] == "A"
        assert row["continuation"] == "pending" and row["playback_reason"] == "drained"
        assert row["conversation_id"] == "conv-1" and row["reply_to"] is None
        await h.wait_for(lambda: h.actor.state == "IDLE", timeout=3)
        assert h.terminals()[-1] == "reply_timeout"
        closes = [b for b, _ in h.link.of("uplink.close") if b["lease_id"] == reply_open[0]["lease_id"]]
        assert closes == [{"lease_id": reply_open[0]["lease_id"], "reason": "closed"}]
        assert len(h.ha.intents) == 1                      # a silent window never dispatches
        await h.wait_for(lambda: h.rows[0]["continuation"] == "reply_timeout")
        await h.actor.close()
    run(main())


def test_an_answer_to_home_assistants_question_is_sent_on_in_its_conversation_without_a_wake_word():
    async def main():
        h = Harness()
        h.ha.next_run = FakeRun([
            IntentEnded("Which TV?", "conv-1", True, "action_done", False),
            TtsReady("http://ha/tts/1", False), RunEnded()])
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        base = PRESS + 50_000
        await h.feed(base, base + 24_000)                    # quiet question tail
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.actor.state == "EXPECT_REPLY")
        answer = (base + 26_112, base + 35_840)
        h.worker.vad = speech(answer)
        h.device.level = levels(answer)
        h.worker.tokens = [("▁LIVING", base + 29_000), ("▁ROOM", base + 32_000)]
        h.ha.stt_text = "The living room."
        h.ha.next_run = FakeRun([IntentEnded("Turned off.", "conv-1", False, "action_done", False),
                                 TtsReady("http://ha/tts/2", False), RunEnded()])
        await h.feed(base + 24_000, base + 80_000)
        await h.wait_for(lambda: len(h.render.streams) == 2)
        assert h.ha.intents[1] == ("The living room.", "conv-1")
        h.render.streams[1].finish("drained")
        await h.wait_for(lambda: len(h.rows) == 2)
        question, reply = h.rows
        assert question["continuation"] == "answered"
        assert reply["trigger"] == "reply" and reply["reply_to"] == question["turn_uuid"]
        assert reply["conversation_id"] == "conv-1" and reply["continuation"] is None
        await h.actor.close()
    run(main())


def test_a_streamed_answer_that_ends_in_a_question_watches_for_a_reply_while_it_plays():
    async def main():
        h = Harness()
        run_ = FakeRun([TtsReady("http://ha/tts/s", True)])
        h.ha.next_run = run_
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)          # audible before intent-end
        assert not [b for b, _ in h.link.of("uplink.open") if b["reason"] == "reply"]
        run_.queue.put_nowait(IntentEnded("It is noon. Anything else?", "conv-1", True, "action_done", False))
        run_.queue.put_nowait(RunEnded())
        await h.wait_for(lambda: [b for b, _ in h.link.of("uplink.open") if b["reason"] == "reply"])
        response = h.render.streams[0]
        assert not response.done
        response.finish("drained")
        await h.wait_for(lambda: h.actor.state == "EXPECT_REPLY")
        await h.actor.close()
    run(main())


def test_a_question_whose_audio_is_cancelled_drops_its_reply_expectation():
    async def main():
        h = Harness()
        h.ha.next_run = FakeRun([
            IntentEnded("Which TV?", "conv-1", True, "action_done", False),
            TtsReady("http://ha/tts/1", False), RunEnded()])
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        reply_open = [b for b, _ in h.link.of("uplink.open") if b["reason"] == "reply"]
        h.render.streams[0].finish("cancelled")
        await h.wait_for(lambda: h.rows)
        assert h.actor.state == "IDLE" and not h.actor.turn_active
        row = h.rows[0]
        assert row["continuation"] == "prompt_cancelled" and row["playback_reason"] == "cancelled"
        closes = [b for b, _ in h.link.of("uplink.close") if b["lease_id"] == reply_open[0]["lease_id"]]
        assert closes == [{"lease_id": reply_open[0]["lease_id"], "reason": "closed"}]
        assert reply_focus(h, reply_open[0]["owner"]) == []
        await h.actor.close()
    run(main())


def test_an_intent_run_past_its_deadline_closes_as_ha_timeout_and_later_events_are_fenced(monkeypatch):
    monkeypatch.setattr(em_session, "HA_TIMEOUT_S", 0.2)

    async def main():
        h = Harness()
        run_ = FakeRun()
        h.ha.next_run = run_
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: "ha_timeout" in h.terminals())
        assert run_.abandoned
        assert h.cues() == ["error_anim"]
        await h.wait_for(lambda: h.render.streams)
        assert h.ha.tts == ["Something went wrong."]
        run_.queue.put_nowait(IntentEnded("late", None, False, None, False))
        run_.queue.put_nowait(TtsReady("http://ha/tts/late", True))
        await asyncio.sleep(0.1)
        await h.settle()
        assert "http://ha/tts/late" not in h.render.urls()
        await h.actor.close()
    run(main())


def test_privacy_mute_mid_turn_closes_it_as_muted_without_dispatch():
    async def main():
        h = Harness()
        await button_turn(h, PRESS + 6_000)
        assert h.actor.state in ("ARMED", "LISTENING")
        h.actor.on_message(envelope("privacy.changed", {"muted": True, "capture_epoch": None, "physical_seq": 2}))
        await h.settle()
        assert h.terminals() == ["muted"]
        assert h.cues() == [] and h.ha.stt_calls == [] and h.ha.tts == []
        assert h.actor.state == "IDLE" and h.actor.uplink.timelines == {}
        await h.actor.close()
    run(main())


def test_session_loss_closes_the_turn_and_sends_nothing_more():
    async def main():
        h = Harness()
        await button_turn(h, PRESS + 6_000)
        sent = len(h.link.sent)
        h.actor.detach("session_lost")
        await h.settle()
        assert h.terminals() == ["session_lost"]
        assert h.cues() == [] and h.render.failed == "session_lost"
        assert len(h.link.sent) == sent
        assert h.actor.state == "IDLE" and not h.actor.turn_active
        await h.actor.close()
    run(main())


# --- HA-started conversations ------------------------------------------------------------------------


def test_announcement_with_start_conversation_sends_the_reply_through_the_esphome_path():
    async def main():
        h = Harness()
        await h.start()
        h.reply_events = [IntentEnded("Okay.", "conv-9", False, None, False), TtsReady("http://ha/tts/ok", False)]
        announce = asyncio.create_task(h.actor.announce("http://ha/announce", preannounce_url=None,
                                                        start_conversation=True))
        await h.wait_for(lambda: h.render.streams)
        prompt = h.render.streams[0]
        assert prompt.announcement and prompt.source == "http://ha/announce"
        reply_open = [b for b, _ in h.link.of("uplink.open") if b["reason"] == "reply"]
        assert len(reply_open) == 1
        base = 200_000
        await h.feed(base, base + 24_000)                    # quiet prompt tail
        prompt.finish("drained")
        await asyncio.wait_for(announce, 2)
        answer = (base + 26_112, base + 35_840)
        h.worker.vad = speech(answer)
        h.device.level = levels(answer)
        h.worker.tokens = [("▁BLUE", base + 30_000)]
        await h.feed(base + 24_000, base + 80_000)
        await h.wait_for(lambda: h.replies)
        assert h.ha.stt_calls == [] and h.ha.intents == []   # HA runs STT with its stored prompt
        await h.wait_for(lambda: len(h.render.streams) == 2)
        h.render.streams[1].finish("drained")
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        assert row["trigger"] == "ha_reply" and row["commit_route"] == "R" and row["terminal_reason"] == "completed"
        await h.actor.close()
    run(main())
