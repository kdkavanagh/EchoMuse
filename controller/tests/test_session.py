"""Session actor (em_session) against fake link, renderer, speech worker, HA, and alerts.

The device is simulated at the wire: `stream.open`, EMA1 mic/cell/reference
frames with consistent clocks, and WIRE control messages. The fake worker
turns scripted per-sample VAD and ASR tokens into observations, so every
endpoint decision runs through the real CellAssembler/Attributor/reducer.
"""

from __future__ import annotations

import asyncio
import json
import uuid
from dataclasses import dataclass, field
from types import SimpleNamespace

import numpy as np
import pytest

import em_session
import em_text_fix_service
from em_arbiter import WakeArbiter
from em_audio_timeline import (
    FLAG_DIGITAL_SILENCE,
    AfeRecord,
    Kind,
    build_afe,
    build_cells,
    build_packet,
)
from em_device_link import AckStatus, CommandAck, Envelope, MessageType
from em_ha_client import AnswerDelta, HaUnavailable, IntentEnded, RunEnded, SentenceRecognition, TtsReady, Vocabulary
from em_pause_asr import PauseDecode, WyomingServer
from em_session import ActorDeps, SessionActor
from em_text_fix_service import TextFixService
from em_speech_worker import (
    AsrPayload,
    EchoPayload,
    Observation,
    ReferenceScorePayload,
    VadPayload,
    VerificationPayload,
    VoiceSnapshot,
)
from em_utterance import CellAssembler, Utterance, UtteranceKind, UtteranceSpec
from em_wake_scorer import ReferenceCandidate

from _text_fix_fakes import inline_workers

MIC_EPOCH, REF_EPOCH = 11, 22
BLOCK = 1_280
CELL = 512
PERIOD = 1_280              # one native AFE record (afe_metadata_v1)
MONO_BASE = 10**12
SPEECH_DB, QUIET_DB = -30.0, -70.0


@pytest.fixture(autouse=True)
def fast_deadlines(monkeypatch):
    monkeypatch.setattr(em_session, "_stream_url", lambda url: url)
    monkeypatch.setattr(em_session, "RESPONSE_STALL_S", 60.0)
    monkeypatch.setattr(em_session, "WAKE_VERIFY_S", 0.3)
    monkeypatch.setattr(em_text_fix_service, "REBUILD_DEBOUNCE_S", 0.0)


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
        self.first_frame_ns = None
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
        self.played: list[str] = []    # stream URLs and local assets, in the order they started
        self.failed = None
        self.autostart = True          # False: a stream's audio starts only when the test says
        self.local_finish = "drained"  # how a local asset's playback ends, at once; None: when the test says
        self.local_playbacks: list[FakePlayback] = []

    async def play_stream(self, source_class, pcm48, *, generation, gain_db=0.0, announcement=False):
        assert source_class == "dialog_output"
        playback = FakePlayback(pcm48, generation, announcement)
        if self.autostart:
            playback.start()
        self.streams.append(playback)
        self.played.append(pcm48)
        return playback

    async def play_local(self, source_class, asset, *, generation, gain_db=0.0):
        self.local.append((source_class, asset))
        self.played.append(asset)
        playback = FakePlayback(asset, generation, False)
        playback.start()
        self.local_playbacks.append(playback)
        if self.local_finish is not None:
            playback.finish(self.local_finish)
        return playback

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
        self.recognizer = False          # HA's sentence matcher answers (its startup probe passed)
        self.recognitions: dict[str, SentenceRecognition] = {}   # sentence → answer; absent: None
        self.recognized: list[tuple[str, ...]] = []
        self.warmups = 0
        # Transcript correction (§16.8): the matcher in a pipeline's language.
        self.connected = True
        self.generation = 1
        self.language = "en"
        self.calls: list[tuple[tuple[str, ...], str | None, str | None]] = []   # every recognize, in order
        self.fix_delay = 0.0
        self.fix_error: Exception | None = None
        self.on_fix = lambda sentences: None
        self.disconnect = []

    def add_disconnect_listener(self, listener):
        self.disconnect.append(listener)

    def track_triggers(self, on_change):
        pass

    async def trigger_sentences(self):
        return ["(watch|put on) [the] plex"]

    async def recognize(self, sentences, device_id, *, language=None, timeout):
        self.calls.append((tuple(sentences), device_id, language))
        if language is None:
            self.recognized.append(tuple(sentences))
        else:
            await asyncio.sleep(self.fix_delay)
            if self.fix_error is not None:
                raise self.fix_error
            self.on_fix(sentences)
        return [self.recognitions.get(s) for s in sentences]

    def fix_calls(self):
        return [c for c in self.calls if c[2] is not None]

    async def pipeline_language(self, pipeline_id, *, timeout):
        return self.language

    async def warm_recognizer(self, device_id, *, timeout):
        self.warmups += 1

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
    thresholds: SimpleNamespace = field(default_factory=lambda: SimpleNamespace(idle=0.9, playback=0.65))


class FakeRegistry:
    def get(self, sha):
        return Model()

    def for_config(self, config):
        return Model()


class FakeWorker:
    """Scripted evidence: `vad(sample)`, `tokens` [(token, emission sample)], verification result.
    With a pause server, `pause_text` = (sample, text) stands from that sample on: the first
    result there is Kroko's pause decode with the server's answer already in, as the worker
    reports a fast server; `span_text` is the server's committed-span transcript. A pause
    finalizes at each of `pauses`: an utterance's first result fed through one carries a
    voice snapshot taken there. `primes` records `prime_vad`."""

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
        self.pause_servers: list = []
        self.pause_text: tuple[int, str] | None = None
        self.span_text = ""
        self.pauses: list[int] = []
        self.primes: list[tuple[str, int, VoiceSnapshot | None]] = []
        self.span_requests = 0

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

    def open_utterance(self, lease_id, utterance_id, start, trigger, mic, *, pause_server=None, **_):
        self.pause_servers.append(pause_server)
        lease = self.leases[lease_id]
        fed = max(lease["mic_through"] or start, start)
        lease["utt"] = [utterance_id, start, fed, pause_server, False, set()]
        if fed > start:
            self._asr(lease_id)

    def close_utterance(self, lease_id):
        if lease_id in self.leases:
            self.leases[lease_id]["utt"] = None

    def _asr(self, lease_id):
        utt = self.leases[lease_id]["utt"]
        utt_id, start, fed, server = utt[:4]
        emitted = [(t, s) for t, s in self.tokens if start <= s <= fed]
        last = emitted[-1][1] if emitted else start
        text = decode = voice = None
        if server is not None and self.pause_text is not None and fed >= self.pause_text[0]:
            text = self.pause_text[1]
            if not utt[4]:
                utt[4] = True
                decode = PauseDecode(fed, text, 250, None)
        due = {p for p in self.pauses if start <= p <= fed} - utt[5]
        if due:
            utt[5] |= due
            voice = VoiceSnapshot(np.full((2, 1, 128), 0.5, dtype=np.float32), np.zeros(64, dtype=np.float32), fed)
        self.post(lease_id, "asr", fed, AsrPayload(
            tuple(t for t, _ in emitted), tuple(s for _, s in emitted), "", None, (fed - last) // 640, None,
            120 if decode is not None else None, pause_text=text, pause_decode=decode, voice=voice), utt_id)

    def prime_vad(self, lease_id, from_sample, voice):
        self.primes.append((lease_id, from_sample, voice))

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

    async def transcribe_span(self, pcm, server):
        self.span_requests += 1
        return self.span_text


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

    def __init__(self, actor: SessionActor, afe: bool = False):
        self.actor = actor
        self.seq = {Kind.MIC: 0, Kind.CELLS: 0, Kind.REFERENCE: 0}
        self.pos: dict[Kind, int] = {}
        self.level = lambda s: QUIET_DB
        self.mic_pcm = lambda a, b: np.zeros(b - a, dtype=np.int16)
        self.ref_pcm = None           # None: digital silence
        self.afe = afe                # the session carries the `afe` stream (afe_metadata_v1)
        self.send_afe = afe           # False: an opted-in device whose decoder sends nothing
        self.afe_seq = 0
        self.afe_pos: int | None = None
        self.afe_record = lambda start: AfeRecord(start, 10, 0, 0, 0, 0, 0, 0, 0, 216, 210, 1, 2, 70, 0)

    def open_streams(self):
        streams = [("mic", 1, MIC_EPOCH, 1), ("cells", 4, MIC_EPOCH, 2), ("reference", 2, REF_EPOCH, 1)]
        if self.afe:
            streams.append(("afe", 5, MIC_EPOCH, 3))
        for stream_id, kind, epoch, fmt in streams:
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
        if self.afe_pos is None:
            self.afe_pos = start - start % PERIOD
        while self.pos[Kind.MIC] < until:
            a = self.pos[Kind.MIC]
            b = a + BLOCK
            self.actor.on_audio(self._packet(Kind.MIC, a, pcm=self.mic_pcm(a, b)))
            self.pos[Kind.MIC] = b
            while self.send_afe and self.afe_pos + PERIOD <= b:
                self.afe_seq += 1
                self.actor.on_audio(build_packet(
                    Kind.AFE, epoch=MIC_EPOCH, sequence=self.afe_seq, first_sample=self.afe_pos,
                    afe=build_afe([self.afe_record(self.afe_pos)]), mono_ns=MONO_BASE + self.afe_pos * 62_500,
                    uncertainty_us=1_000))
                self.afe_pos += PERIOD
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
    def __init__(self, config=None, afe=False, no_input_chime=True, announcement_chime=True, text_fix=False,
                 turn_view=False):
        self.link = FakeLink()
        self.render = FakeRender()
        self.worker = FakeWorker()
        self.ha = FakeHa()
        self.alerts = FakeAlerts()
        self.arbiter = WakeArbiter()
        self.rows: list[dict] = []
        self.near_misses: list = []             # (em_db.NearMissRecord, pcm | None) as persisted
        self.replies: list[bytes] = []
        self.reply_events: list = []
        self.events = []
        self.config = {"wakeSound": True, "wakeArbitrationMs": 700, **(config or {})}
        self.link.render = self.render
        self.no_input_chime = no_input_chime   # the device announces no_input_chime
        self.announcement_chime = announcement_chime   # ... and announcement_chime
        self.turn_view = turn_view              # ... and turn_view_v1
        self.media_playing = False              # the Dot's own content player
        self.media_pauses = 0

        async def persist(row):
            self.rows.append(row)
            return len(self.rows)

        async def record_continuation(row_id, outcome):
            self.rows[row_id - 1]["continuation"] = outcome

        async def persist_near_miss(record, pcm):
            self.near_misses.append((record, pcm))

        async def pipeline():
            return "p1"

        async def pause_media():
            self.media_pauses += 1

        async def esphome_reply(pcm):
            self.replies.append(pcm)
            for e in self.reply_events:
                yield e

        self.text_fix = None
        if text_fix:
            async def no_history():
                return []

            self.text_fix = TextFixService(self.ha, no_history, worker=inline_workers()[0])

        self.actor = SessionActor("dev1", ActorDeps(
            ha=self.ha, worker=self.worker, registry=FakeRegistry(), alerts=self.alerts, arbiter=self.arbiter,
            config=lambda: self.config, ha_device_id=lambda: "hadev", recognizer=lambda: self.ha.recognizer,
            pipeline_id=pipeline,
            vocabulary=lambda: None, esphome_reply=esphome_reply, persist_turn=persist,
            record_continuation=record_continuation, persist_near_miss=persist_near_miss,
            media_playing=lambda: self.media_playing,
            pause_media=pause_media, text_fix=self.text_fix))
        self.worker.actor = self.actor
        self.actor.add_listener(self.events.append)
        self.device = Device(self.actor, afe)

    async def start(self):
        if self.text_fix is not None:
            # Tables holding the trigger "(watch|put on) [the] plex" (FakeHa.trigger_sentences).
            self.text_fix.start()
            self.text_fix.on_vocabulary(Vocabulary((), (), ()))
            await self.text_fix._rebuild
            assert self.text_fix.current() is not None
        await self.actor.start()
        self.actor.attach(self.link, self.render, afe_metadata=self.device.afe, no_input_chime=self.no_input_chime,
                          announcement_chime=self.announcement_chime, turn_view=self.turn_view)
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


def candidate(*, producing_sound=False, active_alert=None, lease="L1", cid="C1", chimed=None, start=S):
    hops = [{"end_sample": str(start + 2_560 * k), "raw": 0.95 if k == 5 else 0.3,
             "smoothed": 0.92 if k == 5 else 0.3, "profile": "playback" if producing_sound else "idle"}
            for k in range(1, 6)]
    body = {
        "candidate_id": cid, "lease_id": lease, "capture_epoch": str(MIC_EPOCH), "graph_sha256": "g" * 64,
        "scorer_revision": 3, "profile": hops[0]["profile"], "threshold": 0.9 if not producing_sound else 0.65,
        "producing_sound": producing_sound, "first_crossing_end": str(start + 12_800),
        "support_start": str(start), "mono_ns": "1", "active_alert": active_alert, "hops": hops}
    if chimed is not None:
        body["chimed"] = chimed
    return envelope("wake.candidate", body, message_id=f"msg-{cid}")


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


@pytest.mark.parametrize("chimed, controller_chime", [(False, True), (True, False)])
def test_the_controller_chimes_an_accepted_wake_only_when_the_device_did_not(chimed, controller_chime):
    async def main():
        h = Harness()
        await h.start()
        h.actor.on_message(candidate(chimed=chimed))
        await h.settle()
        assert h.actor.state == "ARMED" and h.actor.turn_active
        assert h.render.local == ([("earcon", "builtin:wake_chime")] if controller_chime else [])
        await h.actor.close()
    run(main())


# Turn 647 (2026-10-10 23:02, decision trace): an idle wake the Dot chimed itself. The recognizer
# heard "go turn off all of my scene", but no cell was ever speech, so no command speech ended and
# the turn closed `no_input` 5 s after its trigger, with only the LED throb to say so.
TURN_647_TOKENS = [("▁GO", OPEN + 12_800), ("▁TURN", OPEN + 33_280), ("▁OFF", OPEN + 40_000),
                   ("▁ALL", OPEN + 44_000), ("▁OF", OPEN + 47_000), ("▁MY", OPEN + 50_000),
                   ("▁SCENE", OPEN + 53_760)]


@pytest.mark.parametrize("wake_sound, capable, chime", [(True, True, True), (False, True, False),
                                                        (True, False, False)])
def test_a_wake_that_hears_no_command_ends_with_the_no_input_chime(wake_sound, capable, chime):
    async def main():
        h = Harness(config={"wakeSound": wake_sound}, no_input_chime=capable)
        h.worker.tokens = TURN_647_TOKENS
        await h.start()
        h.actor.on_message(candidate(chimed=True))
        await h.settle()
        await h.feed(S - 4_800 - 20_000, OPEN + 90_000)
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        assert row["terminal_reason"] == "no_input" and row["asr_text"] == "GO TURN OFF ALL OF MY SCENE"
        assert h.cues() == ["nospeech_anim"] and h.ha.stt_calls == []
        assert h.render.local == ([("earcon", "builtin:no_input_chime")] if chime else [])
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
        assert h.render.local == [("earcon", "builtin:wake_chime")]   # the wake program, as at idle
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


@pytest.mark.parametrize("word, local", [("STOP", True), ("PAUSE", True), ("CANCEL", False)])
def test_stop_or_pause_while_the_dot_plays_content_pauses_it_without_home_assistant(word, local):
    async def main():
        h = Harness()
        h.media_playing = True
        said = (OPEN + 3_200, OPEN + 7_296)
        h.worker.vad = speech((S, OPEN - 512), said)
        h.device.level = levels((S, OPEN - 512), said)
        h.worker.tokens = [("▁OPHELIA", OPEN - 1_800), (f"▁{word}", OPEN + 5_200)]
        h.ha.stt_text = f"Ophelia, {word.lower()}."
        h.ha.next_run = FakeRun([IntentEnded("", None, False, "action_done", True), RunEnded()])
        await h.start()
        h.actor.on_message(candidate())
        await h.settle()
        assert h.render.local == [("earcon", "builtin:wake_chime")]   # content only ducks: the wake still chimes
        await h.feed(S - 4_800 - 20_000, OPEN + 40_000)
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        if local:
            assert row["outcome"] == "local_command" and row["stt_text"] == word.lower()
            assert h.media_pauses == 1 and h.ha.stt_calls == [] and h.ha.intents == []
        else:                                   # "cancel" stops alerts and replies, never content
            assert row["outcome"] == "ha" and len(h.ha.intents) == 1 and h.media_pauses == 0
        await h.actor.close()
    run(main())


# Turn 654 (2026-10-11 12:17, Kitchen): "Ophelia, stop" over the Kitchen Dot's newscast lost
# arbitration to the idle Living Room Dot, which answered nothing; the newscast played on.
@pytest.mark.parametrize("words, local", [(["▁STOP"], True),
                                          (["▁WHAT", "▁TIME", "▁IS", "▁IT"], False)])
def test_a_wake_over_the_dots_content_that_lost_arbitration_acts_only_on_a_local_command(words, local):
    async def main():
        h = Harness()
        h.media_playing = True
        said = (OPEN + 3_200, OPEN + 11_392)
        h.worker.vad = speech((S, OPEN - 512), said)
        h.device.level = levels((S, OPEN - 512), said)
        h.worker.tokens = [("▁OPHELIA", OPEN - 1_800)] + [(w, OPEN + 4_000 + 2_000 * i)
                                                         for i, w in enumerate(words)]
        await h.start()
        h.arbiter.claim("living_room", 0.7)
        h.actor.on_message(candidate())
        await h.settle()
        assert h.link.of("focus.acquire") and h.render.local == []   # it listens; the winner chimes
        await h.feed(S - 4_800 - 20_000, OPEN + 80_000)
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        assert h.ha.stt_calls == [] and h.ha.intents == []
        assert h.cues() == [] and h.render.local == []
        assert json.loads(row["decision_trace"])["local_only"] is True
        if local:
            assert row["outcome"] == "local_command" and row["terminal_reason"] == "completed"
            assert h.media_pauses == 1
        else:
            assert row["outcome"] is None and row["terminal_reason"] == "arbitration_lost"
            assert h.media_pauses == 0
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
        # Its first frame played 60,000 samples past OPEN on the Dot's clock (MONO_BASE + sample × 62.5 µs).
        h.render.streams[0].first_frame_ns = MONO_BASE + (OPEN + 60_000) * 62_500
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
        assert row["first_audio_ms"] is not None and 0 <= row["first_audio_ms"] <= row["total_ms"]
        trace = json.loads(row["decision_trace"])
        assert trace["turn_id"] == row["turn_uuid"] and trace["first_audio_ms"] == row["first_audio_ms"]
        # End of the last word (the commit boundary) → that first frame, both on the Dot's clock.
        boundary = next(e["boundary"] for e in trace["utterance"]["endpoint"] if e["event"] == "commit")
        assert row["response_latency_ms"] == round((OPEN + 60_000 - boundary) / 16)
        await h.actor.close()
    run(main())


@pytest.mark.parametrize("recognizer", [True, False])
def test_text_home_assistant_recognizes_as_complete_ends_after_the_short_pause(recognizer):
    async def main():
        h = Harness()
        h.ha.recognizer = recognizer
        h.ha.recognitions = {"what time is it": SentenceRecognition(True, False, "trigger", "what time is it")}
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
        assert h.ha.warmups == (1 if recognizer else 0)    # warmed as the candidate opened
        await h.feed(S - 4_800 - 20_000, OPEN + 60_000)
        await h.wait_for(lambda: h.render.streams)
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        answers = json.loads(row["decision_trace"])["utterance"]["recognizer"]
        if recognizer:
            # Asked about the wake-cut prefix with the probe word; nothing extends it.
            assert ("what time is it", "what time is it zqxj") in h.ha.recognized
            assert answers[-1]["text"] == "what time is it" and answers[-1]["klass"] == "complete"
            assert row["endpoint_class"] == "complete" and 608 <= row["endpoint_ms"] < 1_792
        else:
            assert h.ha.recognized == [] and answers == []
            assert row["endpoint_class"] == "unknown" and row["endpoint_ms"] >= 1_792
        await h.actor.close()
    run(main())


PAUSE_SERVER = {"engine": "wyoming", "host": "stt", "port": 10300, "model": "", "language": "en"}
MISHEARD = [("▁FULL", OPEN - 1_800), ("▁FILLY", OPEN - 1_200), ("▁ATE", OPEN - 600), ("▁WHAT", OPEN + 4_000),
            ("▁TIME", OPEN + 6_000), ("▁IS", OPEN + 8_000), ("▁IT", OPEN + 10_000)]


@pytest.mark.parametrize("engine", ["wyoming", "kroko"])
def test_the_pause_servers_words_decide_when_kroko_mishears_the_wake_word(engine):
    """Kroko's "full filly ate what time is it" is unknown to HA; the server's "Ophelia, what
    time is it?" cuts to "what time is it", which HA recognizes, so the short pause applies."""
    async def main():
        h = Harness(config={"pauseAsr": {**PAUSE_SERVER, "engine": engine}})
        h.ha.recognizer = True
        h.ha.recognitions = {"what time is it": SentenceRecognition(True, False, "trigger", "what time is it")}
        question = (OPEN + 3_200, OPEN + 11_392)
        h.worker.vad = speech((S, OPEN - 512), question)
        h.device.level = levels((S, OPEN - 512), question)
        h.worker.tokens = MISHEARD
        h.worker.pause_text = (question[1] + 10_240, "Ophelia, what time is it?")   # 320 ms after the pause
        h.ha.next_run = FakeRun([IntentEnded("It is noon.", "conv-1", False, "query_answer", True),
                                 TtsReady("http://ha/tts/1", False), RunEnded()])
        await h.start()
        h.actor.on_message(candidate())
        await h.feed(S - 4_800 - 20_000, OPEN + 60_000)
        await h.wait_for(lambda: h.render.streams)
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        utterance = json.loads(row["decision_trace"])["utterance"]
        if engine == "wyoming":
            assert h.worker.pause_servers == [WyomingServer("stt", 10300, "", "en")]
            assert utterance["stable_prefix"] == "what time is it"
            [pause] = utterance["pauses"]
            assert (pause["server"]["text"], pause["server"]["judged"]) == ("Ophelia, what time is it?",
                                                                            "what time is it")
            commit = next(e for e in utterance["endpoint"] if e["event"] == "commit")
            assert commit["source"] == "server"
            assert row["endpoint_class"] == "complete" and 608 <= row["endpoint_ms"] < 1_792
            assert row["policy_hash"] == "post_afe_1+wyoming:default"
        else:
            assert h.worker.pause_servers == [None] and utterance["pauses"] == []
            assert utterance["stable_prefix"] == "full filly ate what time is it"
            assert row["endpoint_class"] == "unknown" and row["endpoint_ms"] >= 1_792
            assert row["policy_hash"] == "post_afe_1"
        await h.actor.close()
    run(main())


@pytest.mark.parametrize("span_text, terminal", [
    ("Ophelia, what time is it?", "completed"),
    ("Ophelia, what time is it in Paris?", "retry"),     # the span holds more than the committed text
])
def test_a_route_b_commit_on_the_servers_words_is_re_decoded_by_the_server(span_text, terminal):
    """After the pause the server's words stand; quieter speech (a TV) follows, so route B
    commits the complete command, and its re-decode must use the same server."""
    async def main():
        h = Harness(config={"pauseAsr": PAUSE_SERVER})
        h.ha.recognizer = True
        h.ha.recognitions = {"what time is it": SentenceRecognition(True, False, "trigger", "what time is it")}
        question = (OPEN + 3_200, OPEN + 11_392)
        tv = (question[1] + 6_400, OPEN + 80_000)
        h.worker.vad = speech((S, OPEN - 512), question, tv)
        loud = levels((S, OPEN - 512), question)
        h.device.level = lambda s: SPEECH_DB - 20 if tv[0] <= s < tv[1] else loud(s)
        h.worker.tokens = MISHEARD
        h.worker.pause_text = (question[1] + 10_240, "Ophelia, what time is it?")
        h.worker.span_text = span_text
        h.ha.next_run = FakeRun([IntentEnded("It is noon.", "conv-1", False, "query_answer", True),
                                 TtsReady("http://ha/tts/1", False), RunEnded()])
        await h.start()
        h.actor.on_message(candidate())
        await h.feed(S - 4_800 - 20_000, OPEN + 60_000)
        if terminal == "completed":
            await h.wait_for(lambda: h.render.streams)
            h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        assert (row["commit_route"], row["terminal_reason"]) == ("B", terminal)
        assert h.worker.span_requests == 1
        await h.actor.close()
    run(main())


# --- native AFE evidence (afe_metadata_v1) --------------------------------------------------

# Turn-row columns that record a decision (or what it acted on), not wall-clock timing or ids.
DECISION_COLUMNS = ("trigger", "wake_model", "wake_score", "wake_threshold", "outcome", "asr_text", "stt_raw",
                    "stt_text", "response_text", "response_type", "intent_local", "playback_reason", "audio_ms",
                    "endpoint_ms", "endpoint_class", "wake_model_sha256", "policy_hash", "wake_attribution",
                    "reference_coverage", "commit_route", "terminal_reason", "conversation_id", "reply_to",
                    "continuation")


async def wake_question_playing(h: Harness) -> None:
    """A wake question committed and answered, its response playing for 2.5 s of capture."""
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
    await h.feed(OPEN + 60_000, OPEN + 100_000)


async def wake_question_turn(h: Harness) -> dict:
    await wake_question_playing(h)
    h.render.streams[0].finish("drained")
    await h.wait_for(lambda: h.rows)
    return h.rows[0]


def _decisions(h: Harness, row: dict) -> dict:
    trace = json.loads(row["decision_trace"])
    utterance = {k: v for k, v in trace["utterance"].items() if k != "utterance_id"}
    utterance["endpoint"] = [{k: v for k, v in e.items() if k != "commit_id"} for e in utterance["endpoint"]]
    return {"row": {k: row.get(k) for k in DECISION_COLUMNS}, "utterance": utterance,
            "terminals": h.terminals(), "cues": h.cues(), "intents": h.ha.intents, "stt": h.ha.stt_calls,
            "trace": {k: trace[k] for k in ("trigger", "profile", "producing_sound", "chimed", "rule", "hops",
                                            "terminal")}}


def test_an_opted_in_wake_turn_records_native_afe_evidence_and_decides_exactly_as_without_it():
    async def main(afe: bool):
        h = Harness(afe=afe)
        row = await wake_question_turn(h)
        await h.actor.close()
        return h, row

    plain, plain_row = run(main(False))
    native, native_row = run(main(True))
    assert _decisions(native, native_row) == _decisions(plain, plain_row)

    # Without the capability the evidence is unavailable: SQL NULL, null in the trace.
    assert plain_row["afe_evidence"] is None and json.loads(plain_row["decision_trace"])["afe_evidence"] is None

    ev = json.loads(native_row["afe_evidence"])
    assert json.loads(native_row["decision_trace"])["afe_evidence"] == ev
    assert ev["support_start"] == S and ev["playback_onset"] is None      # nothing played: no rise
    pre, wake, utt = ev["pre"], ev["wake"], ev["utterance"]
    assert (pre["start"], pre["end"]) == (S - 16_000, S)
    assert (wake["start"], wake["end"]) == (S, OPEN)                      # no candidate_end: the opening hop
    assert wake["periods_expected"] == wake["periods_received"] == -(-OPEN // PERIOD) - S // PERIOD
    assert wake["frames"] == wake["frames_expected"] and wake["rms_max_db"] == 216 - 256
    assert round((utt["end"] - utt["start"]) / 16) == native_row["audio_ms"]   # the committed span
    assert utt["frames"] > 0 and utt["vad_max"] == 0.25 and utt["volume"] == 70

    # The per-period series behind the summaries: its own column, never the logged trace. It runs
    # on through the response, to the last record the device sent, on an afe-only `turn` lease
    # opened at the commit before the committed lease closed (not one period is missing), and
    # closed once the turn had ended. Without Save utterances no mic follows the commit.
    assert plain_row["afe_series"] is None and "afe_series" not in json.loads(native_row["decision_trace"])
    series = json.loads(native_row["afe_series"])
    assert series["start"] == (S - 16_000) // PERIOD * PERIOD
    assert series["start"] + len(series["frames"]) * PERIOD == native.device.afe_pos
    assert None not in series["frames"]
    assert series["support"] == [S, OPEN] and series["utterance"] == [utt["start"], utt["end"]]
    uplink = [(t, b) for t, b, _ in native.link.sent if t.startswith("uplink.")]
    [(opened, body)] = [(i, b) for i, (t, b) in enumerate(uplink) if t == "uplink.open"]
    assert (body["reason"], body["owner"], list(body["streams"])) == ("turn", native_row["turn_uuid"], ["afe"])
    assert opened < uplink.index(("uplink.close", {"lease_id": "L1", "reason": "committed"}))
    assert ("uplink.close", {"lease_id": body["lease_id"], "reason": "closed"}) in uplink
    assert plain.link.of("uplink.open") == []        # without the capability nothing follows the commit
    assert series["recording"] is None

    # The processing, in order on the same timeline: the endpoint wait in sample time, then the
    # controller's stages, the spoken answer's audio ready until audible, then playing.
    stages = series["stages"]
    assert set(stages) == {"endpoint", "stt", "intent", "tts", "reply"}
    assert round((stages["endpoint"][1] - stages["endpoint"][0]) / 16) == native_row["endpoint_ms"]
    assert stages["endpoint"][0] < utt["end"] <= stages["endpoint"][1]
    assert stages["endpoint"][1] <= stages["stt"][0] <= stages["stt"][1] <= stages["intent"][0]
    assert stages["intent"][0] <= stages["intent"][1] and stages["intent"][1] <= stages["tts"][1]
    assert stages["tts"][1] == stages["reply"][0] <= stages["reply"][1]


def test_privacy_mute_during_the_response_ends_its_lease_and_erases_what_it_carried(monkeypatch):
    saved = {}
    monkeypatch.setattr(em_session.em_recordings, "save",
                        lambda device_id, row_id, pcm, kind=em_session.em_recordings.RecordingKind.UTTERANCE:
                        saved.__setitem__(str(kind), pcm))

    async def main():
        h = Harness(config={"saveUtterances": True}, afe=True)
        await wake_question_playing(h)
        h.actor.on_message(envelope("privacy.changed", {"muted": True, "capture_epoch": None, "physical_seq": 2}))
        await h.wait_for(lambda: h.rows)
        await h.actor.close()
        return h, h.rows[0]

    h, row = run(main())
    assert row["terminal_reason"] == "muted"
    # The series and the turn recording keep only what the committed lease carried: the response's
    # records and mic went with the mute, which ended its lease on the device, so the controller
    # closed nothing.
    utt = json.loads(row["afe_evidence"])["utterance"]
    series = json.loads(row["afe_series"])
    assert series["start"] + len(series["frames"]) * PERIOD == -(-max(OPEN, utt["end"]) // PERIOD) * PERIOD
    [body] = [b for b, _ in h.link.of("uplink.open")]
    assert [b for b, _ in h.link.of("uplink.close") if b["lease_id"] == body["lease_id"]] == []
    mic = int(body["streams"]["mic"])
    a, b = series["recording"]
    assert mic <= b < mic + CELL < h.device.pos[Kind.MIC]      # the capture, up to the response lease's start
    assert len(saved["turn"]) == 2 * (b - a)


def test_a_response_lease_closes_at_its_cap_even_while_the_response_plays(monkeypatch):
    monkeypatch.setattr(em_session, "RESPONSE_LEASE_MAX_S", 0.2)

    async def main():
        h = Harness(afe=True)
        await wake_question_playing(h)
        await asyncio.sleep(0.3)
        await h.settle()
        [lease] = [b["lease_id"] for b, _ in h.link.of("uplink.open")]
        closed_while_playing = {"lease_id": lease, "reason": "closed"} in [b for b, _ in h.link.of("uplink.close")]
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.rows)
        await h.actor.close()
        return closed_while_playing, h.rows[0]

    closed_while_playing, row = run(main())
    assert closed_while_playing and row["terminal_reason"] == "completed"
    assert json.loads(row["afe_series"])["frames"]


def test_the_afe_series_places_each_saved_recording_on_its_timeline_sample_for_sample(monkeypatch):
    saved = {}

    def save_recording(device_id, row_id, pcm, kind=em_session.em_recordings.RecordingKind.UTTERANCE):
        saved[str(kind)] = pcm
        return None                         # no file name: nothing is attached to the row

    def save_wake(device_id, row_id, pcm):
        saved["wake"] = pcm
        return None

    monkeypatch.setattr(em_session.em_wakeclips, "save", save_wake)
    monkeypatch.setattr(em_session.em_recordings, "save", save_recording)

    def ramp(a, b):                         # each sample names its capture index
        return (np.arange(a, b) % 32_749).astype(np.int16)

    async def main():
        h = Harness(config={"saveWakeClips": True, "saveUtterances": True}, afe=True)
        h.device.mic_pcm = ramp
        row = await wake_question_turn(h)
        await h.actor.close()
        return h, row

    h, row = run(main())
    series = json.loads(row["afe_series"])
    # The dashboard's playhead: WAV sample k is capture sample clip[0] + k.
    assert series["wake_clip"] == [S - em_session.PREROLL, OPEN]         # support −300 ms … the opening hop
    assert series["audio_clip"] == series["utterance"]                   # the committed span, as sent to STT
    for key, kind in (("wake_clip", "wake"), ("audio_clip", "utterance")):
        a, b = series[key]
        assert len(saved[kind]) == 2 * (b - a)
    # The turn recording: the candidate lease's mic from its start, then the response lease's mic
    # from where it stopped, through the last sample the device sent — no sample lost or repeated
    # at the seam. The response lease took the mic because Save utterances is on.
    [body] = [b for b, _ in h.link.of("uplink.open")]
    assert sorted(body["streams"]) == ["afe", "mic"]
    a, b = series["recording"]
    assert (a, b) == ((S - 4_800) // CELL * CELL, h.device.pos[Kind.MIC])
    assert saved["turn"] == ramp(a, b).tobytes()


@pytest.mark.parametrize("terminal", ["no_input", "unverified_wake"])
def test_a_turn_that_never_commits_keeps_its_turn_recording_too(monkeypatch, terminal):
    saved = {}

    def save_recording(device_id, row_id, pcm, kind=em_session.em_recordings.RecordingKind.UTTERANCE):
        saved[str(kind)] = pcm
        return None

    monkeypatch.setattr(em_session.em_recordings, "save", save_recording)

    def ramp(a, b):                         # each sample names its capture index
        return (np.arange(a, b) % 32_749).astype(np.int16)

    async def main():
        h = Harness(config={"saveUtterances": True}, afe=True)
        h.device.mic_pcm = ramp
        if terminal == "no_input":          # an accepted wake that heard no command
            h.worker.tokens = TURN_647_TOKENS
            await h.start()
            h.actor.on_message(candidate())
            await h.settle()
            await h.feed(S - 4_800 - 20_000, OPEN + 90_000)
        else:                               # a refused wake
            h.worker.verification = "fail"
            await producing_sound_case(h)
        await h.wait_for(lambda: h.rows)
        await h.actor.close()
        return h, h.rows[0]

    h, row = run(main())
    assert row["terminal_reason"] == terminal and row.get("commit_id") is None
    # What the lease heard, from the candidate lease's mic start through its close, placed on the
    # chart sample for sample.
    series = json.loads(row["afe_series"])
    heard = series["utterance"] or series["support"]
    a, b = series["recording"]
    assert a == (S - 4_800) // CELL * CELL and heard[1] <= b <= h.device.pos[Kind.MIC]
    assert set(saved) == {"turn"} and saved["turn"] == ramp(a, b).tobytes()



def test_an_opted_in_candidate_lease_mirrors_the_afe_stream_from_its_reference_start():
    async def main(afe: bool):
        h = Harness(afe=afe)
        await h.start()
        h.actor.on_message(candidate())
        await h.settle()
        streams = dict(h.actor.uplink.leases.get("L1").streams)
        await h.actor.close()
        return streams

    mic = (S - 4_800) // CELL * CELL
    assert run(main(True))["afe"] == (mic - 40_000) // PERIOD * PERIOD
    assert "afe" not in run(main(False))


@pytest.mark.parametrize("afe, sends, expected", [
    (False, False, None),                                 # unavailable: the device lacks the capability
    (True, False, "no data"),                             # opted in, but nothing arrived
    (True, True, "summary"),
])
def test_a_rejected_candidate_row_carries_its_wake_evidence(afe, sends, expected):
    async def main():
        h = Harness(afe=afe)
        h.device.send_afe = sends
        h.worker.verification = "fail"
        assert await producing_sound_case(h) == ["unverified_wake"]
        await h.wait_for(lambda: h.rows)
        await h.actor.close()
        return h.rows[0]

    row = run(main())
    assert row["terminal_reason"] == "unverified_wake"
    if expected is None:
        assert row["afe_evidence"] is None and row["afe_series"] is None
        return
    ev = json.loads(row["afe_evidence"])
    assert ev["support_start"] == S and ev["utterance"] is None
    if expected == "no data":
        assert ev["pre"] is None and ev["wake"] is None and ev["playback_onset"] is None
        assert row["afe_series"] is None
    else:
        assert ev["wake"]["start"] == S and ev["wake"]["frames"] > 0 and ev["pre"]["end"] == S
        series = json.loads(row["afe_series"])
        assert series["support"][0] == S and series["utterance"] is None and 10 in series["frames"]


@pytest.mark.parametrize("afe", [False, True])
def test_the_diagnostic_lease_feeds_its_tap_the_mic_and_in_an_afe_session_the_records(afe):
    # The recording modes' charts: only a session that opted in to `afe` asks for it.
    async def main():
        h = Harness(afe=afe)
        await h.start()
        mic, records = [], []
        await h.actor.open_diagnostic(em_session.DiagnosticTap(
            lambda lease, start, pcm: mic.append((lease, start, pcm.size)),
            lambda lease, recs: records.extend((lease, r.start) for r in recs)))
        await h.settle()
        [(body, _)] = h.link.of("uplink.open")
        await h.feed(0, 4 * PERIOD)
        await h.actor.close()
        return body, mic, records

    body, mic, records = run(main())
    assert body["reason"] == "diagnostic"
    assert body["streams"] == ({"mic": "live", "afe": "live"} if afe else {"mic": "live"})
    lease = body["lease_id"]
    assert mic and {m[0] for m in mic} == {lease}
    assert records == ([(lease, k * PERIOD) for k in range(4)] if afe else [])


# --- near misses (near_miss_v1) -------------------------------------------------------------------

NM_START, NM_END = 48_000, 73_600          # support: the floor reached, no candidate opened
NM_MIC = (NM_START - 4_800) // CELL * CELL  # the lease's mic start


def near_miss(nm_id="N1", *, start=NM_START, end=NM_END, epoch=MIC_EPOCH):
    return envelope("wake.near_miss", {
        "near_miss_id": nm_id, "capture_epoch": str(epoch), "graph_sha256": "g" * 64, "profile": "idle",
        "floor": 0.17, "peak": 0.31, "support_start": str(start), "support_end": str(end),
        "mono_ns": str(MONO_BASE), "reason": "below"})


def capture_ramp(a, b):                      # each sample names its capture index
    return (np.arange(a, b) % 32_749).astype(np.int16)


def at_wall(mono_ns):
    return 1_000.0 + (mono_ns - MONO_BASE) / 1e9


def test_near_misses_with_wake_clips_on_record_their_span_through_their_own_leases_beside_the_worker():
    async def main():
        h = Harness(config={"saveWakeClips": True})
        h.device.mic_pcm = capture_ramp
        await h.start()
        audio: set[str] = set()
        real_audio = h.actor._audio

        async def spy(deliveries):
            audio.update(d.lease_id for d in deliveries)
            await real_audio(deliveries)
        h.actor._audio = spy
        # Two in flight at once: the second opens while the first still waits for its tail.
        h.actor.on_near_miss(near_miss("N1"), at_wall)
        h.actor.on_near_miss(near_miss("N2", start=NM_START + 12_800, end=NM_END + 12_800), at_wall)
        await h.settle()
        opens = [(b, g) for b, g in h.link.of("uplink.open")]
        await h.feed(NM_MIC, NM_END + 12_800 + 4_800 + BLOCK)
        await h.wait_for(lambda: len(h.near_misses) == 2)
        await h.actor.close()
        return h, opens, audio

    h, opens, audio = run(main())
    second_mic = (NM_START + 12_800 - 4_800) // CELL * CELL
    assert [(b["owner"], b["reason"], b["streams"], b["ttl_ms"], g) for b, g in opens] == [
        ("N1", "near_miss", {"mic": str(NM_MIC)}, 3_000, 1), ("N2", "near_miss", {"mic": str(second_mic)}, 3_000, 1)]
    leases = [b["lease_id"] for b, _ in opens]
    assert sorted(b["lease_id"] for b, _ in h.link.of("uplink.close")) == sorted(leases)
    assert {b["reason"] for b, _ in h.link.of("uplink.close")} == {"closed"}
    # Their mic never reached the speech pipeline.
    assert not audio & set(leases) and h.worker.leases == {}
    rows = {rec.near_miss_uuid: (rec, pcm) for rec, pcm in h.near_misses}
    rec, pcm = rows["N1"]
    assert (rec.ts, rec.profile, rec.floor, rec.peak, rec.wake_threshold, rec.wake_model_sha256,
            rec.duration_ms, rec.end_reason) == (1_000.0, "idle", 0.17, 0.31, 0.9, "g" * 64, 1_600, "below")
    assert pcm == capture_ramp(NM_MIC, NM_END + 4_800).tobytes()
    assert rows["N2"][1] == capture_ramp(second_mic, NM_END + 12_800 + 4_800).tobytes()


@pytest.mark.parametrize("case, status", [
    ("off", "off"), ("muted", "not_permitted"), ("diagnostic", "not_permitted"), ("old_epoch", "failed")])
def test_a_near_miss_that_cannot_be_recorded_is_persisted_with_why_and_opens_no_lease(case, status):
    async def main():
        h = Harness(config={"saveWakeClips": case != "off"})
        await h.start()
        if case == "muted":
            h.actor.on_message(envelope("privacy.changed", {"muted": True, "capture_epoch": None,
                                                            "physical_seq": 2}))
        elif case == "diagnostic":
            await h.actor.open_diagnostic(em_session.DiagnosticTap(lambda *a: None, lambda *a: None))
        await h.settle()
        h.actor.on_near_miss(near_miss(epoch=MIC_EPOCH + 1 if case == "old_epoch" else MIC_EPOCH), at_wall)
        await h.wait_for(lambda: h.near_misses)
        await h.actor.close()
        return h

    h = run(main())
    [(rec, pcm)] = h.near_misses
    assert (rec.near_miss_uuid, rec.recording, pcm) == ("N1", status, None)
    assert [b for b, _ in h.link.of("uplink.open") if b["reason"] == "near_miss"] == []


@pytest.mark.parametrize("held, keeps", [(NM_START + 6_400, False), (NM_END, True)])
def test_a_near_miss_lease_the_device_ends_early_keeps_its_audio_only_if_it_covers_the_support(held, keeps):
    async def main():
        h = Harness(config={"saveWakeClips": True})
        h.device.mic_pcm = capture_ramp
        await h.start()
        h.actor.on_near_miss(near_miss(), at_wall)
        await h.settle()
        [(body, _)] = h.link.of("uplink.open")
        await h.feed(NM_MIC, held)
        through = h.device.pos[Kind.MIC]
        h.actor.on_message(envelope("uplink.ended", {"lease_id": body["lease_id"], "reason": "overrun",
                                                     "last_sample": {"mic": str(through)}, "clipped_start": {}}))
        await h.wait_for(lambda: h.near_misses)
        await h.actor.close()
        return h, through

    h, through = run(main())
    [(rec, pcm)] = h.near_misses
    assert rec.recording == "failed"            # what the row keeps unless `pcm` is saved
    assert pcm == (capture_ramp(NM_MIC, through).tobytes() if keeps else None)
    assert h.link.of("uplink.close") == []


@pytest.mark.parametrize("case", ["refused", "session_lost"])
def test_a_refused_or_lost_near_miss_lease_still_writes_its_row_as_failed(case):
    async def main():
        h = Harness(config={"saveWakeClips": True})
        await h.start()
        h.actor.on_near_miss(near_miss(), at_wall)
        await h.settle()
        if case == "refused":                   # the device opened no lease: no audio comes
            [capture] = h.actor._near_misses.values()
            h.actor.on_message(envelope("command.ack", {"message_id": capture.open_message,
                                                        "status": "rejected", "error": "muted"}))
        else:                                   # what it carried went with the session
            await h.feed(NM_MIC, NM_END)
            h.actor.detach("closed")
        await h.wait_for(lambda: h.near_misses)
        await h.actor.close()
        return h

    h = run(main())
    [(rec, pcm)] = h.near_misses
    assert (rec.recording, pcm) == ("failed", None)
    assert h.link.of("uplink.close") == []


def test_a_malformed_near_miss_is_dropped_without_a_row():
    async def main():
        h = Harness(config={"saveWakeClips": True})
        await h.start()
        bad = near_miss()
        h.actor.on_near_miss(envelope("wake.near_miss", {**bad.body, "profile": "loud"}), at_wall)
        h.actor.on_near_miss(envelope("wake.near_miss", {**bad.body, "support_end": str(NM_START - 1)}), at_wall)
        await h.settle()
        await h.actor.close()
        return h

    h = run(main())
    assert h.near_misses == [] and h.link.of("uplink.open") == []


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


@pytest.mark.parametrize("afe", [False, True])
def test_a_button_turn_lease_requests_afe_from_its_reference_start_only_when_opted_in(afe):
    async def main():
        h = Harness(afe=afe)
        await h.start()
        h.actor.button_turn(button())
        await h.settle()
        streams = h.link.of("uplink.open")[0][0]["streams"]
        await h.actor.close()
        return streams

    streams = run(main())
    if afe:
        assert streams["afe"] == str(int(streams["reference"]) // PERIOD * PERIOD)
    else:
        assert "afe" not in streams


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


async def later_button_turn(h: Harness, press: int, words: str, stt_text: str) -> None:
    """Another button turn on the running harness, spoken like QUESTION from `press` on."""
    span = (press + 1_536, press + 9_728)
    h.worker.vad = speech(span)
    h.device.level = levels(span)
    h.worker.tokens = [(f"▁{w}", press + 2_500 + 1_500 * i) for i, w in enumerate(words.split())]
    h.ha.stt_text = stt_text
    h.actor.button_turn(button(press))
    await h.settle()
    await h.feed(h.device.pos[Kind.MIC], press + 50_000)


async def answered(h: Harness, speech_: str, url: str) -> None:
    """A button turn HA answers with `speech_` at `url`, played to the end."""
    h.ha.next_run = FakeRun([IntentEnded(speech_, "c", False, "action_done", True),
                             TtsReady(url, False), RunEnded()])
    await button_turn(h, PRESS + 50_000)
    await h.wait_for(lambda: h.render.streams)
    h.render.streams[0].finish("drained")
    await h.wait_for(lambda: h.rows)


def test_say_that_again_replays_the_last_response_without_home_assistant_for_two_minutes():
    async def main():
        h = Harness()
        await answered(h, "It is 3 PM.", "http://ha/tts/1")
        await later_button_turn(h, PRESS + 100_000, "SAY THAT AGAIN", "Say that again.")
        await h.wait_for(lambda: len(h.render.streams) == 2)
        assert h.render.urls() == ["http://ha/tts/1", "http://ha/tts/1"]
        h.render.streams[1].finish("drained")
        await h.wait_for(lambda: len(h.rows) == 2)
        assert h.rows[1]["outcome"] == "repeat" and h.rows[1]["response_text"] == "It is 3 PM."
        assert h.ha.intents == [("What time is it?", None)] and h.ha.tts == []

        # The replay restarted the window; once it has run out there is nothing to repeat.
        h.actor._last_spoken.ended_at -= em_session.REPEAT_WINDOW_S + 1
        await later_button_turn(h, PRESS + 200_000, "SAY THAT AGAIN", "Say that again.")
        await h.wait_for(lambda: len(h.render.streams) == 3)
        assert h.render.urls()[2] == f"tts://{em_session.LINE_NOTHING_TO_REPEAT}"
        h.render.streams[2].finish("drained")
        await h.wait_for(lambda: len(h.rows) == 3)
        assert h.rows[2]["outcome"] == "repeat" and h.ha.intents == [("What time is it?", None)]
        await h.actor.close()
    run(main())


def test_say_that_again_skips_announcements_and_says_so_when_home_assistant_lost_the_audio():
    async def main():
        h = Harness()
        await answered(h, "It is 3 PM.", "http://ha/tts/1")
        announce = asyncio.create_task(h.actor.announce("http://ha/announce", preannounce=False,
                                                        preannounce_url=None, start_conversation=False))
        await h.wait_for(lambda: len(h.render.streams) == 2)
        h.render.streams[1].finish("drained")
        await announce
        h.render.autostart = False
        await later_button_turn(h, PRESS + 100_000, "REPEAT THAT", "Repeat that.")
        await h.wait_for(lambda: len(h.render.streams) == 3)
        assert h.render.urls()[2] == "http://ha/tts/1"
        h.render.autostart = True
        h.render.streams[2].finish("failed")                 # HA restarted: its TTS URL is gone
        await h.wait_for(lambda: len(h.render.streams) == 4)
        assert h.render.urls()[3] == f"tts://{em_session.LINE_NOTHING_TO_REPEAT}"
        h.render.streams[3].finish("drained")
        await h.wait_for(lambda: len(h.rows) == 2)
        assert h.rows[1]["outcome"] == "repeat" and h.rows[1]["response_text"] == em_session.LINE_NOTHING_TO_REPEAT
        await h.actor.close()
    run(main())


def test_say_that_again_in_a_reply_window_replays_the_question_and_reopens_it_in_its_conversation():
    async def main():
        h = Harness()
        drain = await question_drained(h)                     # HA asked "Which TV?" in conv-1
        again = (drain + 16_000, drain + 24_192)
        h.worker.vad = speech(again)
        h.device.level = levels(again)
        h.worker.tokens = [("▁SAY", again[0] + 1_000), ("▁THAT", again[0] + 3_000), ("▁AGAIN", again[0] + 6_000)]
        h.ha.stt_text = "Say that again."
        await h.feed(drain, drain + 60_000)
        await h.wait_for(lambda: len(h.render.streams) == 2)
        assert h.render.urls() == ["http://ha/tts/1", "http://ha/tts/1"] and len(h.ha.intents) == 1
        h.render.streams[1].finish("drained")
        await h.wait_for(lambda: h.actor.state == "ARMED")
        reopened = h.actor._turn.utterance.spec.trigger
        answer = (reopened + 16_000, reopened + 25_728)
        h.worker.vad = speech(answer)
        h.device.level = levels(answer)
        h.worker.tokens = [("▁LIVING", answer[0] + 3_000), ("▁ROOM", answer[0] + 6_000)]
        h.ha.stt_text = "The living room."
        h.ha.next_run = FakeRun([IntentEnded("Turned off.", "conv-1", False, "action_done", False),
                                 TtsReady("http://ha/tts/2", False), RunEnded()])
        await h.feed(h.device.pos[Kind.MIC], reopened + 80_000)
        await h.wait_for(lambda: len(h.render.streams) == 3)
        assert h.ha.intents[1] == ("The living room.", "conv-1")
        h.render.streams[2].finish("drained")
        await h.wait_for(lambda: len(h.rows) == 3)
        question, repeat, reply = h.rows
        assert repeat["outcome"] == "repeat" and repeat["response_text"] == "Which TV?"
        assert repeat["conversation_id"] == "conv-1" and reply["reply_to"] == repeat["turn_uuid"]
        assert question["continuation"] == "answered" and repeat["continuation"] == "answered"
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
        await h.wait_for(lambda: h.actor.state == "ARMED")
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


@pytest.mark.parametrize("wake_sound", [True, False])
def test_ha_turn_with_continue_conversation_opens_a_reply_turn_that_ends_silently_without_input(wake_sound):
    async def main():
        h = Harness(config={"wakeSound": wake_sound})
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
        # Listening (the ring and HA's Voice state) from the drain on.
        await h.wait_for(lambda: h.actor.state == "ARMED")
        assert len(reply_focus(h, reply_open[0]["owner"])) == 1
        base = PRESS + 50_000
        await h.feed(base, base + 4_000)            # no audio reached the reply lease before the drain
        # The wake chime cues the window, which opens once it has finished, on the lease's first
        # audio: no wake word needed.
        assert h.render.local == ([("earcon", "builtin:wake_chime")] if wake_sound else [])
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        assert row["terminal_reason"] == "completed" and row["commit_route"] == "A"
        assert row["continuation"] == "pending" and row["playback_reason"] == "drained"
        assert row["conversation_id"] == "conv-1" and row["reply_to"] is None
        await h.feed(base + 4_000, base + 4_000 + 7 * 16_000 + 4_000)
        await h.wait_for(lambda: h.actor.state == "IDLE")
        assert h.terminals()[-1] == "no_input" and h.cues() == []    # a silent window has no cue
        closes = [b for b, _ in h.link.of("uplink.close") if b["lease_id"] == reply_open[0]["lease_id"]]
        assert closes == [{"lease_id": reply_open[0]["lease_id"], "reason": "closed"}]
        assert len(h.ha.intents) == 1 and len(h.ha.stt_calls) == 1  # a silent window never dispatches
        await h.wait_for(lambda: len(h.rows) == 2 and h.rows[0]["continuation"] == "reply_timeout")
        reply = h.rows[1]
        assert reply["trigger"] == "reply" and reply["terminal_reason"] == "no_input"
        assert reply["reply_to"] == row["turn_uuid"] and reply["turn_uuid"] == reply_open[0]["owner"]
        assert json.loads(reply["decision_trace"])["utterance"]["endpoint"][-1]["reason"] == "no_input"
        await h.actor.close()
    run(main())


async def question_drained(h: Harness) -> int:
    """Button turn → HA asks "Which TV?" (conv-1); its quiet tail plays, it drains, the wake chime
    plays (and ends at once), and the reply turn opens. Returns the reply's trigger (the mic
    frontier then, which is still the drain's)."""
    h.ha.next_run = FakeRun([
        IntentEnded("Which TV?", "conv-1", True, "action_done", False),
        TtsReady("http://ha/tts/1", False), RunEnded()])
    await button_turn(h, PRESS + 50_000)
    await h.wait_for(lambda: h.render.streams)
    base = PRESS + 50_000
    await h.feed(base, base + 24_000)                    # quiet question tail
    h.render.streams[0].finish("drained")
    await h.wait_for(lambda: h.actor._turn is not None and h.actor._turn.trigger == "reply")
    return h.actor._turn.utterance.spec.trigger


def skip_mic(h: Harness, samples: int) -> int:
    """A capture gap: the next mic (and reference) packet starts `samples` later. Returns where
    the gap starts."""
    start = h.device.pos[Kind.MIC]
    h.device.pos[Kind.MIC] = start + samples
    h.device.pos[Kind.REFERENCE] += samples
    return start


def test_a_gap_before_the_answer_does_not_end_the_reply_window_and_the_answer_after_it_commits():
    async def main():
        h = Harness()
        drain = await question_drained(h)
        await h.feed(drain, drain + 16_000)                   # a quiet second of the window
        gap = skip_mic(h, 2_560)                              # 160 ms of capture lost
        answer = (drain + 32_256, drain + 41_984)
        h.worker.vad = speech(answer)
        h.device.level = levels(answer)
        h.worker.tokens = [("▁LIVING", answer[0] + 3_000), ("▁ROOM", answer[0] + 6_000)]
        h.ha.stt_text = "The living room."
        h.ha.next_run = FakeRun([IntentEnded("Turned off.", "conv-1", False, "action_done", False),
                                 TtsReady("http://ha/tts/2", False), RunEnded()])
        await h.feed(gap, drain + 100_000)
        await h.wait_for(lambda: len(h.render.streams) == 2)
        assert h.ha.intents[1] == ("The living room.", "conv-1")
        assert "interrupted" not in h.terminals() and h.cues() == []
        h.render.streams[1].finish("drained")
        await h.wait_for(lambda: len(h.rows) == 2)
        question, reply = h.rows
        assert question["continuation"] == "answered" and reply["terminal_reason"] == "completed"
        trace = json.loads(reply["decision_trace"])
        assert len(trace["reopened"]) == 1                    # the utterance before the gap
        assert trace["utterance"]["start"] >= gap + 2_560 and trace["utterance"]["trigger"] >= gap + 2_560
        await h.actor.close()
    run(main())


def test_a_silent_reply_window_with_a_gap_still_closes_no_input_at_its_original_end_without_a_cue():
    async def main():
        h = Harness()
        drain = await question_drained(h)
        await h.feed(drain, drain + 32_000)
        gap = skip_mic(h, 2_560)
        await h.feed(gap, drain + 7 * 16_000 + 8_000)
        await h.wait_for(lambda: h.actor.state == "IDLE")
        assert h.terminals()[-1] == "no_input" and "interrupted" not in h.terminals()
        assert h.cues() == [] and len(h.ha.intents) == 1
        await h.wait_for(lambda: len(h.rows) == 2 and h.rows[0]["continuation"] == "reply_timeout")
        trace = json.loads(h.rows[1]["decision_trace"])
        close = trace["utterance"]["endpoint"][-1]
        assert close["reason"] == "no_input" and len(trace["reopened"]) == 1
        # The window's own end (the drain + 7 s), not 7 s after the gap.
        assert drain + 7 * 16_000 <= close["at"] < drain + 7 * 16_000 + BLOCK
        await h.actor.close()
    run(main())


def test_a_reply_turn_whose_mic_stops_closes_no_input_by_the_wall_clock(monkeypatch):
    monkeypatch.setattr(em_session, "REPLY_WALL_S", 0.5)

    async def main():
        h = Harness()
        drain = await question_drained(h)
        await h.feed(drain, drain + 8_000)                    # half a second of the window, then nothing
        assert h.actor._turn is not None and h.actor._turn.trigger == "reply"
        await h.wait_for(lambda: h.actor.state == "IDLE", timeout=3)
        assert h.terminals()[-1] == "no_input" and h.cues() == [] and len(h.ha.intents) == 1
        await h.wait_for(lambda: len(h.rows) == 2 and h.rows[0]["continuation"] == "reply_timeout")
        assert h.rows[1]["trigger"] == "reply" and h.rows[1]["terminal_reason"] == "no_input"
        await h.actor.close()
    run(main())


def test_a_reply_window_that_never_gets_mic_closes_no_input_by_the_wall_clock(monkeypatch):
    monkeypatch.setattr(em_session, "REPLY_WALL_S", 0.5)

    async def main():
        h = Harness()
        await h.start()
        announce = asyncio.create_task(h.actor.announce("http://ha/announce", preannounce=False,
                                                        preannounce_url=None, start_conversation=True))
        await h.wait_for(lambda: h.render.streams)
        reply_open = [b for b, _ in h.link.of("uplink.open") if b["reason"] == "reply"]
        h.render.streams[0].finish("drained")                 # no mic ever reached the reply lease
        await asyncio.wait_for(announce, 2)
        assert h.actor.state == "ARMED" and h.actor._turn is None
        await h.wait_for(lambda: h.actor.state == "IDLE", timeout=3)
        assert h.terminals()[-1] == "no_input" and h.cues() == [] and not h.actor.turn_active
        closes = [b for b, _ in h.link.of("uplink.close") if b["lease_id"] == reply_open[0]["lease_id"]]
        assert closes == [{"lease_id": reply_open[0]["lease_id"], "reason": "closed"}]
        await h.actor.close()
    run(main())


def test_a_wake_during_a_silent_reply_window_takes_the_question_over_in_its_conversation():
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
        await h.feed(base + 24_000, base + 32_000)           # the reply turn listens
        assert h.actor.state == "ARMED" and h.actor._turn.trigger == "reply"
        start = base + 40_000                                 # "Ophelia, what time is it?"
        open_ = start + 12_800
        question = (open_ + 3_200, open_ + 11_392)
        h.worker.vad = speech((start, open_ - 512), question)
        h.device.level = levels((start, open_ - 512), question)
        h.worker.tokens = [("▁OPHELIA", open_ - 1_800), ("▁WHAT", open_ + 4_000), ("▁TIME", open_ + 6_000),
                           ("▁IS", open_ + 8_000), ("▁IT", open_ + 10_000)]
        h.ha.stt_text = "Ophelia, what time is it?"
        h.ha.next_run = FakeRun([IntentEnded("It is noon.", "conv-1", False, "query_answer", True),
                                 TtsReady("http://ha/tts/2", False), RunEnded()])
        h.actor.on_message(candidate(start=start))
        await h.settle()
        await h.feed(base + 32_000, open_ + 60_000)
        await h.wait_for(lambda: len(h.render.streams) == 2)
        assert h.ha.intents[1] == ("what time is it?", "conv-1")
        h.render.streams[1].finish("drained")
        await h.wait_for(lambda: len(h.rows) == 3 and h.rows[0]["continuation"] == "wake")
        question_row, reply, wake = h.rows
        assert reply["trigger"] == "reply" and reply["terminal_reason"] == "superseded"
        assert wake["trigger"] == "wake" and wake["conversation_id"] == "conv-1"
        assert wake["reply_to"] == question_row["turn_uuid"]
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
        await h.wait_for(lambda: h.actor.state == "ARMED")
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
        # The answer begins just after the window opens: its run opens only after the 320 ms guard,
        # but starts at its first cell (156,160), and HA hears it from 300 ms before that, not
        # the wait from the utterance start (the trigger − 780 ms): 151,360 … boundary + 192 ms.
        assert reply["audio_ms"] == (165_888 + 3_072 - (156_160 - 4_800)) // 16
        utterance = json.loads(reply["decision_trace"])["utterance"]
        assert utterance["guard_until"] == utterance["trigger"] + 5_120 > 156_160
        assert utterance["answer_start"] == 156_160
        await h.actor.close()
    run(main())


def test_an_answer_already_underway_when_the_question_drains_gets_no_chime_over_it():
    async def main():
        h = Harness()
        h.ha.next_run = FakeRun([
            IntentEnded("Which TV?", "conv-1", True, "action_done", False),
            TtsReady("http://ha/tts/1", False), RunEnded()])
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        base = PRESS + 50_000
        answer = (base + 26_112, base + 35_840)
        h.worker.vad = speech(answer)
        h.device.level = levels(answer)
        h.worker.tokens = [("▁LIVING", base + 29_000), ("▁ROOM", base + 32_000)]
        h.ha.stt_text = "The living room."
        h.ha.next_run = FakeRun([IntentEnded("Turned off.", "conv-1", False, "action_done", False),
                                 TtsReady("http://ha/tts/2", False), RunEnded()])
        # The answer starts over the question's last 368 ms: too short for an early answer,
        # but its onset is complete in the retained audio by the drain.
        await h.feed(base, base + 32_000)
        assert h.actor.state == "SPEAKING"
        drain = h.device.pos[Kind.MIC]
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: reply_open(h))
        # It opens at once at the drain, unchimed, its VAD carried on undisturbed, unguarded.
        assert h.actor._turn.utterance.spec.trigger == drain and h.actor._turn.utterance.spec.guard_until is None
        assert h.render.local == [] and h.worker.primes == []
        await h.feed(base + 32_000, base + 80_000)
        await h.wait_for(lambda: len(h.render.streams) == 2)
        assert h.ha.intents[1] == ("The living room.", "conv-1")
        h.render.streams[1].finish("drained")
        await h.wait_for(lambda: len(h.rows) == 2)
        trace = json.loads(h.rows[1]["decision_trace"])
        assert trace["reply_window"] == {"drain": drain, "chime": None, "vad": "carried", "voice_from": None,
                                         "voice_at": None, "primed_at": None}
        await h.actor.close()
    run(main())


async def question_asked(h: Harness, *, voice: bool = True, held_chime: bool = False) -> int:
    """Button turn ("what time is it", whose pause finalized after it unless `voice` is False:
    its voice snapshot) → HA asks "Which TV?" (conv-1); its quiet tail plays and drains.
    `held_chime`: the wake chime plays on until the test finishes it. Returns the drain (the
    mic frontier then)."""
    if voice:
        h.worker.pauses = [QUESTION[1] + 5_120]
    h.ha.next_run = FakeRun([
        IntentEnded("Which TV?", "conv-1", True, "action_done", False),
        TtsReady("http://ha/tts/1", False), RunEnded()])
    await button_turn(h, PRESS + 50_000)
    await h.wait_for(lambda: h.render.streams)
    base = PRESS + 50_000
    await h.feed(base, base + 24_000)                    # quiet question tail
    if held_chime:
        h.render.local_finish = None
    h.render.streams[0].finish("drained")
    await h.wait_for(lambda: h.actor.state == "ARMED")
    return h.device.pos[Kind.MIC]


def reply_open(h: Harness) -> bool:
    return h.actor._turn is not None and h.actor._turn.trigger == "reply"


def test_the_reply_window_opens_once_the_chime_has_finished_with_the_askers_voice_restored():
    async def main():
        h = Harness()
        drain = await question_asked(h, held_chime=True)
        assert h.render.local == [("earcon", "builtin:wake_chime")]
        await h.feed(drain, drain + 25_600)                    # the chime plays for 1.6 s
        assert h.actor._turn is None and h.worker.primes == []   # no window yet; the VAD carries on
        h.render.local_playbacks[0].finish("drained")
        await h.wait_for(lambda: reply_open(h))
        turn = h.actor._turn
        trigger = turn.utterance.spec.trigger
        assert trigger == drain + 25_600                       # the mic frontier as the chime ended
        await h.wait_for(lambda: h.rows)
        question = h.rows[0]
        voice_at = json.loads(question["decision_trace"])["voice_at"]
        assert voice_at >= QUESTION[1] + 5_120
        # The question turn's snapshot, restored into the reply lease from the trigger on.
        [(lease, at, voice)] = h.worker.primes
        assert (lease, at, voice.sample) == (turn.runtime.lease_id, trigger, voice_at)
        await h.feed(trigger, trigger + 7 * 16_000 + 4_000)
        await h.wait_for(lambda: len(h.rows) == 2)
        trace = json.loads(h.rows[1]["decision_trace"])
        assert trace["reply_window"] == {"drain": drain, "chime": "drained", "vad": "voice",
                                         "voice_from": question["turn_uuid"], "voice_at": voice_at,
                                         "primed_at": trigger}
        assert trace["utterance"]["guard_until"] == trigger + 5_120
        # Its 7 s count from the chime's end, not the question's.
        close = trace["utterance"]["endpoint"][-1]
        assert close["reason"] == "no_input" and trigger + 7 * 16_000 <= close["at"] < trigger + 7 * 16_000 + BLOCK
        await h.actor.close()
    run(main())


@pytest.mark.parametrize("wake_sound, voice", [(False, True), (True, False), (False, False)])
def test_a_reply_windows_vad_starts_from_the_askers_voice_else_fresh_from_the_drain(wake_sound, voice):
    async def main():
        h = Harness(config={"wakeSound": wake_sound})
        drain = await question_asked(h, voice=voice, held_chime=wake_sound)
        if wake_sound:
            # No snapshot: a fresh state from the drain, set before the chime so it warms over it.
            lease = h.actor._expectation.runtime.lease_id
            assert not reply_open(h) and h.worker.primes == [(lease, drain, None)]
            h.render.local_playbacks[0].finish("drained")
        await h.wait_for(lambda: reply_open(h))
        turn = h.actor._turn
        assert h.render.local == ([("earcon", "builtin:wake_chime")] if wake_sound else [])
        assert turn.utterance.spec.trigger == drain            # no mic arrived in between
        [(lease, at, snapshot)] = h.worker.primes
        assert lease == turn.runtime.lease_id and at == drain and (snapshot is not None) is voice
        window = turn.expectation.window
        assert window["vad"] == ("voice" if voice else "fresh") and window["primed_at"] == drain
        assert window["chime"] == ("drained" if wake_sound else None)
        assert turn.utterance.spec.guard_until == drain + 5_120
        await h.actor.close()
    run(main())


def test_an_early_answer_during_the_chime_cancels_it_and_opens_the_reply_at_its_onset():
    async def main():
        h = Harness()
        drain = await question_asked(h, held_chime=True)
        onset = -(-(drain + 3_072) // CELL) * CELL            # 200 ms into the chime
        answer = (onset, onset + 9_728)
        h.worker.vad = speech(answer)
        h.device.level = levels(answer)
        h.worker.tokens = [("▁LIVING", onset + 3_000), ("▁ROOM", onset + 6_000)]
        h.ha.stt_text = "The living room."
        h.ha.next_run = FakeRun([IntentEnded("Turned off.", "conv-1", False, "action_done", False),
                                 TtsReady("http://ha/tts/2", False), RunEnded()])
        await h.feed(drain, drain + 60_000)
        await h.wait_for(lambda: len(h.render.streams) == 2)
        assert h.ha.intents[1] == ("The living room.", "conv-1")
        assert h.render.local_playbacks[0].finished.result()["reason"] == "cancelled"
        assert h.worker.primes == []                          # the answer is heard on the carried state
        h.render.streams[1].finish("drained")
        await h.wait_for(lambda: len(h.rows) == 2)
        trace = json.loads(h.rows[1]["decision_trace"])
        assert trace["utterance"]["trigger"] == onset and trace["utterance"]["guard_until"] is None
        assert trace["reply_window"]["chime"] == "cancelled" and trace["reply_window"]["vad"] == "carried"
        await h.actor.close()
    run(main())


def test_a_committed_answers_voice_primes_the_window_of_the_question_it_leads_to():
    async def main():
        h = Harness()
        drain = await question_asked(h)
        await h.wait_for(lambda: reply_open(h))
        first = h.actor._turn
        answer = (drain + 16_384, drain + 26_112)
        h.worker.vad = speech(answer)
        h.device.level = levels(answer)
        h.worker.tokens = [("▁LIVING", answer[0] + 3_000), ("▁ROOM", answer[0] + 6_000)]
        h.worker.pauses = [answer[1] + 5_120]
        h.ha.stt_text = "The living room."
        h.ha.next_run = FakeRun([IntentEnded("Which lamp?", "conv-1", True, "action_done", False),
                                 TtsReady("http://ha/tts/2", False), RunEnded()])
        await h.feed(drain, drain + 60_000)
        await h.wait_for(lambda: len(h.render.streams) == 2)
        assert first.commit is not None and first.voice is not None and first.voice.sample >= answer[1] + 5_120
        pos = h.device.pos[Kind.MIC]
        await h.feed(pos, pos + 16_000)                       # "Which lamp?" plays
        h.render.streams[1].finish("drained")
        await h.wait_for(lambda: reply_open(h) and h.actor._turn is not first)
        second = h.actor._turn
        assert h.worker.primes[0][2] is not first.voice        # the first window had the button turn's
        assert h.worker.primes[-1][1:] == (second.utterance.spec.trigger, first.voice)
        window = second.expectation.window
        assert window["vad"] == "voice" and window["voice_from"] == first.turn_id
        await h.actor.close()
    run(main())


def test_a_reply_window_never_takes_its_voice_from_a_reply_that_did_not_commit():
    voice = VoiceSnapshot(np.zeros((2, 1, 128), np.float32), np.zeros(64, np.float32), 1_000)

    def asked_by(kind, committed):
        origin = em_session._Turn("t", 0, kind, 0.0, Utterance(UtteranceSpec("u", kind, 0, 0)),
                                  em_session._Runtime("L", CellAssembler()), voice=voice)
        origin.commit = object() if committed else None
        return em_session._Expectation("e", "o", 0, UtteranceKind.REPLY, None, "t", 0.0, 1, origin=origin)

    assert SessionActor._voice(asked_by(UtteranceKind.REPLY, committed=False)) is None
    assert SessionActor._voice(asked_by(UtteranceKind.HA_REPLY, committed=False)) is None
    assert SessionActor._voice(asked_by(UtteranceKind.REPLY, committed=True)) is voice
    assert SessionActor._voice(asked_by(UtteranceKind.WAKE, committed=False)) is voice   # any terminal
    assert SessionActor._voice(asked_by(UtteranceKind.BUTTON, committed=True)) is voice
    assert SessionActor._voice(em_session._Expectation("e", "o", 0, UtteranceKind.HA_REPLY, None, None,
                                                       0.0, 1)) is None              # HA asked on its own


def test_an_answer_whose_speech_score_never_reaches_085_is_taken():
    async def main():
        h = Harness()
        h.ha.next_run = FakeRun([
            IntentEnded("Which TV?", "conv-1", True, "action_done", False),
            TtsReady("http://ha/tts/1", False), RunEnded()])
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        base = PRESS + 50_000
        await h.feed(base, base + 24_000)
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.actor.state == "ARMED")
        answer = (base + 26_112, base + 35_840)
        # Turn 532's answer peaked at 0.80: the reducer's run opens at 0.70.
        h.worker.vad = lambda s: 0.8 if answer[0] <= s < answer[1] else 0.05
        h.device.level = levels(answer)
        h.worker.tokens = [("▁LIVING", base + 29_000), ("▁ROOM", base + 32_000)]
        h.ha.stt_text = "The living room."
        h.ha.next_run = FakeRun([IntentEnded("Turned off.", "conv-1", False, "action_done", False),
                                 TtsReady("http://ha/tts/2", False), RunEnded()])
        await h.feed(base + 24_000, base + 80_000)
        await h.wait_for(lambda: len(h.render.streams) == 2)
        assert h.ha.intents[1] == ("The living room.", "conv-1")
        await h.actor.close()
    run(main())


def test_speech_under_way_long_before_the_drain_is_not_the_answer_but_the_next_run_is():
    async def main():
        h = Harness()
        h.ha.next_run = FakeRun([
            IntentEnded("Which TV?", "conv-1", True, "action_done", False),
            TtsReady("http://ha/tts/1", False), RunEnded()])
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        base = PRESS + 50_000
        drain = base + 24_000
        # Someone talks from 601 ms before the drain to 742 ms after it (at 0.75: no early
        # answer), pauses 608 ms, then answers.
        talk, answer = (144_384, 165_888), (175_616, 186_368)
        h.worker.vad = lambda s: (0.75 if talk[0] <= s < talk[1] else 0.95 if answer[0] <= s < answer[1]
                                  else 0.05)
        h.device.level = levels(talk, answer)
        h.worker.tokens = [("▁THE", 177_000), ("▁LIVING", 180_000), ("▁ROOM", 184_000)]
        h.ha.stt_text = "The living room."
        h.ha.next_run = FakeRun([IntentEnded("Turned off.", "conv-1", False, "action_done", False),
                                 TtsReady("http://ha/tts/2", False), RunEnded()])
        await h.feed(base, drain)
        assert h.actor.state == "SPEAKING"
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.actor.state == "ARMED")
        await h.feed(drain, base + 120_000)
        await h.wait_for(lambda: len(h.render.streams) == 2)
        assert h.ha.intents[1] == ("The living room.", "conv-1")
        assert h.render.local == [("earcon", "builtin:wake_chime")]   # talk under way is no answer
        h.render.streams[1].finish("drained")
        await h.wait_for(lambda: len(h.rows) == 2)
        reply = h.rows[1]
        assert reply["audio_ms"] == (answer[1] + 3_072 - (answer[0] - 4_800)) // 16
        utterance = json.loads(reply["decision_trace"])["utterance"]
        trigger = utterance["trigger"]                      # the drain: the mic frontier then
        assert drain <= trigger < drain + 1_280
        # From the cell holding the trigger until the talk pauses: not the answer.
        assert ["background_speech", "under_way", trigger - trigger % 512, talk[1]] in utterance["segments"]
        await h.actor.close()
    run(main())


def test_a_streamed_answer_that_ends_in_a_question_watches_for_a_reply_while_it_plays():
    async def main():
        h = Harness()
        run_ = FakeRun([TtsReady("http://ha/tts/s", True)])   # intent-progress released the stream
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
        await h.wait_for(lambda: h.actor.state == "ARMED")
        await h.actor.close()
    run(main())


def test_a_streamed_reply_whose_audio_never_starts_times_out_response_start_s_after_intent_end(monkeypatch):
    monkeypatch.setattr(em_session, "RESPONSE_START_S", 0.3)

    async def main():
        h = Harness()
        h.render.autostart = False
        run_ = FakeRun([TtsReady("http://ha/tts/s", True)])
        h.ha.next_run = run_
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        await asyncio.sleep(0.5)            # before intent-end only HA's own limit bounds the start
        assert not h.render.streams[0].done and not h.terminals()
        loop = asyncio.get_running_loop()
        run_.queue.put_nowait(IntentEnded("Once upon a time.", "conv-1", False, "action_done", False))
        run_.queue.put_nowait(RunEnded())
        intent_end = loop.time()
        await h.wait_for(lambda: h.terminals(), timeout=3)
        assert h.terminals()[0] == "response_timeout"
        assert 0.25 <= loop.time() - intent_end < 1.5
        assert h.render.streams[0].done
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        assert row["playback_reason"] == "response_timeout" and row["first_audio_ms"] is None
        assert row["response_latency_ms"] is None          # nothing played: unmeasured, never zero
        await h.actor.close()
    run(main())


@pytest.mark.parametrize("audible", [True, False])
def test_stop_is_a_local_dialog_stop_only_while_the_reply_is_audible(audible):
    async def main():
        h = Harness()
        h.render.autostart = audible
        stop = (OPEN + 3_200, OPEN + 7_296)
        h.worker.vad = speech((S, OPEN - 512), stop)
        h.device.level = levels((S, OPEN - 512), stop)
        h.worker.tokens = [("▁OPHELIA", OPEN - 1_800), ("▁STOP", OPEN + 5_200)]
        h.ha.stt_text = "Ophelia, stop."
        h.ha.next_run = FakeRun([IntentEnded("", None, False, "action_done", True), RunEnded()])
        await h.start()
        announce = asyncio.create_task(h.actor.announce("http://ha/announce", preannounce=False,
                                                        preannounce_url=None, start_conversation=False))
        await h.wait_for(lambda: h.render.streams)
        h.actor.on_message(candidate())
        await h.settle()
        # A reply the user cannot hear yet is no command context: an ordinary wake, chime included.
        assert h.render.local == ([] if audible else [("earcon", "builtin:wake_chime")])
        await h.feed(S - 4_800 - 20_000, OPEN + 40_000)
        await h.wait_for(lambda: h.rows)
        row = h.rows[0]
        if audible:
            assert row["outcome"] == "local_command" and h.ha.stt_calls == [] and h.ha.intents == []
        else:
            assert row["outcome"] == "ha" and len(h.ha.intents) == 1
        announce.cancel()
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
        announce = asyncio.create_task(h.actor.announce("http://ha/announce", preannounce=False,
                                                        preannounce_url=None, start_conversation=True))
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


# A media-player announcement (tts.speak) always asks for a preannounce; an assist_satellite
# one asks with HA's sound, or not at all. The Dot's chime takes the place of HA's sound.
@pytest.mark.parametrize("capable, preannounce, url, played", [
    (True, True, None, ["builtin:announcement_chime", "http://ha/announce"]),
    (True, True, "http://ha/pre", ["builtin:announcement_chime", "http://ha/announce"]),
    (True, False, None, ["http://ha/announce"]),
    (False, True, "http://ha/pre", ["http://ha/pre", "http://ha/announce"]),
    (False, True, None, ["http://ha/announce"]),
])
def test_an_announcement_opens_with_the_announcement_chime(capable, preannounce, url, played):
    async def main():
        h = Harness(announcement_chime=capable)
        await h.start()
        announce = asyncio.create_task(h.actor.announce("http://ha/announce", preannounce=preannounce,
                                                        preannounce_url=url, start_conversation=False))
        await h.wait_for(lambda: h.render.streams)
        if h.render.streams[0].source == "http://ha/pre":
            h.render.streams[0].finish("drained")
            await h.wait_for(lambda: len(h.render.streams) == 2)
        assert h.render.played == played
        assert all(p.announcement for p in h.render.streams)
        h.render.streams[-1].finish("drained")
        await asyncio.wait_for(announce, 2)
        await h.actor.close()
    run(main())


def test_an_interrupted_announcement_chime_drops_the_announcement():
    async def main():
        h = Harness()
        h.render.local_finish = "cancelled"     # e.g. the Dot's own wake chime took its slot
        await h.start()
        await asyncio.wait_for(h.actor.announce("http://ha/announce", preannounce=True,
                                                preannounce_url=None, start_conversation=False), 2)
        assert h.render.played == ["builtin:announcement_chime"]
        acquired = [b["lease_id"] for b, _ in h.link.of("focus.acquire") if b["focus"] == "dialog_output"]
        released = [b["lease_id"] for b, _ in h.link.of("focus.release")]
        assert len(acquired) == 1 and acquired == released
        await h.actor.close()
    run(main())


# --- transcript correction (§16.8) -----------------------------------------------------------------

WATCH_FLEX = [("▁WATCH", PRESS + 2_500), ("▁FLEX", PRESS + 6_000)]
PLEX = SentenceRecognition(True, False, "trigger", "(watch|put on) [the] plex")


def fix_harness(mode: str, **kw) -> Harness:
    """A Dot whose transcript is "Watch Flex." against tables holding the trigger
    "(watch|put on) [the] plex", which HA's matcher accepts as "watch plex"."""
    h = Harness(config={"transcriptFix": mode}, text_fix=True, **kw)
    h.ha.stt_text = "Watch Flex."
    h.ha.recognitions = {"watch plex": PLEX}
    h.ha.next_run = FakeRun([IntentEnded("OK.", None, False, "action_done", True),
                             TtsReady("http://ha/tts/1", False), RunEnded()])
    return h


async def watch_flex(h: Harness) -> None:
    h.worker.vad = speech(QUESTION)
    h.device.level = levels(QUESTION)
    h.worker.tokens = WATCH_FLEX
    await h.start()
    h.actor.button_turn(button())
    await h.settle()
    await h.feed(PRESS - 4_800, PRESS + 50_000)


async def finished_fix(h: Harness) -> dict | None:
    await h.wait_for(lambda: h.render.streams)
    h.render.streams[0].finish("drained")
    await h.wait_for(lambda: h.rows, timeout=10)
    row = h.rows[0]
    assert row["stt_text"] == "Watch Flex."               # the transcript is kept as heard
    return json.loads(row["stt_fix"]) if row["stt_fix"] is not None else None


def test_off_does_no_correction_work():
    async def main():
        h = fix_harness("off")
        await watch_flex(h)
        assert await finished_fix(h) is None
        assert h.ha.intents == [("Watch Flex.", None)] and h.ha.fix_calls() == []
        await h.actor.close()
    run(main())


def test_on_sends_the_validated_correction_matched_in_the_pipelines_language_for_this_satellite():
    async def main():
        h = fix_harness("on")
        await watch_flex(h)
        record = await finished_fix(h)
        assert h.ha.intents == [("watch plex", None)]
        assert record["applied"] is True and record["reason"] == "applied"
        assert (record["original"], record["proposed"], record["sent"]) == ("Watch Flex.", "watch plex", "watch plex")
        assert record["rule"] == "phrase" and record["source"] == "trigger" and record["deadline_met"] is True
        assert h.ha.fix_calls() and all(dev == "hadev" and lang == "en" for _, dev, lang in h.ha.fix_calls())
        await h.actor.close()
    run(main())


def test_shadow_sends_the_original_at_once_and_records_what_on_would_have_sent():
    async def main():
        h = fix_harness("shadow")
        h.ha.fix_delay = 1.0
        await watch_flex(h)
        await h.wait_for(lambda: h.ha.intents, timeout=0.5)   # dispatch never waits for the correction
        assert h.ha.intents == [("Watch Flex.", None)]
        record = await finished_fix(h)
        assert record["reason"] == "would_apply" and record["applied"] is False
        assert (record["proposed"], record["sent"]) == ("watch plex", "Watch Flex.")
        assert record["deadline_met"] is False and record["latency_ms"] >= 1_000
        await h.actor.close()
    run(main())


@pytest.mark.parametrize("fault, reason", [("slow", "timeout"), ("error", "error"), ("stale", "stale")])
def test_on_sends_the_original_when_the_correction_is_late_fails_or_goes_stale(fault, reason):
    async def main():
        h = fix_harness("on")
        if fault == "slow":
            h.ha.fix_delay = 0.3
        elif fault == "error":
            h.ha.fix_error = HaUnavailable("connection lost")
        else:
            # The tables change while HA validates the candidate (a registry event, say).
            h.ha.on_fix = lambda sentences: "watch plex" in sentences and h.text_fix.invalidate("test")
        await watch_flex(h)
        record = await finished_fix(h)
        assert h.ha.intents == [("Watch Flex.", None)]
        assert record["reason"] == reason and record["applied"] is False and record["sent"] == "Watch Flex."
        await h.actor.close()
    run(main())


def test_a_turn_interrupted_while_its_correction_runs_dispatches_nothing(monkeypatch):
    monkeypatch.setattr(em_session, "FINAL_DEADLINE_S", 5.0)

    async def main():
        h = fix_harness("on")
        h.ha.fix_delay = 0.5
        await watch_flex(h)
        await h.wait_for(lambda: h.ha.fix_calls())
        h.actor.on_message(envelope("privacy.changed", {"muted": True, "capture_epoch": None, "physical_seq": 2}))
        await asyncio.sleep(0.8)
        await h.settle()
        assert h.terminals() == ["muted"] and h.ha.intents == []
        await h.actor.close()
    run(main())


def test_a_pause_result_is_reused_and_the_endpoint_never_waits_for_it():
    async def main():
        h = fix_harness("on")
        h.ha.recognizer = True
        await watch_flex(h)
        record = await finished_fix(h)
        assert record["cache"] == "hit" and h.ha.intents == [("watch plex", None)]
        # Each prefix's endpoint question went first; its correction asked only after it.
        for prefix in ("watch", "watch flex"):
            asked = next(i for i, (s, _, lang) in enumerate(h.ha.calls) if s[0] == prefix and lang is None)
            fixed = [i for i, (s, _, lang) in enumerate(h.ha.calls) if s == (prefix,) and lang is not None]
            assert all(i > asked for i in fixed)
        assert any(s == ("watch flex",) and lang == "en" for s, _, lang in h.ha.calls)
        await h.actor.close()
    run(main())


def test_local_routes_read_the_transcript_as_heard_and_are_never_corrected():
    async def main():
        h = Harness(config={"transcriptFix": "on"}, text_fix=True)
        h.ha.stt_text = "Set an alarm for 7 AM."
        h.alerts.set_result = {"ok": True, "first_due": "2030-01-02T07:00:00+00:00", "days": []}
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.rows)
        assert h.rows[0]["outcome"] == "alarm" and h.rows[0]["stt_fix"] is None and h.ha.fix_calls() == []
        await h.actor.close()
    run(main())


def test_an_answer_in_a_conversation_is_sent_as_heard():
    async def main():
        h = Harness(config={"transcriptFix": "on"}, text_fix=True)
        drain = await question_drained(h)                     # HA asked "Which TV?" in conv-1
        answer = (drain + 16_000, drain + 25_728)
        h.worker.vad = speech(answer)
        h.device.level = levels(answer)
        h.worker.tokens = [("▁LIVING", answer[0] + 3_000), ("▁ROOM", answer[0] + 6_000)]
        h.ha.stt_text = "The living groom."
        h.ha.next_run = FakeRun([IntentEnded("Turned off.", "conv-1", False, "action_done", False),
                                 TtsReady("http://ha/tts/2", False), RunEnded()])
        fixes = len(h.ha.fix_calls())
        await h.feed(drain, drain + 100_000)
        await h.wait_for(lambda: len(h.render.streams) == 2)
        assert h.ha.intents[1] == ("The living groom.", "conv-1") and len(h.ha.fix_calls()) == fixes
        h.render.streams[1].finish("drained")
        await h.wait_for(lambda: len(h.rows) == 2)
        assert json.loads(h.rows[1]["stt_fix"])["reason"] == "contextual"
        await h.actor.close()
    run(main())


# --- turn view (docs/echo-show-companion-plan.md §11 decision 8) -----------------------------


def views(h: Harness) -> list[dict]:
    return [b for b, _ in h.link.of("turn.view")]


def shown(view: dict) -> tuple:
    return view["phase"], view["heard"], view["answer"], view["final"], view["follow_up"]


def test_a_turn_view_device_sees_the_turn_listen_think_answer_and_end_after_the_reply_plays():
    async def main():
        h = Harness(turn_view=True)
        h.ha.next_run = FakeRun([IntentEnded("It is 3 PM.", "c", False, "action_done", True),
                                 TtsReady("http://ha/tts/1", False), RunEnded()])
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        assert views(h)[-1]["phase"] == "answering"          # done waits for the reply to play
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.rows)
        heard = "What time is it?"
        assert [shown(v) for v in views(h)] == [
            ("listening", None, None, False, False),
            ("thinking", heard, None, False, False),
            ("answering", heard, "It is 3 PM.", True, False),
            ("done", heard, "It is 3 PM.", True, False)]
        assert {v["turn_id"] for v in views(h)} == {h.rows[0]["turn_uuid"]}
        assert {g for _, g in h.link.of("turn.view")} == {0}
        await h.actor.close()
    run(main())


def test_a_streamed_answer_goes_out_at_most_every_interval_and_its_latest_text_is_never_dropped(monkeypatch):
    monkeypatch.setattr(em_session, "ANSWER_INTERVAL_S", 0.3)

    async def main():
        h = Harness(turn_view=True)
        run_ = h.ha.next_run = FakeRun()
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: views(h)[-1]["phase"] == "thinking")

        def streamed():
            return [(v["answer"], v["final"]) for v in views(h) if v["phase"] == "answering"]

        for piece in ("It ", "is ", "3 ", "PM."):
            run_.queue.put_nowait(AnswerDelta(piece))
        await h.settle()
        assert streamed() == []                               # within the interval of `thinking`
        await h.wait_for(lambda: streamed())
        assert streamed() == [("It is 3 PM.", False)]         # four pieces, one send
        run_.queue.put_nowait(AnswerDelta(" Anything else?"))
        await h.settle()
        assert len(streamed()) == 1                           # held back...
        run_.queue.put_nowait(IntentEnded("It is 3 PM.", "c", False, "action_done", False))
        await h.settle()
        # ...and replaced at once by intent-end's speech, never sent after it.
        await asyncio.sleep(0.4)
        assert streamed() == [("It is 3 PM.", False), ("It is 3 PM.", True)]
        for event in (TtsReady("http://ha/tts/1", False), RunEnded()):
            run_.queue.put_nowait(event)
        await h.wait_for(lambda: h.render.streams)
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: views(h)[-1]["phase"] == "done")
        assert shown(views(h)[-1]) == ("done", "What time is it?", "It is 3 PM.", True, False)
        await h.actor.close()
    run(main())


def test_a_question_keeps_the_view_up_into_its_reply_and_a_silent_reply_ends_with_no_answer():
    async def main():
        h = Harness(turn_view=True)
        await question_drained(h)                             # HA asked "Which TV?" in conv-1
        asking, reply = views(h)[0]["turn_id"], h.actor._turn.turn_id
        assert [(v["turn_id"], *shown(v)) for v in views(h)[-2:]] == [
            (asking, "done", "What time is it?", "Which TV?", True, True),
            (reply, "listening", None, None, False, False)]
        base = h.device.pos[Kind.MIC]
        await h.feed(base, base + 7 * 16_000 + 4_000)
        await h.wait_for(lambda: h.actor.state == "IDLE")
        assert h.terminals()[-1] == "no_input"
        assert (views(h)[-1]["turn_id"], *shown(views(h)[-1])) == (reply, "done", None, None, False, False)
        await h.actor.close()
    run(main())


def test_a_local_route_shows_the_text_as_heard_and_its_own_answer():
    async def main():
        h = Harness(config={"transcriptFix": "on"}, text_fix=True, turn_view=True)
        h.ha.stt_text = "When's my next alarm?"
        h.alerts.alarms = [{"name": "Alarm", "kind": "alarm", "due": "2030-01-02T07:00:00+00:00",
                            "repeats": [], "schedule_id": "s1"}]
        await button_turn(h, PRESS + 50_000)
        await h.wait_for(lambda: h.render.streams)
        h.render.streams[0].finish("drained")
        await h.wait_for(lambda: h.rows)
        heard, said = "When's my next alarm?", "Your next alarm is 7 AM on January 2."
        assert [shown(v) for v in views(h)] == [
            ("listening", None, None, False, False),
            ("thinking", heard, None, False, False),
            ("answering", heard, said, True, False),
            ("done", heard, said, True, False)]
        await h.actor.close()
    run(main())


def test_a_device_without_turn_view_is_sent_none():
    async def main():
        h = Harness()
        await answered(h, "It is 3 PM.", "http://ha/tts/1")
        assert h.rows[0]["response_text"] == "It is 3 PM." and views(h) == []
        await h.actor.close()
    run(main())
