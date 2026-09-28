"""Per-device session actor (SPEC §3.1–3.2, §6–§9, §11.3, §16.2, §16.6–16.7).

One actor per device serializes physical events, uplink audio, speech
evidence, HA results, playback and deadlines through one priority queue in
the §16.2 order. It alone accepts wakes, commits endpoints, and dispatches.
Model work stays in `SpeechWorker`; the pure per-utterance composition lives
in `em_utterance`. Long awaits (HA runs, playback) run in helper tasks that
act only while their turn/expectation is still current (generation fencing,
§3.2 invariant 3).

Positions are capture-epoch sample indices at 16 kHz; every reducer age is
sample time. Wall-clock deadlines (§7) use the event-loop clock.
"""

from __future__ import annotations

import asyncio
import inspect
import json
import logging
import time
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from typing import Any, AsyncIterator, Awaitable, Callable, Coroutine, TYPE_CHECKING
from zoneinfo import ZoneInfo

import numpy as np

import echomuse_grammar
import em_db
import em_recordings
import em_wakeclips
from echomuse_grammar import AMPM_CHOICES, AlarmParse, Choice, CommandContext
from em_attribution import (
    CELL,
    COVERAGE_FULL,
    COVERAGE_PARTIAL,
    ECHO_ONLY,
    MAX_LAG,
    NEAR_END_PRESENT,
    NO_REFERENCE,
    UNKNOWN,
    EchoTracker,
    compare_reference,
    estimate_lag,
    reference_candidate_overlaps,
    self_playback_verdict,
    wake_trigger_sample,
)
from em_audio_timeline import (
    CANDIDATE_CELLS_LEAD,
    CANDIDATE_REFERENCE_LEAD,
    LeaseMessage,
    LeaseReason,
    LeaseTimeline,
    UplinkSession,
    parse_u64,
)
from em_endpoint_policy import redecode_differs
from em_ha_client import (
    HaError,
    HaUnavailable,
    IntentEnded,
    RunEnded,
    RunFailed,
    RunLost,
    SttEnded,
    RunRejected,
    TtsReady,
)
from em_speech_worker import (
    VERIFICATION_LOOKAHEAD,
    Observation,
    SpeechWorker,
    SpeechWorkerError,
)
from em_stt_copy import asr_gain_for, stt_copy
from em_utterance import (
    PREROLL,
    CellAssembler,
    Close,
    Commit,
    Pending,
    ReplyWatch,
    Revoked,
    Utterance,
    UtteranceSpec,
    redecoded_command,
)
from em_wake_phrase import final_command_text

if TYPE_CHECKING:
    from em_alerts import AlertEngine
    from em_arbiter import WakeArbiter
    from em_device_link import DeviceLink
    from em_ha_client import HaClient, Vocabulary
    from em_render import Playback, RenderClient
    from em_wake_registry import WakeModel, WakeRegistry

log = logging.getLogger("echomuse.session")

SAMPLE_RATE = 16_000

# §7 / §9 / §16.2 / §16.6 deadlines (seconds of loop time unless noted).
WAKE_VERIFY_S = 0.700            # verification deadline from receipt of wake.candidate
FOCUS_TTL_MS = 3_000             # ephemeral dialog focus; renewed every second
RENEW_S = 1.0
HA_TIMEOUT_S = 30.0              # intent run sent → intent-end
RESPONSE_START_S = 5.0           # intent-end → response audio start
RESPONSE_STALL_S = 2.0           # no output progress
RESPONSE_TOTAL_S = 120.0         # one spoken response
REPLY_S = 7.0                    # reply window after guarded drain
REPLY_CHAIN_MAX = 5              # no-wake replies per chain
REPLY_CHAIN_S = 60.0
ACTOR_TICK_S = 0.050
LOCAL_ACT_TIMEOUT_S = 2.0
PERSIST_SHUTDOWN_S = 2.0         # shutdown waits this long for pending turn rows (§11.3)

# Sample-time bounds on evidence the actor waits for.
ECHO_WAIT = SAMPLE_RATE          # reference for a cell's echo label: wait ≤1 s of mic time
ASR_STALL = 4 * SAMPLE_RATE      # utterance ASR this far behind the mic: the stream is dead

# §16.2 priority order of already-received events.
P_MUTE, P_STOP, P_LOSS, P_INTERRUPT, P_GAP, P_SPEECH, P_RESULT, P_DEADLINE = range(8)

STATES = ("IDLE", "ARMED", "LISTENING", "END_PENDING", "COMMITTED",
          "THINKING", "SPEAKING", "EXPECT_REPLY", "CLOSING")

# SPEC §7 terminal reasons with feedback. `completed`, `reply_timeout` and
# `superseded` (a newer wake/button took over) are normal ends without feedback.
ERROR_LINE_REASONS = frozenset({"speech_unavailable", "ha_timeout", "response_timeout", "stt_failed", "ha_error"})
CLARIFY_REASONS = frozenset({"empty_transcript", "retry"})
ERROR_CUE_REASONS = frozenset({"interrupted", "audio_overrun"})
ERROR_CUE = "error_anim"
NO_SPEECH_CUE = "nospeech_anim"

LINE_SORRY = "Sorry, I didn't catch that."
LINE_TOO_LONG = "That was too long. Try a shorter request."
LINE_ERROR = "Something went wrong."
LINE_UNKNOWN = "I'm not sure that worked."
LINE_AMPM = "AM or PM?"
LINE_AMPM_AGAIN = "Please say AM or PM."
LINE_LEFT_IT = "Okay, I left it."

ECHO_TAG = "echo"                # submit_echo tag for per-cell labels
CANDIDATE_TAG = "candidate:"     # submit_echo tag prefix for a candidate comparison


def _stream_url(url: str) -> AsyncIterator[np.ndarray]:
    """48 kHz mono int16 blocks of an HA media URL (em_media, imported lazily: it spawns ffmpeg)."""
    from em_media import stream_url

    return stream_url(url)


class _Link:
    """What the actor uses of `em_device_link.DeviceLink`."""

    closed: bool

    async def send(self, msg_type: str, body: dict, *, generation: int = 0) -> str: ...
    async def request(self, msg_type: str, body: dict, *, generation: int = 0,
                      timeout: float = 2.0) -> dict: ...
    async def ack(self, message_id: str, status: str, error: str | None = None) -> None: ...


@dataclass
class ActorDeps:
    ha: "HaClient"
    worker: SpeechWorker
    registry: "WakeRegistry"
    alerts: "AlertEngine"
    arbiter: "WakeArbiter"
    config: Callable[[], dict]
    ha_device_id: Callable[[], str | None]
    pipeline_id: Callable[[], Awaitable[str]]
    vocabulary: Callable[[], "Vocabulary | None"]
    esphome_reply: Callable[[bytes], AsyncIterator[object]]
    persist_turn: Callable[[dict], Awaitable[int]]


@dataclass(frozen=True)
class ActorEvent:
    """`state` on every state change; `terminal` with the §7 reason; `cue` with
    `error_anim`/`nospeech_anim`; `dialog_focus` when `dialog_active` flips."""

    kind: str
    state: str
    reason: str | None = None
    dialog_active: bool = False


# --- reference access ---------------------------------------------------------------


def read_reference(view, a: int, b: int) -> tuple[np.ndarray, np.ndarray]:
    """Samples [a, b) of one reference epoch and which of them are known (missing = zero, invalid)."""
    out = np.zeros(max(0, b - a), dtype=np.int16)
    valid = np.ones(out.size, dtype=bool)
    for x, y in view.missing(a, b):
        valid[max(0, x - a):max(0, y - a)] = False
    i = 0
    while i < out.size:
        if not valid[i]:
            i += 1
            continue
        j = i
        while j < out.size and valid[j]:
            j += 1
        try:
            out[i:j] = view.read(a + i, a + j)
        except KeyError:
            valid[i:j] = False
        i = j
    return out, valid


@dataclass
class _Focus:
    lease_id: str
    owner: str
    focus: str
    generation: int
    renewed: float


@dataclass
class _Runtime:
    """Actor-side evidence state of one uplink lease the speech worker analyses."""

    lease_id: str
    assembler: CellAssembler
    utterance: Utterance | None = None
    echo: EchoTracker | None = None
    echo_open: int | None = None      # sample the per-cell echo lag is estimated around
    echo_inflight: bool = False
    echo_known: int = 0               # cells labelled with a trusted reference (coverage)
    echo_total: int = 0
    last_mic_end: int | None = None


@dataclass
class _Candidate:
    candidate_id: str
    lease_id: str
    body: dict
    model: "WakeModel"
    runtime: _Runtime
    capture_epoch: int
    received: float
    open_sample: int
    support_start: int
    threshold: float
    producing_sound: bool
    peak: float | None
    support_end: int | None = None
    comparison: str | None = None
    comparison_submitted: bool = False
    verification: str | None = None
    verification_submitted: bool = False
    reference_events: list[tuple[int, Any]] = field(default_factory=list)
    reference_ready: bool = False


@dataclass
class _Expectation:
    """A §9.1 reply expectation. `owner` names its focus/uplink leases and becomes
    the reply turn's id."""

    expectation_id: str
    owner: str
    generation: int
    source: str                       # reply (websocket path) | ha_reply (ESPHome path)
    conversation_id: str | None
    originating_turn_id: str | None
    chain_started: float
    chain_count: int
    choices: tuple[Choice, ...] | None = None
    pending_operation: dict | None = None
    reprompted: bool = False
    runtime: _Runtime | None = None
    watch: ReplyWatch = field(default_factory=ReplyWatch)
    prompt: "Playback | None" = None
    deadline: float | None = None
    drain_pending: bool = False


@dataclass
class _Turn:
    turn_id: str
    generation: int
    trigger: str                      # wake | button | reply | ha_reply
    opened: float
    utterance: Utterance
    runtime: _Runtime
    model: "WakeModel | None" = None
    candidate: _Candidate | None = None
    context: CommandContext | None = None
    context_target: str | None = None
    conversation_id: str | None = None
    expectation: _Expectation | None = None
    next_expectation: _Expectation | None = None
    task: asyncio.Task | None = None
    run: object | None = None
    playback: "Playback | None" = None
    wake_db: float | None = None
    wake_clip: bytes | None = None
    stt_copy: bytes | None = None
    stt_raw: str | None = None                # HA's transcript before wake-phrase removal
    stt_text: str | None = None               # what was routed: stripped HA transcript or local command
    response_text: str | None = None          # the spoken answer, from whichever handler took the request
    response_type: str | None = None          # HA intent-end response_type
    intent_local: bool | None = None          # HA's built-in agent answered (None: not reported)
    outcome: str | None = None
    terminal: str | None = None
    commit: Commit | None = None
    coverage: float | None = None
    timings: dict[str, int] = field(default_factory=dict)


@dataclass
class _Announcement:
    url: str
    preannounce_url: str | None
    start_conversation: bool
    future: asyncio.Future


class SessionActor:
    """One per device, outliving sessions. `start()` creates the actor task."""

    def __init__(self, device_id: str, deps: ActorDeps):
        self.device_id = device_id
        self.deps = deps
        self.state = "IDLE"
        self.awaiting_intent = False
        self.link: _Link | None = None
        self.render: "RenderClient | None" = None
        self.uplink: UplinkSession | None = None
        self._queue: asyncio.PriorityQueue = asyncio.PriorityQueue()
        self._sequence = 0
        self._task: asyncio.Task | None = None
        self._closed = False
        self._generation = 0
        self._turn: _Turn | None = None
        self._candidate: _Candidate | None = None
        self._expectation: _Expectation | None = None
        self._runtimes: dict[str, _Runtime] = {}
        self._focus: dict[str, _Focus] = {}
        self._dialog_playback: "Playback | None" = None
        self._listeners: list[Callable[[ActorEvent], Any]] = []
        self._muted = False
        self._alert_foreground: bool | None = None
        self._diagnostic: Callable[[int, np.ndarray], None] | None = None
        self._diagnostic_lease: str | None = None
        self._announcements = asyncio.Lock()
        self._tasks: set[asyncio.Task] = set()
        self._persists: set[asyncio.Task] = set()
        self._dialog_active = False

    # --- public surface -------------------------------------------------------

    @property
    def turn_active(self) -> bool:
        """A candidate, turn, reply expectation, or its feedback is live."""
        return self._turn is not None or self._expectation is not None or self.state != "IDLE"

    def add_listener(self, cb: Callable[[ActorEvent], Any]) -> Callable[[], None]:
        self._listeners.append(cb)

        def remove() -> None:
            if cb in self._listeners:
                self._listeners.remove(cb)
        return remove

    async def start(self) -> None:
        if self._task is None:
            self._task = asyncio.create_task(self._run(), name=f"session:{self.device_id}")

    async def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        self._queue.put_nowait((-1, 0, "close", None))
        if self._task is not None:
            await self._task

    def attach(self, link: "DeviceLink", render: "RenderClient") -> None:
        """Session ready: the new session's link, renderer, and an empty uplink record."""
        self.link, self.render, self.uplink = link, render, UplinkSession()
        self._post(P_LOSS, "attach", None)

    def detach(self, reason: str) -> None:
        """Session lost: nothing more is sent on the old link; the actor cleans up in order."""
        old = (self.uplink, self.render)
        self.link = self.render = self.uplink = None
        self._post(P_LOSS, "detach", old)

    def on_message(self, msg_type: str, envelope: dict) -> None:
        body = envelope.get("body") if isinstance(envelope.get("body"), dict) else {}
        if msg_type in ("stream.open", "stream.end"):
            # Applied at once: audio of a new epoch may follow on the audio socket
            # before the actor reaches this message (WIRE §4.2).
            if self.uplink is not None:
                if msg_type == "stream.open":
                    self.uplink.stream_open(body)
                else:
                    self.uplink.stream_end(body)
            if msg_type == "stream.open" and body.get("stream_id") == "mic":
                self._post(P_SPEECH, "mic_epoch", None)
            return
        priority = {
            "privacy.changed": P_MUTE,
            "uplink.ended": P_LOSS,
            "wake.candidate": P_SPEECH,
            "wake.candidate_end": P_SPEECH,
            "alert.state": P_RESULT,
        }.get(msg_type)
        if priority is not None:
            self._post(priority, msg_type, (envelope, body))

    def on_observation(self, obs: Observation) -> None:
        if obs.device_id == self.device_id:
            self._post(P_SPEECH, "observation", obs)

    def on_audio(self, frame: bytes) -> None:
        """One uplink EMA1 frame. Raises `em_audio_timeline.ProtocolError` for a WIRE violation."""
        if self.uplink is None:
            return
        deliveries = self.uplink.ingest(frame)
        if deliveries:
            gap = any(d.packet.discontinuity or d.packet.muted for d in deliveries)
            self._post(P_GAP if gap else P_SPEECH, "audio", deliveries)

    def button_turn(self, action: dict) -> None:
        self._post(P_INTERRUPT, "button", action)

    def cancel_turn(self, reason: str = "interrupted") -> None:
        self._post(P_STOP, "cancel", reason)

    async def announce(self, url: str, *, preannounce_url: str | None,
                       start_conversation: bool) -> None:
        """Play an HA announcement as dialog output; returns after guarded drain or cancellation."""
        if self.link is None or self.render is None or self._closed:
            return
        future = asyncio.get_running_loop().create_future()
        self._post(P_RESULT, "announce", _Announcement(url, preannounce_url, start_conversation, future))
        await future

    async def open_diagnostic(self, on_mic: Callable[[int, np.ndarray], None]) -> None:
        """Hold a live-mic diagnostic lease (re-opened after every reconnect); wakes are refused meanwhile."""
        self._diagnostic = on_mic
        self._post(P_INTERRUPT, "diagnostic", None)

    async def close_diagnostic(self) -> None:
        self._diagnostic = None
        self._post(P_INTERRUPT, "diagnostic", None)

    # --- loop ---------------------------------------------------------------------

    def _post(self, priority: int, kind: str, value: object) -> None:
        if self._closed:
            if isinstance(value, _Announcement) and not value.future.done():
                value.future.set_result(None)
            return
        self._sequence += 1
        self._queue.put_nowait((priority, self._sequence, kind, value))

    def _spawn(self, coro: Coroutine) -> asyncio.Task:
        task = asyncio.create_task(coro)
        self._tasks.add(task)
        task.add_done_callback(self._task_done)
        return task

    def _task_done(self, task: asyncio.Task) -> None:
        self._tasks.discard(task)
        self._persists.discard(task)
        if not task.cancelled() and task.exception() is not None:
            log.error("[%s] actor helper failed", self.device_id, exc_info=task.exception())

    async def _run(self) -> None:
        loop = asyncio.get_running_loop()
        last_tick = loop.time()
        while True:
            timeout = max(0.0, ACTOR_TICK_S - (loop.time() - last_tick))
            try:
                _, _, kind, value = await asyncio.wait_for(self._queue.get(), timeout)
            except TimeoutError:
                kind = None
            if kind == "close":
                break
            if kind is not None:
                try:
                    await self._dispatch(kind, value)
                except Exception:
                    log.exception("[%s] actor event %s failed", self.device_id, kind)
            if loop.time() - last_tick >= ACTOR_TICK_S:
                last_tick = loop.time()
                try:
                    await self._tick()
                except Exception:
                    log.exception("[%s] actor deadline tick failed", self.device_id)
        await self._shutdown()

    async def _dispatch(self, kind: str, value: Any) -> None:
        if kind == "attach":
            await self._sync_diagnostic()
        elif kind == "detach":
            await self._session_lost(*value)
        elif kind == "mic_epoch":
            await self._sync_diagnostic()
        elif kind == "privacy.changed":
            await self._privacy(value[1])
        elif kind == "uplink.ended":
            if self.uplink is not None:
                lease = self.uplink.leases.device_ended(value[1])
                if lease is not None:
                    await self._lease_ended(lease.lease_id, lease.ended or "closed")
        elif kind == "wake.candidate":
            await self._wake_candidate(*value)
        elif kind == "wake.candidate_end":
            self._candidate_end(value[1])
        elif kind == "alert.state":
            await self._alert_state(value[1])
        elif kind == "audio":
            await self._audio(value)
        elif kind == "observation":
            await self._observation(value)
        elif kind == "button":
            await self._button(value)
        elif kind == "cancel":
            await self._cancel(value)
        elif kind == "diagnostic":
            await self._sync_diagnostic()
        elif kind == "announce":
            self._spawn(self._announcement(value))

    async def _tick(self) -> None:
        loop = asyncio.get_running_loop()
        now = loop.time()
        if self.link is not None and self.uplink is not None:
            for lease in self.uplink.leases.due_renewals():
                await self._lease_message(self.uplink.leases.renew(lease.lease_id))
            for lease in self.uplink.leases.expire():
                await self._lease_ended(lease.lease_id, lease.ended or "ttl")
        for focus in list(self._focus.values()):
            if now - focus.renewed >= RENEW_S:
                focus.renewed = now
                await self._send("focus.renew", {"lease_id": focus.lease_id, "ttl_ms": FOCUS_TTL_MS},
                                 focus.generation)
        candidate = self._candidate
        if candidate is not None and now - candidate.received >= WAKE_VERIFY_S:
            await self._decide_candidate(candidate, deadline=True)
        exp = self._expectation
        if exp is not None and exp.deadline is not None and now >= exp.deadline and self._turn is None:
            await self._end_expectation("reply_timeout")

    async def _shutdown(self) -> None:
        if self._turn is not None:
            await self._finish_turn(self._turn, "session_lost", feedback=False)
        await self._drop_expectation()
        if self._persists:
            await asyncio.wait(list(self._persists), timeout=PERSIST_SHUTDOWN_S)
        for task in list(self._tasks):
            task.cancel()
        await asyncio.gather(*list(self._tasks), return_exceptions=True)

    # --- session lifecycle -----------------------------------------------------------

    async def _session_lost(self, uplink: UplinkSession | None, render: "RenderClient | None") -> None:
        # Every dialog and uplink lease ended with the session (§16.1); nothing is released on the wire.
        self._focus.clear()
        if uplink is not None:
            for lease in uplink.clear("session"):
                self.deps.worker.close_lease(lease.lease_id)
        self._candidate = None
        if self._turn is not None:
            await self._finish_turn(self._turn, "session_lost")
        await self._drop_expectation()
        self._runtimes.clear()
        self._diagnostic_lease = None
        self._alert_foreground = None
        if render is not None:
            render.fail_all("session_lost")
        self._update_dialog_active()
        if self._turn is None:
            self._set_state("IDLE")

    async def _privacy(self, body: dict) -> None:
        self._muted = bool(body.get("muted"))
        if not self._muted:
            return
        # Device-sovereign mute ends every lease and erases buffered audio (§3.2 inv. 10).
        if self.uplink is not None:
            for lease in self.uplink.clear("mute"):
                self.deps.worker.close_lease(lease.lease_id)
        self._candidate = None
        self._diagnostic_lease = None
        if self._turn is not None:
            await self._finish_turn(self._turn, "muted")
        await self._drop_expectation()
        self._runtimes.clear()
        if self._turn is None:
            self._set_state("IDLE")

    async def _alert_state(self, body: dict) -> None:
        active = body.get("active")
        foreground = bool(active.get("foreground")) if isinstance(active, dict) else None
        was, self._alert_foreground = self._alert_foreground, foreground
        turn = self._turn
        # The device foregrounds a backgrounded alert at its 15 s cap (§6.2).
        if was is False and foreground and turn is not None and turn.commit is None:
            await self._finish_turn(turn, "alert_preempted")

    async def _lease_ended(self, lease_id: str, reason: str) -> None:
        if self.uplink is not None:
            self.uplink.release(lease_id)
        runtime = self._runtimes.pop(lease_id, None)
        if runtime is not None:
            self.deps.worker.close_lease(lease_id)
        if lease_id == self._diagnostic_lease:
            self._diagnostic_lease = None
            if reason not in ("ttl", "closed"):   # 30 min cap and explicit closes stay closed
                await self._sync_diagnostic()
            return
        candidate = self._candidate
        if candidate is not None and candidate.lease_id == lease_id:
            await self._reject_candidate(candidate, "verifier_timeout")
            return
        turn = self._turn
        if turn is not None and turn.runtime.lease_id == lease_id and turn.commit is None:
            terminal = {"overrun": "audio_overrun", "mute": "muted", "session": "session_lost"}.get(
                reason, "interrupted")
            await self._finish_turn(turn, terminal)
        exp = self._expectation
        if exp is not None and exp.runtime is not None and exp.runtime.lease_id == lease_id:
            exp.runtime = None
            await self._end_expectation("interrupted")

    # --- diagnostic lease --------------------------------------------------------------

    async def _sync_diagnostic(self) -> None:
        if self._diagnostic is None:
            if self._diagnostic_lease is not None:
                await self._close_uplink(self._diagnostic_lease, "closed")
                if self.uplink is not None:
                    self.uplink.release(self._diagnostic_lease)
                self._diagnostic_lease = None
            return
        if self._diagnostic_lease is not None or self.uplink is None or self._muted:
            return
        epoch = self.uplink.streams.current("mic")
        if epoch is None:
            return
        lease_id = str(uuid.uuid4())
        await self._lease_message(self.uplink.open(
            lease_id, LeaseReason.DIAGNOSTIC, f"diagnostic:{self.device_id}", epoch, {"mic": None}))
        self._diagnostic_lease = lease_id

    # --- wake candidates -----------------------------------------------------------------

    def _refusal(self) -> str | None:
        if self._diagnostic is not None:
            return "diagnostic"
        if not self.deps.worker.available:
            return "speech_unavailable"
        if self._muted:
            return "muted"
        return None

    async def _wake_candidate(self, envelope: dict, body: dict) -> None:
        link, uplink = self.link, self.uplink
        if link is None or uplink is None:
            return
        message_id = str(envelope.get("message_id"))
        try:
            lease_id = str(body["lease_id"])
            candidate_id = str(body["candidate_id"])
            capture_epoch = parse_u64(body.get("capture_epoch"), "capture_epoch")
            support_start = parse_u64(body.get("support_start"), "support_start")
            hops = list(body.get("hops") or [])
            open_sample = max(parse_u64(h["end_sample"], "hops.end_sample") for h in hops)
            threshold = float(body["threshold"])
            wake_trigger_sample([(parse_u64(h["end_sample"], "end_sample"), h.get("raw")) for h in hops],
                                threshold)
            model = self.deps.registry.get(str(body["graph_sha256"]))
            uplink.candidate(lease_id, candidate_id, capture_epoch, support_start)
        except Exception as exc:
            log.warning("[%s] refused malformed wake.candidate: %s", self.device_id, exc)
            await link.ack(message_id, "rejected", "malformed")
            return
        refusal = self._refusal() or ("candidate_pending" if self._candidate is not None else None)
        if refusal is not None:
            uplink.acknowledge(lease_id, False)
            uplink.release(lease_id)
            await link.ack(message_id, "rejected", refusal)
            if refusal in ("diagnostic", "speech_unavailable"):
                self._cue(ERROR_CUE)
            return
        try:
            await self.deps.worker.load_wake_graph(model.graph_sha256)
            self.deps.worker.open_lease(self.device_id, capture_epoch, lease_id, model.graph_sha256)
        except Exception as exc:
            log.warning("[%s] wake candidate refused, speech worker: %s", self.device_id, exc)
            uplink.acknowledge(lease_id, False)
            uplink.release(lease_id)
            await link.ack(message_id, "rejected", "speech_unavailable")
            self._cue(ERROR_CUE)
            return
        uplink.acknowledge(lease_id, True)
        await link.ack(message_id, "accepted")
        runtime = self._new_runtime(lease_id)
        peak = max((float(h["smoothed"]) for h in hops if h.get("smoothed") is not None), default=None)
        candidate = _Candidate(
            candidate_id, lease_id, body, model, runtime, capture_epoch,
            asyncio.get_running_loop().time(), open_sample, support_start, threshold,
            bool(body.get("producing_sound")), peak,
        )
        self._candidate = candidate
        if not candidate.producing_sound:
            # Idle profile: the BCResNet threshold alone accepts (§6.1).
            await self._accept_candidate(candidate)

    def _candidate_end(self, body: dict) -> None:
        candidate = self._candidate
        if candidate is None or body.get("candidate_id") != candidate.candidate_id:
            turn = self._turn
            candidate = turn.candidate if turn is not None else None
            if candidate is None or body.get("candidate_id") != candidate.candidate_id:
                return
        candidate.support_end = parse_u64(body.get("support_end"), "support_end")
        if body.get("peak_smoothed") is not None:
            candidate.peak = float(body["peak_smoothed"])

    def _reference_overlap(self, candidate: _Candidate) -> bool:
        if self.uplink is None:
            return False
        support_end = candidate.support_end or candidate.open_sample
        for epoch, event in candidate.reference_events:
            mapping = self.uplink.clock_map(candidate.capture_epoch, epoch)
            if mapping is None:
                # Unknown timing: a wake-like reference candidate counts as overlapping (§3.2 inv. 7).
                return True
            start = mapping.reference_to_capture(event.support_start)
            end = mapping.reference_to_capture(event.support_end or event.first_crossing_end)
            uncertainty = int(max(start.uncertainty_samples, end.uncertainty_samples))
            if reference_candidate_overlaps(int(start.sample), int(end.sample),
                                            candidate.support_start, support_end, uncertainty):
                return True
        return False

    async def _decide_candidate(self, candidate: _Candidate, *, deadline: bool = False) -> None:
        """§6.1 steps 1–4 while the device produces sound; decided by the 700 ms deadline (§16.6)."""
        if candidate is not self._candidate:
            return
        evidence_done = candidate.comparison is not None and candidate.reference_ready
        if not evidence_done and not deadline:
            return
        verdict = self_playback_verdict(
            reference_candidate_overlaps=self._reference_overlap(candidate),
            comparison=candidate.comparison or UNKNOWN,
        )
        if verdict is not None:
            await self._reject_candidate(candidate, verdict)
        elif candidate.verification == "pass":
            await self._accept_candidate(candidate)
        elif candidate.verification == "fail":
            await self._reject_candidate(candidate, "unverified_wake")
        elif candidate.verification == "timeout" or deadline:
            await self._reject_candidate(candidate, "verifier_timeout")

    async def _reject_candidate(self, candidate: _Candidate, reason: str) -> None:
        """Close the candidate lease (also releasing any provisional duck); no turn, focus, or chime."""
        self._candidate = None
        close = "arbitration_lost" if reason == "arbitration_lost" else "rejected"
        await self._close_uplink(candidate.lease_id, close)
        self._release_runtime(candidate.lease_id)
        self._emit(ActorEvent("terminal", self.state, reason, self._dialog_active))
        self._persists.add(self._spawn(self._persist_candidate(candidate, reason)))

    async def _accept_candidate(self, candidate: _Candidate) -> None:
        context, target = self._command_context(candidate)
        if context is None:
            window = float((self.deps.config() or {}).get("wakeArbitrationMs", 700)) / 1000.0
            if self.deps.arbiter.claim(self.device_id, window) != self.device_id:
                await self._reject_candidate(candidate, "arbitration_lost")
                return
        self._candidate = None
        inherited = self._take_expectation()
        await self._supersede()
        uplink = self.uplink
        if uplink is None:
            return
        turn_id = str(uuid.uuid4())
        await self._lease_message(uplink.leases.convert_to_turn(candidate.lease_id, turn_id))
        hops = [(parse_u64(h["end_sample"], "end_sample"), h.get("raw")) for h in candidate.body["hops"]]
        trigger = wake_trigger_sample(hops, candidate.threshold)
        lease = uplink.leases.get(candidate.lease_id)
        start = max(candidate.support_start - PREROLL, lease.streams.get("mic") or 0)
        spec = self._spec("wake", start, trigger, inherited,
                          seed_start=candidate.support_start, wake_open=candidate.open_sample,
                          wake_phrase=candidate.model.wake_phrase, context=context)
        turn = _Turn(turn_id, self._generation, "wake", asyncio.get_running_loop().time(),
                     Utterance(spec), candidate.runtime, candidate.model, candidate, context, target,
                     inherited.conversation_id if inherited else None, inherited)
        self._turn = turn
        await self._acquire_focus(turn_id, "dialog_input", turn.generation)
        # No chime when the command context is non-empty: the ring/reply stopping is the acknowledgement (§16.2).
        if context is None and (self.deps.config() or {}).get("wakeSound") and self.render is not None:
            try:
                await self.render.play_local("earcon", "builtin:wake_chime", generation=turn.generation)
            except Exception:
                log.warning("[%s] wake chime failed", self.device_id, exc_info=True)
        self._set_state("ARMED")
        await self._open_utterance(turn)

    def _command_context(self, candidate: _Candidate) -> tuple[CommandContext | None, str | None]:
        """§6.3 step 1: the alert occurrence, else the dialog output this wake cancels."""
        active = candidate.body.get("active_alert")
        if isinstance(active, dict) and active.get("id") is not None:
            kind = active.get("kind") if active.get("kind") in ("alarm", "timer") else None
            return CommandContext("alert", kind, active.get("name")), str(active["id"])
        playback = self._dialog_playback
        if playback is not None and not playback.done:
            return CommandContext("dialog"), playback.playback_id
        return None, None

    # --- button, cancel, replies --------------------------------------------------------

    async def _button(self, body: dict) -> None:
        uplink = self.uplink
        if (self.link is None or uplink is None or self._refusal() is not None
                or body.get("capture_epoch") is None or body.get("capture_sample") is None):
            self._cue(ERROR_CUE)
            return
        capture_epoch = parse_u64(body["capture_epoch"], "capture_epoch")
        press = parse_u64(body["capture_sample"], "capture_sample")
        inherited = self._take_expectation()
        await self._supersede()
        turn_id, lease_id = str(uuid.uuid4()), str(uuid.uuid4())
        mic = max(0, press - PREROLL)
        await self._lease_message(uplink.open(lease_id, LeaseReason.TURN, turn_id, capture_epoch, {
            "mic": mic,
            "cells": max(0, mic - CANDIDATE_CELLS_LEAD),
            "reference": max(0, mic - CANDIDATE_REFERENCE_LEAD),
        }))
        model = self.deps.registry.for_config(self.deps.config())
        await self.deps.worker.load_wake_graph(model.graph_sha256)
        self.deps.worker.open_lease(self.device_id, capture_epoch, lease_id, model.graph_sha256)
        runtime = self._new_runtime(lease_id)
        start = uplink.leases.get(lease_id).streams["mic"]
        turn = _Turn(turn_id, self._generation, "button", asyncio.get_running_loop().time(),
                     Utterance(self._spec("button", start, press, inherited)), runtime,
                     conversation_id=inherited.conversation_id if inherited else None, expectation=inherited)
        self._turn = turn
        await self._acquire_focus(turn_id, "dialog_input", turn.generation)
        self._set_state("ARMED")
        await self._open_utterance(turn)

    async def _cancel(self, reason: str) -> None:
        if self._turn is not None:
            await self._finish_turn(self._turn, reason)
        elif self._expectation is not None:
            await self._end_expectation(reason)
        elif self._dialog_playback is not None and not self._dialog_playback.done:
            await self._dialog_playback.cancel(reason)

    def _take_expectation(self) -> _Expectation | None:
        """A still-valid question survives an explicit wake/button as context (§9.1)."""
        exp = self._expectation
        if exp is None:
            return None
        now = asyncio.get_running_loop().time()
        if (exp.deadline is not None and now >= exp.deadline) or now - exp.chain_started >= REPLY_CHAIN_S:
            return None
        return exp

    async def _supersede(self) -> None:
        """A new turn: bump the generation, then cancel prior output and fence the old turn (§16.2)."""
        self._generation += 1
        if self._turn is not None:
            await self._finish_turn(self._turn, "superseded", feedback=False)
        await self._drop_expectation()
        if self._dialog_playback is not None and not self._dialog_playback.done:
            await self._dialog_playback.cancel("interrupted")

    def _spec(self, kind: str, start: int, trigger: int, inherited: _Expectation | None = None,
              **extra: Any) -> UtteranceSpec:
        vocab = self.deps.vocabulary()
        return UtteranceSpec(
            utterance_id=str(uuid.uuid4()), kind=kind, start=start, trigger=trigger,
            vocabulary=tuple(sorted(vocab.targets())) if vocab is not None else (),
            choices=inherited.choices if inherited is not None else None,
            extended=bool((self.deps.config() or {}).get("extendedUtterances")),
            **extra,
        )

    async def _open_utterance(self, turn: _Turn) -> None:
        """Bind the utterance to its lease: ASR from its pre-roll, per-cell echo from its start."""
        runtime, spec = turn.runtime, turn.utterance.spec
        timeline = self._timeline(runtime)
        if timeline is None:
            await self._finish_turn(turn, "session_lost")
            return
        timeline.set_utterance_start(spec.start)
        try:
            self.deps.worker.open_utterance(runtime.lease_id, spec.utterance_id, spec.start, spec.trigger,
                                            timeline.mic)
        except SpeechWorkerError as exc:
            log.warning("[%s] utterance ASR failed to open: %s", self.device_id, exc)
            await self._finish_turn(turn, "interrupted")
            return
        runtime.utterance = turn.utterance
        runtime.echo_open = spec.start
        runtime.echo = None
        runtime.assembler.require_echo_from(spec.start)
        for ev in list(runtime.assembler.history):
            turn.utterance.push_cell(ev)
        await self._pump(runtime)

    async def _start_reply(self, exp: _Expectation, onset: int) -> None:
        """The qualifying reply starts at onset − 300 ms under the expectation's context (§16.6)."""
        runtime = exp.runtime
        timeline = self._timeline(runtime) if runtime is not None else None
        if exp is not self._expectation or timeline is None:
            return
        # §16.2: the generation increases before prior output is cancelled.
        self._generation += 1
        if exp.prompt is not None and not exp.prompt.done:
            await exp.prompt.cancel("early_answer")
        if self._turn is not None:
            await self._finish_turn(self._turn, "completed", feedback=False)
        self._expectation = None
        first = timeline.mic.first_sample if timeline.mic.first_sample is not None else onset
        start = min(onset, max(onset - PREROLL, first))
        kind = "ha_reply" if exp.source == "ha_reply" else "reply"
        turn = _Turn(exp.owner, self._generation, kind, asyncio.get_running_loop().time(),
                     Utterance(self._spec(kind, start, onset, exp)), runtime,
                     conversation_id=exp.conversation_id, expectation=exp)
        self._turn = turn
        self._set_state("ARMED")
        await self._open_utterance(turn)

    # --- audio and evidence ---------------------------------------------------------------

    def _new_runtime(self, lease_id: str) -> _Runtime:
        runtime = _Runtime(lease_id, CellAssembler())
        self._runtimes[lease_id] = runtime
        return runtime

    def _release_runtime(self, lease_id: str) -> None:
        if self._runtimes.pop(lease_id, None) is not None:
            self.deps.worker.close_lease(lease_id)
        if self.uplink is not None:
            self.uplink.release(lease_id)

    def _timeline(self, runtime: _Runtime | None) -> LeaseTimeline | None:
        if runtime is None or self.uplink is None or self._runtimes.get(runtime.lease_id) is not runtime:
            return None
        return self.uplink.timelines.get(runtime.lease_id)

    async def _audio(self, deliveries: list) -> None:
        if self.uplink is None:
            return
        touched: dict[str, _Runtime] = {}
        for d in deliveries:
            timeline = self.uplink.timelines.get(d.lease_id)
            if timeline is None:
                continue
            runtime = self._runtimes.get(d.lease_id)
            if d.stream_id == "mic":
                for start, end in d.ranges:
                    pcm = timeline.mic.read(start, end)
                    if d.lease_id == self._diagnostic_lease and self._diagnostic is not None:
                        try:
                            self._diagnostic(start, pcm)
                        except Exception:
                            log.exception("[%s] diagnostic consumer failed", self.device_id)
                    if runtime is None:
                        continue
                    runtime.assembler.set_vad_from(start)
                    if runtime.last_mic_end is not None and start > runtime.last_mic_end:
                        runtime.assembler.mark_gap(runtime.last_mic_end, start)
                    runtime.last_mic_end = max(runtime.last_mic_end or end, end)
                    self.deps.worker.submit_mic_block(d.lease_id, start, pcm, flags=d.packet.flags)
            elif runtime is not None and d.stream_id == "cells":
                for start, end in d.ranges:
                    records = timeline.cells.read(start, end)
                    runtime.assembler.add_cells(start, records.e_db.tolist(), records.flags.tolist())
            elif runtime is not None and d.stream_id == "reference":
                candidate = self._candidate
                if candidate is not None and candidate.lease_id == d.lease_id and candidate.producing_sound:
                    view = timeline.reference.segment(d.epoch)
                    if view is not None:
                        self.deps.worker.submit_reference(d.lease_id, view)
            if runtime is not None:
                touched[d.lease_id] = runtime
        for runtime in touched.values():
            await self._pump(runtime)

    async def _observation(self, obs: Observation) -> None:
        runtime = self._runtimes.get(obs.lease_id) if obs.lease_id is not None else None
        if runtime is None:
            return
        payload = obs.payload
        candidate = self._candidate if self._candidate and self._candidate.runtime is runtime else None
        if obs.kind == "vad":
            runtime.assembler.add_vad(payload.first_cell_sample, payload.probabilities)
        elif obs.kind == "asr":
            if runtime.utterance is not None and obs.utterance_id == runtime.utterance.utterance_id:
                runtime.utterance.push_asr(payload.tokens, payload.token_emission_sample,
                                           payload.trailing_blank_frames, obs.through_sample)
        elif obs.kind == "echo":
            if obs.utterance_id and obs.utterance_id.startswith(CANDIDATE_TAG):
                if candidate is not None:
                    candidate.comparison = payload.results[-1]
                    await self._decide_candidate(candidate)
            else:
                runtime.echo_inflight = False
                runtime.assembler.add_echo(payload.first_cell_sample, payload.results)
                start = runtime.utterance.spec.start if runtime.utterance is not None else None
                for i, result in enumerate(payload.results):
                    if start is None or payload.first_cell_sample + i * CELL >= start:
                        runtime.echo_total += 1
                        runtime.echo_known += result in (ECHO_ONLY, NEAR_END_PRESENT, NO_REFERENCE)
        elif obs.kind == "verification":
            if candidate is not None and payload.candidate_id == candidate.candidate_id:
                candidate.verification = payload.result
                await self._decide_candidate(candidate)
        elif obs.kind == "reference_score":
            if candidate is not None:
                candidate.reference_events.extend((payload.reference_epoch, c) for c in payload.candidates)
                mapping = self.uplink.clock_map(candidate.capture_epoch, payload.reference_epoch) \
                    if self.uplink is not None else None
                end = candidate.support_end or candidate.open_sample
                if mapping is None or mapping.reference_to_capture(obs.through_sample).sample >= end:
                    candidate.reference_ready = True
                await self._decide_candidate(candidate)
        elif obs.kind == "error":
            await self._worker_error(runtime, obs)
            return
        await self._pump(runtime)

    async def _worker_error(self, runtime: _Runtime, obs: Observation) -> None:
        """A worker failure aborts the affected utterance without dispatch (§8.1)."""
        log.warning("[%s] speech worker %s failed: %s", self.device_id,
                    obs.payload.component, obs.payload.reason)
        runtime.echo_inflight = False
        candidate = self._candidate
        if candidate is not None and candidate.runtime is runtime:
            if obs.payload.component == "verification":
                candidate.verification = "timeout"
            await self._decide_candidate(candidate)
            return
        turn = self._turn
        if turn is not None and turn.runtime is runtime and turn.commit is None:
            await self._finish_turn(turn, "interrupted")
            return
        exp = self._expectation
        if exp is not None and exp.runtime is runtime:
            await self._end_expectation("interrupted")

    async def _pump(self, runtime: _Runtime) -> None:
        """Advance one lease's evidence to its frontier and act on what it decides."""
        timeline = self._timeline(runtime)
        if timeline is None:
            return
        await self._schedule_echo(runtime, timeline)
        released = runtime.assembler.release()
        exp = self._expectation
        if exp is not None and exp.runtime is runtime and runtime.utterance is None:
            if exp.drain_pending and runtime.assembler.frontier is not None:
                exp.drain_pending = False
                onset = exp.watch.drained(runtime.assembler.history[0].start, runtime.assembler.history)
                if onset is not None:
                    await self._start_reply(exp, onset)
                    return
            for ev in released:
                onset = exp.watch.push(ev)
                if onset is not None:
                    await self._start_reply(exp, onset)
                    return
        utterance = runtime.utterance
        if utterance is None:
            await self._schedule_candidate_comparison(runtime, timeline)
            return
        for ev in released:
            utterance.push_cell(ev)
        turn = self._turn
        if turn is None or turn.utterance is not utterance or utterance.done:
            return
        self._measure_wake(turn, runtime)
        mic_end = timeline.mic.frontier or utterance.spec.start
        for decision in utterance.advance(mic_end):
            await self._decision(turn, decision)
            if turn.terminal is not None or utterance.done:
                return
        if self.state == "ARMED" and utterance.has_command_speech:
            self._set_state("LISTENING")
        if mic_end - utterance.asr_through > ASR_STALL:
            log.warning("[%s] utterance ASR stopped at %d with mic at %d", self.device_id,
                        utterance.asr_through, mic_end)
            await self._finish_turn(turn, "interrupted")

    def _measure_wake(self, turn: _Turn, runtime: _Runtime) -> None:
        """Peak cell level over the accepted candidate's support: the STT copy's gain anchor."""
        candidate = turn.candidate
        if candidate is None or turn.wake_db is not None:
            return
        a = candidate.support_start - candidate.support_start % CELL
        b = candidate.open_sample - candidate.open_sample % CELL
        cells = runtime.assembler.lookup(a, b)
        if runtime.assembler.frontier is None or runtime.assembler.frontier < b or not cells:
            return
        levels = [ev.cell.level for ev in cells if ev.cell.level is not None]
        if levels:
            turn.wake_db = max(levels)

    # --- attribution jobs -----------------------------------------------------------------

    def _reference_ready(self, timeline: LeaseTimeline, capture_end: int) -> bool | None:
        """Reference received through `capture_end` in mic time; None when timing is unknown."""
        epoch = timeline.reference.current_epoch
        if epoch is None or self.uplink is None:
            return None
        mapping = self.uplink.clock_map(timeline.lease.capture_epoch, epoch)
        view = timeline.reference.segment(epoch)
        if mapping is None or view is None:
            return None
        frontier = view.frontier
        return frontier is not None and frontier >= mapping.capture_to_reference(capture_end).sample

    def _reference_window(self, timeline: LeaseTimeline, a: int, b: int):
        """Final-mix reference for mic [a, b) laid out MAX_LAG samples early (em_attribution).

        Mapped through the clock fit at the window start; the two 16 kHz clocks
        drift <1,000 ppm, under a sample across the ≤3 s windows compared here.
        """
        epoch = timeline.reference.current_epoch
        if epoch is None or self.uplink is None:
            return None
        mapping = self.uplink.clock_map(timeline.lease.capture_epoch, epoch)
        view = timeline.reference.segment(epoch)
        if mapping is None or view is None:
            return None
        r0 = int(round(mapping.capture_to_reference(a - MAX_LAG).sample))
        reference, valid = read_reference(view, r0, r0 + (b - a) + MAX_LAG)
        return reference, valid, COVERAGE_FULL if valid.all() else COVERAGE_PARTIAL

    def _cell_arrays(self, runtime: _Runtime, timeline: LeaseTimeline, a: int, b: int):
        """Mic PCM, reference, and per-cell evidence for whole cells [a, b), or None if incomplete."""
        cells = runtime.assembler.lookup(a, b)
        if len(cells) != (b - a) // CELL or not timeline.mic.covers(a, b):
            return None
        reference = self._reference_window(timeline, a, b)
        if reference is None:
            return None
        ref, ref_valid, coverage = reference
        return (timeline.mic.read(a, b), ref, [ev.cell.valid for ev in cells], [ev.cell.vad for ev in cells],
                [ev.background for ev in cells], ref_valid, coverage)

    async def _schedule_echo(self, runtime: _Runtime, timeline: LeaseTimeline) -> None:
        """Per-cell echo labels for an open utterance or reply lease (§16.6), one job at a time."""
        if runtime.echo_inflight:
            return
        pending = runtime.assembler.echo_pending()
        if not pending:
            return
        if runtime.echo is None:
            first = timeline.mic.first_sample
            if first is None:
                return
            first = -(-first // CELL) * CELL
            runtime.echo = EchoTracker(open_sample=max(runtime.echo_open or first, first), lease_mic_start=first)
        tracker = runtime.echo
        mic_frontier = timeline.mic.frontier or 0
        steps: list[tuple] = []
        next_cell = pending[0].start
        lag_step = False
        for ev in pending:
            ready = self._reference_ready(timeline, ev.end)
            if ready is not True and mic_frontier - ev.end < ECHO_WAIT:
                break
            while next_cell < ev.start:     # skipped invalid cells keep the result run contiguous
                steps.append(("fixed", UNKNOWN))
                next_cell += CELL
            next_cell = ev.end
            if ready is not True:
                steps.append(("fixed", UNKNOWN))   # timing/reference unknown is never "no playback"
                continue
            if not lag_step:
                window = tracker.estimate_window(ev.end)
                if window is not None:
                    arrays = self._cell_arrays(runtime, timeline, *window)
                    if arrays is None:
                        tracker.update_lag(None)
                    else:
                        steps.append(("lag", arrays))
                        lag_step = True
            run_start = ev.start
            while run_start - CELL >= ev.end - 6 * CELL and timeline.mic.covers(run_start - CELL, run_start) \
                    and len(runtime.assembler.lookup(run_start - CELL, run_start)) == 1:
                run_start -= CELL
            arrays = self._cell_arrays(runtime, timeline, run_start, ev.end)
            steps.append(("label", arrays) if arrays is not None else ("fixed", UNKNOWN))
        if not any(step[0] != "lag" for step in steps):
            return
        runtime.assembler.echo_requested(next_cell)
        runtime.echo_inflight = True

        def compare() -> tuple[tuple[str, ...], int | None]:
            results = []
            for step in steps:
                if step[0] == "lag":
                    mic, ref, valid, _vad, _bg, ref_valid, _cov = step[1]
                    tracker.update_lag(estimate_lag(mic, ref, valid, ref_valid))
                elif step[0] == "fixed":
                    results.append(step[1])
                else:
                    mic, ref, valid, vad, bg, ref_valid, coverage = step[1]
                    results.append(tracker.label(mic, ref, cell_valid=valid, vad=vad, background=bg,
                                                 coverage=coverage, reference_valid=ref_valid))
            return tuple(results), tracker.lag

        self.deps.worker.submit_echo(runtime.lease_id, pending[0].start, next_cell, compare, ECHO_TAG)

    async def _schedule_candidate_comparison(self, runtime: _Runtime, timeline: LeaseTimeline) -> None:
        """§6.1 inputs for a producing-sound candidate: verification and the support comparison."""
        candidate = self._candidate
        if candidate is None or candidate.runtime is not runtime or not candidate.producing_sound:
            return
        if not candidate.verification_submitted:
            a = max(timeline.mic.floor, candidate.support_start - PREROLL)
            if timeline.mic.covers(a, candidate.open_sample + VERIFICATION_LOOKAHEAD):
                candidate.verification_submitted = True
                self.deps.worker.submit_verification(runtime.lease_id, candidate.candidate_id, timeline.mic,
                                                     candidate.support_start, candidate.open_sample,
                                                     candidate.model.verify_core)
        if candidate.comparison_submitted:
            return
        a = candidate.support_start - candidate.support_start % CELL
        end = candidate.support_end or candidate.open_sample
        b = -(-end // CELL) * CELL
        if runtime.assembler.frontier is None or runtime.assembler.frontier < b:
            return
        ready = self._reference_ready(timeline, b)
        if ready is False:
            return
        arrays = self._cell_arrays(runtime, timeline, a, b) if ready else None
        candidate.comparison_submitted = True
        if arrays is None:
            candidate.comparison = UNKNOWN    # missing timing or audio is unknown, not "no playback"
            candidate.reference_ready = True
            await self._decide_candidate(candidate)
            return
        mic, ref, valid, vad, bg, ref_valid, coverage = arrays
        cells = len(valid)

        def compare() -> tuple[tuple[str, ...], int | None]:
            result = compare_reference(mic, ref, cell_valid=valid, vad=vad, background=bg,
                                       coverage=coverage, reference_valid=ref_valid)
            return (result.result,) * cells, result.lag

        self.deps.worker.submit_echo(runtime.lease_id, a, b, compare, CANDIDATE_TAG + candidate.candidate_id)

    # --- endpoint decisions and dispatch ----------------------------------------------------

    async def _decision(self, turn: _Turn, decision: object) -> None:
        if isinstance(decision, Pending):
            self._set_state("END_PENDING")
        elif isinstance(decision, Revoked):
            self._set_state("LISTENING")
        elif isinstance(decision, Close):
            await self._finish_turn(turn, decision.reason)
        elif isinstance(decision, Commit):
            await self._commit(turn, decision)

    async def _commit(self, turn: _Turn, commit: Commit) -> None:
        timeline = self._timeline(turn.runtime)
        if timeline is None or not timeline.mic.covers(commit.start, commit.end):
            await self._finish_turn(turn, "interrupted")
            return
        turn.commit = commit
        turn.coverage = (turn.runtime.echo_known / turn.runtime.echo_total) if turn.runtime.echo_total else None
        self._set_state("COMMITTED")
        pcm = timeline.mic.read(commit.start, commit.end)
        turn.wake_clip = self._wake_clip(turn, timeline)
        turn.timings["audio_ms"] = round((commit.end - commit.start) * 1000 / SAMPLE_RATE)
        await self._close_uplink(turn.runtime.lease_id, "committed")
        self._release_runtime(turn.runtime.lease_id)
        turn.task = self._spawn(self._run_committed(turn, pcm))

    def _current(self, turn: _Turn) -> bool:
        return turn.terminal is None and self._turn is turn

    async def _run_committed(self, turn: _Turn, pcm: np.ndarray) -> None:
        """§16.7 voice turn order after commit: local command, STT, route, intent→TTS. Never resubmits."""
        commit = turn.commit
        if commit.redecode_required:
            try:
                tokens, seconds = await self.deps.worker.decode_span(pcm)
            except SpeechWorkerError:
                if self._current(turn):
                    await self._finish_turn(turn, "interrupted")
                return
            if not self._current(turn):
                return
            if redecode_differs(commit.text, redecoded_command(turn.utterance.spec, tokens, seconds)):
                await self._finish_turn(turn, "retry")
                return
        if commit.local_action is not None:
            await self._run_local(turn, commit.local_action)
            return
        self._set_state("THINKING")
        cfg = self.deps.config() or {}
        loop = asyncio.get_running_loop()
        turn.stt_copy = await loop.run_in_executor(
            None, lambda: stt_copy(pcm, gain=asr_gain_for(turn.wake_db), ns=bool(cfg.get("nsAsr"))))
        if not self._current(turn):
            return
        if turn.trigger == "ha_reply":
            turn.outcome = "ha"
            await self._consume_run(turn, self.deps.esphome_reply(turn.stt_copy))
            return
        try:
            pipeline = await self.deps.pipeline_id()
            started = time.monotonic()
            final = await self.deps.ha.run_stt(pipeline, self.deps.ha_device_id(), turn.stt_copy)
        except (HaUnavailable, HaError) as exc:
            log.warning("[%s] HA STT failed: %s", self.device_id, exc)
            if self._current(turn):
                await self._finish_turn(turn, "stt_failed")
            return
        if not self._current(turn):
            return
        turn.timings["stt_ms"] = round((time.monotonic() - started) * 1000)
        turn.stt_raw = final
        turn.stt_text = final_command_text(
            final, wake_initiated=turn.trigger == "wake", streaming_window=commit.wake_window,
            wake_phrase=turn.model.wake_phrase if turn.model is not None else "",
        ).strip()
        if not turn.stt_text:
            await self._finish_turn(turn, "empty_transcript")
            return
        exp = turn.expectation
        if exp is not None and exp.choices:
            await self._answer_choice(turn, exp)
            return
        alarm = echomuse_grammar.parse_alarm(turn.stt_text)
        if alarm is not None:
            await self._alarm(turn, alarm)
            return
        turn.outcome = "ha"
        self.awaiting_intent = True
        started = time.monotonic()
        try:
            run = await self.deps.ha.run_intent_tts(pipeline, self.deps.ha_device_id(), turn.stt_text,
                                                    turn.conversation_id)
        except (HaUnavailable, HaError) as exc:
            # Nothing was dispatched, so nothing can have executed.
            log.warning("[%s] HA intent run not sent: %s", self.device_id, exc)
            self.awaiting_intent = False
            if self._current(turn):
                await self._finish_turn(turn, "ha_error")
            return
        turn.run = run
        await self._consume_run(turn, run, sent=started)

    async def _consume_run(self, turn: _Turn, events: AsyncIterator[object], *,
                           sent: float | None = None) -> None:
        """Intent result, response audio, and continuation for one dispatched run (§7, §16.7)."""
        sent = time.monotonic() if sent is None else sent
        iterator = events.__aiter__()
        intent: IntentEnded | None = None
        tts: TtsReady | None = None
        playback: asyncio.Task | None = None
        try:
            async with asyncio.timeout(HA_TIMEOUT_S):
                while intent is None:
                    event = await iterator.__anext__()
                    if not self._current(turn):
                        return
                    if isinstance(event, SttEnded):
                        # ESPHome reply path: HA ran STT inside this run.
                        turn.stt_raw = turn.stt_text = event.text
                        turn.timings["stt_ms"] = round((time.monotonic() - sent) * 1000)
                        sent = time.monotonic()
                    elif isinstance(event, IntentEnded):
                        intent = event
                    elif isinstance(event, TtsReady):
                        tts = event
                        turn.timings["tts_url_ms"] = round((time.monotonic() - sent) * 1000)
                        if event.streamed and playback is None:
                            playback = self._spawn(self._play_response(turn, event.url, RESPONSE_TOTAL_S))
                    elif isinstance(event, RunLost):
                        await self._finish_turn(turn, "outcome_unknown")
                        return
                    elif isinstance(event, (RunFailed, RunRejected, RunEnded)):
                        await self._finish_turn(turn, "ha_error")
                        return
        except TimeoutError:
            await self._close_events(events)
            if self._current(turn):
                await self._finish_turn(turn, "ha_timeout")
            return
        except StopAsyncIteration:
            if self._current(turn):
                await self._finish_turn(turn, "outcome_unknown")
            return
        self.awaiting_intent = False
        intent_at = asyncio.get_running_loop().time()
        turn.timings["intent_ms"] = round((time.monotonic() - sent) * 1000)
        turn.response_text = intent.speech or None
        turn.response_type = intent.response_type
        turn.intent_local = intent.processed_locally
        turn.conversation_id = intent.conversation_id or turn.conversation_id
        if intent.continue_conversation and self._chain_allowed(turn):
            turn.next_expectation = self._new_expectation("reply", turn.conversation_id, turn.turn_id,
                                                          previous=self._chain_parent(turn))
        if tts is None and intent.speech:
            try:
                async with asyncio.timeout(RESPONSE_START_S):
                    while tts is None:
                        event = await iterator.__anext__()
                        if isinstance(event, TtsReady):
                            tts = event
                            turn.timings["tts_url_ms"] = round((time.monotonic() - sent) * 1000)
                        elif isinstance(event, RunLost):
                            if self._current(turn):
                                await self._finish_turn(turn, "outcome_unknown")
                            return
                        elif isinstance(event, (RunFailed, RunRejected)):
                            if self._current(turn):
                                await self._finish_turn(turn, "ha_error")
                            return
                        elif isinstance(event, RunEnded):
                            break
            except (TimeoutError, StopAsyncIteration):
                pass
            if tts is None:
                await self._close_events(events)
                if self._current(turn):
                    await self._finish_turn(turn, "response_timeout")
                return
        if not self._current(turn):
            return
        if tts is not None and playback is None:
            remaining = max(0.0, RESPONSE_START_S - (asyncio.get_running_loop().time() - intent_at))
            playback = self._spawn(self._play_response(turn, tts.url, remaining))
        reason = await playback if playback is not None else "drained"
        if not self._current(turn):
            return
        if reason not in ("drained", "cancelled"):
            await self._finish_turn(turn, "response_timeout")
            return
        exp = turn.next_expectation
        if exp is None or reason == "cancelled":
            await self._finish_turn(turn, "completed", feedback=False)
            return
        await self._continue(turn, exp)

    @staticmethod
    async def _close_events(events: object) -> None:
        if hasattr(events, "abandon"):
            events.abandon()
        elif hasattr(events, "aclose"):
            try:
                await events.aclose()
            except Exception:
                pass

    async def _play_response(self, turn: _Turn, url: str, start_timeout: float) -> str:
        """The turn's response as dialog output; the reply lease opens when its audio starts (§9.1)."""
        async def started() -> None:
            self._set_state("SPEAKING")
            exp = turn.next_expectation
            if exp is not None:
                exp.prompt = turn.playback
                self._expectation = exp       # early answers are watched while the prompt plays
                await self._open_expectation_lease(exp)

        def bind(playback: "Playback") -> None:
            turn.playback = playback

        started_at = time.monotonic()
        reason = await self._play_dialog(turn.turn_id, turn.generation, url, bind=bind, started=started,
                                         start_timeout=start_timeout, fence=lambda: self._current(turn))
        turn.timings["playback_ms"] = round((time.monotonic() - started_at) * 1000)
        return reason

    async def _continue(self, turn: _Turn, exp: _Expectation) -> None:
        """Turn the drained response into a live reply expectation, then close the turn."""
        self._expectation = exp
        if exp.runtime is None:
            await self._open_expectation_lease(exp)
        if exp.runtime is None:
            self._expectation = None
            await self._release_owner(exp.owner)
            await self._finish_turn(turn, "completed", feedback=False)
            return
        await self._finish_turn(turn, "completed", feedback=False)
        await self._begin_window(exp)

    async def _begin_window(self, exp: _Expectation) -> None:
        """After the guarded drain (or without prompt audio): 7 s window and onset scan (§16.2, §16.6)."""
        if exp is not self._expectation or exp.runtime is None:
            return
        exp.deadline = asyncio.get_running_loop().time() + REPLY_S
        self._set_state("EXPECT_REPLY")
        timeline = self._timeline(exp.runtime)
        drain = timeline.mic.frontier if timeline is not None else None
        if drain is None:
            exp.drain_pending = True
            return
        onset = exp.watch.drained(drain, exp.runtime.assembler.history)
        if onset is not None:
            await self._start_reply(exp, onset)

    # --- local commands, alarms, clarifications ------------------------------------------------

    async def _run_local(self, turn: _Turn, action: str) -> None:
        """§6.3 step 3: execute on the captured occurrence without HA."""
        turn.outcome = "local_command"
        turn.stt_text = turn.commit.text
        if action == "stop":        # dialog context: the wake already cancelled the output
            await self._finish_turn(turn, "completed", feedback=False)
            return
        link = self.link
        if link is None:
            await self._finish_turn(turn, "session_lost")
            return
        body = {
            "op_id": str(uuid.uuid5(uuid.NAMESPACE_URL, f"{turn.turn_id}|{action}")),
            "target_id": turn.context_target,
            "action": action,
            "source": "voice",
        }
        try:
            ack = await link.request("alert.act", body, generation=turn.generation, timeout=LOCAL_ACT_TIMEOUT_S)
        except Exception as exc:
            log.warning("[%s] alert.act unanswered: %s", self.device_id, exc)
            if self._current(turn):
                await self._finish_turn(turn, "outcome_unknown")
            return
        if ack.get("status") == "rejected":
            # The captured occurrence already ended (queue moved on or it expired).
            log.info("[%s] alert.act %s rejected: %s", self.device_id, action, ack.get("error"))
        if self._current(turn):
            await self._finish_turn(turn, "completed", feedback=False)

    async def _alarm(self, turn: _Turn, parsed: AlarmParse) -> None:
        """§10.5 voice alarm through the alert engine; op_id = UUIDv5 of the turn."""
        turn.outcome = "alarm"
        if parsed.missing == "ampm":
            exp = self._new_expectation("reply", turn.conversation_id, turn.turn_id,
                                        previous=self._chain_parent(turn), choices=AMPM_CHOICES,
                                        pending={"parse": parsed, "turn_id": turn.turn_id})
            turn.outcome = "clarification"
            turn.response_text = LINE_AMPM
            await self._finish_turn(turn, "completed", feedback=False)
            self._spawn(self._prompt(exp, LINE_AMPM))
            return
        await self._apply_alarm(turn, parsed, turn.turn_id)

    async def _apply_alarm(self, turn: _Turn, parsed: AlarmParse, origin_turn_id: str) -> None:
        op_id = str(uuid.uuid5(uuid.NAMESPACE_URL, origin_turn_id))
        alerts = self.deps.alerts
        clock = None if parsed.hour24 is None else f"{parsed.hour24:02d}:{parsed.minute or 0:02d}"
        try:
            if parsed.action == "set":
                days = sorted(parsed.days) if isinstance(parsed.days, frozenset) else ()
                on_date = await self._alarm_date(parsed.days)
                result = await alerts.set_alarm(self.device_id, clock, days, on_date=on_date,
                                                source="voice", op_id=op_id)
            elif parsed.action == "cancel_all":
                result = await alerts.cancel_alarm(self.device_id, all_alarms=True, source="voice", op_id=op_id)
            elif clock is not None:
                result = await alerts.cancel_alarm(self.device_id, time=clock, source="voice", op_id=op_id)
            else:
                result = await self._cancel_only_alarm(op_id)
        except HaUnavailable:
            result = {"ok": False, "error": "Home Assistant is unreachable"}
        if not self._current(turn):
            return
        if not result.get("ok") and result.get("error") not in ("no matching alarm", "which alarm"):
            log.warning("[%s] voice alarm failed: %s", self.device_id, result.get("error"))
            await self._finish_turn(turn, "ha_error")
            return
        line = self._alarm_line(parsed, result)
        turn.response_text = line
        await self._finish_turn(turn, "completed", feedback=False)
        self._spawn(self._speak(line, turn.turn_id, turn.generation))

    async def _cancel_only_alarm(self, op_id: str) -> dict:
        """"cancel my alarm" names no time: it cancels only when exactly one alarm exists."""
        listed = self.deps.alerts.list_alarms(self.device_id)
        alarms = listed.get("alarms") or []
        schedules = {a["schedule_id"]: a for a in alarms}
        if not listed.get("ok"):
            return listed
        if len(schedules) != 1:
            return {"ok": False, "error": "which alarm" if schedules else "no matching alarm"}
        due = next(iter(schedules.values()))["due"]
        return await self.deps.alerts.cancel_alarm(self.device_id, time=datetime.fromisoformat(due).strftime("%H:%M"),
                                                   source="voice", op_id=op_id)

    async def _alarm_date(self, days: object):
        if days not in ("today", "tomorrow"):
            return None
        today = datetime.now(ZoneInfo(await self.deps.ha.time_zone())).date()
        return today if days == "today" else today + timedelta(days=1)

    @staticmethod
    def _alarm_line(parsed: AlarmParse, result: dict) -> str:
        if not result.get("ok"):
            return "Which alarm? Say its time." if result.get("error") == "which alarm" \
                else "I couldn't find that alarm."
        if parsed.action == "set":
            hour = parsed.hour24 % 12 or 12
            minute = f":{parsed.minute:02d}" if parsed.minute else ""
            return f"Alarm set for {hour}{minute} {'AM' if parsed.hour24 < 12 else 'PM'}."
        count = len(result.get("cancelled") or [])
        return "Alarm cancelled." if count <= 1 else f"Cancelled {count} alarms."

    async def _answer_choice(self, turn: _Turn, exp: _Expectation) -> None:
        """§16.6 Choices: select, re-prompt once, or leave it."""
        value = echomuse_grammar.match_choice(turn.stt_text, exp.choices)
        pending = exp.pending_operation or {}
        parsed = pending.get("parse")
        if value is not None and isinstance(parsed, AlarmParse):
            hour = (parsed.hour12 or 12) % 12 + (12 if value == "pm" else 0)
            completed = AlarmParse(parsed.action, hour, parsed.minute or 0, parsed.days, None,
                                   parsed.hour12, value)
            turn.outcome = "alarm"
            await self._apply_alarm(turn, completed, str(pending.get("turn_id")))
            return
        turn.outcome = "clarification"
        if not exp.reprompted:
            retry = self._new_expectation("reply", exp.conversation_id, exp.originating_turn_id,
                                          previous=exp, choices=exp.choices, pending=exp.pending_operation)
            retry.reprompted = True
            retry.chain_count = exp.chain_count
            turn.response_text = LINE_AMPM_AGAIN
            await self._finish_turn(turn, "completed", feedback=False)
            self._spawn(self._prompt(retry, LINE_AMPM_AGAIN))
            return
        turn.response_text = LINE_LEFT_IT
        await self._finish_turn(turn, "completed", feedback=False)
        self._spawn(self._speak(LINE_LEFT_IT, turn.turn_id, turn.generation))

    # --- expectations ---------------------------------------------------------------------------

    def _chain_parent(self, turn: _Turn) -> _Expectation | None:
        """Only no-wake replies count toward the chain (§9.1); a wake or button starts a new chain."""
        return turn.expectation if turn.trigger in ("reply", "ha_reply") else None

    def _chain_allowed(self, turn: _Turn) -> bool:
        parent = self._chain_parent(turn)
        if parent is None:
            return True
        now = asyncio.get_running_loop().time()
        return parent.chain_count < REPLY_CHAIN_MAX and now - parent.chain_started < REPLY_CHAIN_S

    def _new_expectation(self, source: str, conversation_id: str | None, originating_turn_id: str | None,
                         *, previous: _Expectation | None = None, choices: tuple[Choice, ...] | None = None,
                         pending: dict | None = None) -> _Expectation:
        now = asyncio.get_running_loop().time()
        return _Expectation(
            expectation_id=str(uuid.uuid4()), owner=str(uuid.uuid4()), generation=self._generation,
            source=source, conversation_id=conversation_id, originating_turn_id=originating_turn_id,
            chain_started=previous.chain_started if previous is not None else now,
            chain_count=previous.chain_count + 1 if previous is not None else 1,
            choices=choices, pending_operation=pending,
        )

    async def _open_expectation_lease(self, exp: _Expectation) -> None:
        """The `reply` lease: live mic, cells, and reference, with dialog-input focus (§9.1)."""
        if exp.runtime is not None or self.uplink is None or self.link is None or self._muted:
            return
        epoch = self.uplink.streams.current("mic")
        if epoch is None:
            return
        lease_id = str(uuid.uuid4())
        await self._lease_message(self.uplink.open(lease_id, LeaseReason.REPLY, exp.owner, epoch,
                                                   {"mic": None, "cells": None, "reference": None}))
        model = self.deps.registry.for_config(self.deps.config())
        await self.deps.worker.load_wake_graph(model.graph_sha256)
        self.deps.worker.open_lease(self.device_id, epoch, lease_id, model.graph_sha256)
        runtime = self._new_runtime(lease_id)
        runtime.assembler.require_echo_from(0)
        exp.runtime = runtime
        await self._acquire_focus(exp.owner, "dialog_input", exp.generation)

    async def _prompt(self, exp: _Expectation, text: str) -> None:
        """An EchoMuse question: TTS-only prompt, then its reply window."""
        if self.link is None:
            return
        self._expectation = exp
        self._set_state("SPEAKING")
        url = await self._tts(text)
        if exp is not self._expectation:
            return
        if url is None:
            self._cue(ERROR_CUE)
            await self._end_expectation("ha_error")
            return

        async def started() -> None:
            await self._open_expectation_lease(exp)

        def bind(playback: "Playback") -> None:
            exp.prompt = playback

        reason = await self._play_dialog(exp.owner, exp.generation, url, bind=bind, started=started,
                                         fence=lambda: exp is self._expectation)
        if exp is not self._expectation:
            return
        if reason != "drained" or exp.runtime is None:
            # A failed prompt invalidates its expectation (§9.1).
            await self._end_expectation("response_timeout" if reason != "cancelled" else "interrupted")
            return
        await self._begin_window(exp)

    async def _end_expectation(self, reason: str) -> None:
        """Close the live expectation without a turn; a silent window never dispatches (§9.1)."""
        if self._expectation is None:
            return
        await self._drop_expectation()
        self._emit(ActorEvent("terminal", self.state, reason, self._dialog_active))
        if self._turn is None:
            self._set_state("IDLE")

    async def _drop_expectation(self) -> None:
        exp, self._expectation = self._expectation, None
        if exp is None:
            return
        if exp.prompt is not None and not exp.prompt.done:
            await exp.prompt.cancel("expectation_closed")
        if exp.runtime is not None:
            await self._close_uplink(exp.runtime.lease_id, "closed")
            self._release_runtime(exp.runtime.lease_id)
            exp.runtime = None
        await self._release_owner(exp.owner)

    # --- announcements ------------------------------------------------------------------------

    async def _announcement(self, item: _Announcement) -> None:
        """§16.7: queued behind dialog output; `start_conversation` opens an ESPHome-path reply expectation."""
        try:
            async with self._announcements:
                while self._dialog_playback is not None and not self._dialog_playback.done:
                    await asyncio.wait({self._dialog_playback.finished})
                if self.render is None:
                    return
                self._generation += 1
                owner, generation = str(uuid.uuid4()), self._generation
                if item.preannounce_url:
                    if await self._play_dialog(owner, generation, item.preannounce_url,
                                               announcement=True) != "drained":
                        return
                if not item.start_conversation:
                    await self._play_dialog(owner, generation, item.url, announcement=True)
                    return
                exp = self._new_expectation("ha_reply", None, None)
                self._expectation = exp

                async def started() -> None:
                    await self._open_expectation_lease(exp)

                def bind(playback: "Playback") -> None:
                    exp.prompt = playback

                reason = await self._play_dialog(owner, generation, item.url, announcement=True, bind=bind,
                                                 started=started, fence=lambda: exp is self._expectation)
                if exp is not self._expectation:
                    return
                if reason != "drained" or exp.runtime is None:
                    await self._end_expectation("interrupted")
                    return
                await self._begin_window(exp)
        finally:
            if not item.future.done():
                item.future.set_result(None)

    # --- dialog output -------------------------------------------------------------------------

    async def _play_dialog(self, owner: str, generation: int, url: str, *, announcement: bool = False,
                           bind: Callable[["Playback"], None] | None = None,
                           started: Callable[[], Awaitable[None]] | None = None,
                           start_timeout: float = RESPONSE_START_S,
                           fence: Callable[[], bool] = lambda: True) -> str:
        """Play `url` as dialog output under a dialog_output lease for `owner`.

        Returns the finish reason (`drained`, `cancelled`, `failed`, `underrun`),
        `response_timeout` for §7's start/progress/length limits, or `fenced`.
        """
        render = self.render
        if render is None:
            return "failed"
        await self._acquire_focus(owner, "dialog_output", generation)
        try:
            try:
                playback = await render.play_stream("dialog_output", _stream_url(url), generation=generation,
                                                    announcement=announcement)
            except Exception as exc:
                log.warning("[%s] dialog output failed to start: %s", self.device_id, exc)
                return "failed"
            self._dialog_playback = playback
            if bind is not None:
                bind(playback)
            began = time.monotonic()
            try:
                await asyncio.wait_for(asyncio.shield(playback.started), start_timeout)
            except TimeoutError:
                await playback.cancel("response_timeout")
                return "response_timeout"
            except Exception:
                return playback.finished.result().get("reason", "failed") if playback.done else "failed"
            if not fence():
                return "fenced"
            if started is not None:
                await started()
            last, last_at = playback.last_progress, time.monotonic()
            while not playback.done:
                await asyncio.wait({playback.finished}, timeout=0.05)
                if not fence():
                    return "fenced"
                now = time.monotonic()
                if playback.last_progress is not last:
                    last, last_at = playback.last_progress, now
                elif not playback.done and now - last_at >= RESPONSE_STALL_S:
                    await playback.cancel("response_timeout")
                    return "response_timeout"
                if not playback.done and now - began >= RESPONSE_TOTAL_S:
                    await playback.cancel("response_timeout")
                    return "response_timeout"
            return playback.finished.result().get("reason", "failed")
        finally:
            await self._release_owner(owner, only="dialog_output")

    async def _tts(self, text: str) -> str | None:
        """TTS-only run with the pipeline's engine and voice (§16.7 step 4)."""
        try:
            pipeline = await self.deps.pipeline_id()
            return await self.deps.ha.run_tts(pipeline, text, self.deps.ha_device_id())
        except (HaUnavailable, HaError) as exc:
            log.warning("[%s] TTS for %r failed: %s", self.device_id, text, exc)
            return None

    async def _speak(self, text: str, owner: str, generation: int) -> None:
        """One spoken terminal/confirmation line; if its TTS fails, the error cue alone (§7)."""
        if self.link is None:
            return
        url = await self._tts(text)
        if url is None or generation != self._generation:
            if url is None:
                self._cue(ERROR_CUE)
            return
        reason = await self._play_dialog(owner, generation, url, fence=lambda: generation == self._generation)
        if reason not in ("drained", "cancelled", "fenced"):
            self._cue(ERROR_CUE)

    # --- close, feedback, persistence ------------------------------------------------------------

    async def _finish_turn(self, turn: _Turn, reason: str, *, feedback: bool = True) -> None:
        """Enter CLOSING exactly once: fence, release owned leases, persist, give §7 feedback."""
        if turn.terminal is not None:
            return
        turn.terminal = reason
        if turn.run is not None and hasattr(turn.run, "abandon"):
            turn.run.abandon()
        if turn.task is not None and turn.task is not asyncio.current_task() and not turn.task.done():
            turn.task.cancel()
        if turn.playback is not None and not turn.playback.done:
            await turn.playback.cancel(reason)
        timeline = self._timeline(turn.runtime)
        if timeline is not None and turn.wake_clip is None:
            turn.wake_clip = self._wake_clip(turn, timeline)
        if turn.coverage is None and turn.runtime.echo_total:
            turn.coverage = turn.runtime.echo_known / turn.runtime.echo_total
        await self._close_uplink(turn.runtime.lease_id, "closed")
        self._release_runtime(turn.runtime.lease_id)
        next_exp = turn.next_expectation
        if next_exp is not None and next_exp is self._expectation and reason != "completed":
            await self._drop_expectation()      # a failed prompt invalidates its expectation (§9.1)
        elif next_exp is not None and next_exp is not self._expectation:
            if next_exp.runtime is not None:
                await self._close_uplink(next_exp.runtime.lease_id, "closed")
                self._release_runtime(next_exp.runtime.lease_id)
                next_exp.runtime = None
            await self._release_owner(next_exp.owner)
        await self._release_owner(turn.turn_id)
        self.awaiting_intent = False
        if turn.trigger == "wake" and reason != "superseded":
            self.deps.arbiter.release(self.device_id)
        if self._turn is turn:
            self._turn = None
        self._set_state("CLOSING")
        self._emit(ActorEvent("terminal", self.state, reason, self._dialog_active))
        self._persists.add(self._spawn(self._persist(turn)))
        if feedback:
            self._feedback(turn, reason)
        if self._turn is None:
            self._set_state("EXPECT_REPLY" if self._expectation is not None else "IDLE")

    def _feedback(self, turn: _Turn, reason: str) -> None:
        """Exactly the §7 terminal feedback table."""
        if reason == "no_input":
            self._cue(NO_SPEECH_CUE)
        elif reason in ERROR_CUE_REASONS:
            self._cue(ERROR_CUE)
        elif reason in ERROR_LINE_REASONS:
            self._cue(ERROR_CUE)
            self._spawn(self._speak(LINE_ERROR, turn.turn_id, turn.generation))
        elif reason == "too_long":
            self._spawn(self._speak(LINE_TOO_LONG, turn.turn_id, turn.generation))
        elif reason == "outcome_unknown":
            self._spawn(self._speak(LINE_UNKNOWN, turn.turn_id, turn.generation))
        elif reason in CLARIFY_REASONS:
            if self._chain_allowed(turn):
                exp = self._new_expectation("reply", turn.conversation_id, turn.turn_id,
                                            previous=self._chain_parent(turn))
                self._spawn(self._prompt(exp, LINE_SORRY))
            else:
                self._spawn(self._speak(LINE_SORRY, turn.turn_id, turn.generation))

    def _wake_clip(self, turn: _Turn, timeline: LeaseTimeline) -> bytes | None:
        """Accepted candidate support −300 ms … support_end, canonical PCM."""
        candidate = turn.candidate
        if candidate is None:
            return None
        a = max(timeline.mic.floor, candidate.support_start - PREROLL)
        b = candidate.support_end or candidate.open_sample
        if b <= a or not timeline.mic.covers(a, b):
            return None
        return timeline.mic.read(a, b).tobytes()

    async def _persist(self, turn: _Turn) -> None:
        """The turn row with its §11.3 decision trace; utterance WAV and wake clip when enabled."""
        candidate, commit = turn.candidate, turn.commit
        row = {
            "ts": time.time() - (asyncio.get_running_loop().time() - turn.opened),
            "trigger": turn.trigger,
            "wake_model": turn.model.wake_phrase if turn.model is not None else None,
            "wake_score": candidate.peak if candidate is not None else None,
            "wake_threshold": candidate.threshold if candidate is not None else None,
            "outcome": turn.outcome,
            "asr_text": turn.utterance.heard or None,
            "stt_raw": turn.stt_raw,
            "stt_text": turn.stt_text,
            "response_text": turn.response_text,
            "response_type": turn.response_type,
            "intent_local": turn.intent_local,
            "total_ms": round((asyncio.get_running_loop().time() - turn.opened) * 1000),
            "stt_ms": turn.timings.get("stt_ms"),
            "intent_ms": turn.timings.get("intent_ms"),
            "tts_url_ms": turn.timings.get("tts_url_ms"),
            "playback_ms": turn.timings.get("playback_ms"),
            "audio_ms": turn.timings.get("audio_ms"),
            "endpoint_ms": (round((commit.decided_at - commit.boundary) * 1000 / SAMPLE_RATE)
                            if commit is not None else None),
            "endpoint_class": commit.completeness if commit is not None else None,
            "wake_model_sha256": turn.model.graph_sha256 if turn.model is not None else None,
            "policy_hash": self.deps.worker.policy_hash,
            "wake_attribution": None if candidate is None else ("verified" if candidate.producing_sound else "idle"),
            "reference_coverage": turn.coverage,
            "commit_route": commit.route if commit is not None else None,
            "terminal_reason": turn.terminal,
            "commit_id": commit.commit_id if commit is not None else None,
        }
        log.info("[%s] turn %s trace %s", self.device_id, turn.turn_id, json.dumps({
            "turn_id": turn.turn_id, "generation": turn.generation, "trigger": turn.trigger,
            "candidate_id": candidate.candidate_id if candidate is not None else None,
            "profile": candidate.body.get("profile") if candidate is not None else None,
            "producing_sound": candidate.producing_sound if candidate is not None else None,
            "hops": candidate.body.get("hops") if candidate is not None else None,
            "lease_id": turn.runtime.lease_id, "terminal": turn.terminal,
            "utterance": turn.utterance.trace(), **{k: v for k, v in row.items() if k != "ts"},
        }, default=str, separators=(",", ":")))
        try:
            row_id = await self.deps.persist_turn(row)
        except Exception:
            log.exception("[%s] turn row not persisted", self.device_id)
            return
        cfg = self.deps.config() or {}
        loop = asyncio.get_running_loop()
        for enabled, pcm, save, link in (
            (cfg.get("saveUtterances"), turn.stt_copy, em_recordings.save, em_db.set_turn_audio),
            (cfg.get("saveWakeClips"), turn.wake_clip, em_wakeclips.save, em_db.set_turn_wake),
        ):
            if not enabled or not pcm:
                continue
            try:
                name = await loop.run_in_executor(None, save, self.device_id, row_id, pcm)
                if name:
                    await loop.run_in_executor(None, link, row_id, name)
            except Exception:
                log.exception("[%s] turn %s audio not saved", self.device_id, row_id)

    async def _persist_candidate(self, candidate: _Candidate, reason: str) -> None:
        """A rejected candidate is a turn row with its attribution reason and no audio (§11.3)."""
        try:
            await self.deps.persist_turn({
                "ts": time.time(), "trigger": "wake", "wake_model": candidate.model.wake_phrase,
                "wake_score": candidate.peak, "wake_threshold": candidate.threshold,
                "total_ms": round((asyncio.get_running_loop().time() - candidate.received) * 1000),
                "wake_model_sha256": candidate.model.graph_sha256,
                "policy_hash": self.deps.worker.policy_hash,
                "wake_attribution": reason, "terminal_reason": reason,
            })
        except Exception:
            log.exception("[%s] rejected candidate not persisted", self.device_id)

    # --- wire helpers -----------------------------------------------------------------------------

    async def _send(self, msg_type: str, body: dict, generation: int) -> None:
        link = self.link
        if link is None or getattr(link, "closed", False):
            return
        try:
            await link.send(msg_type, body, generation=generation)
        except Exception as exc:
            log.debug("[%s] %s not sent: %s", self.device_id, msg_type, exc)

    async def _lease_message(self, message: LeaseMessage) -> None:
        await self._send(message.type, message.body, message.generation)

    async def _close_uplink(self, lease_id: str, reason: str) -> None:
        if self.uplink is None:
            return
        lease = self.uplink.leases.get(lease_id)
        if lease is not None and lease.ended is None:
            await self._lease_message(self.uplink.leases.close(lease_id, reason))

    async def _acquire_focus(self, owner: str, focus: str, generation: int) -> None:
        if any(f.owner == owner and f.focus == focus for f in self._focus.values()):
            return
        lease_id = str(uuid.uuid4())
        self._focus[lease_id] = _Focus(lease_id, owner, focus, generation, asyncio.get_running_loop().time())
        await self._send("focus.acquire", {"lease_id": lease_id, "owner": owner, "focus": focus,
                                           "ttl_ms": FOCUS_TTL_MS}, generation)
        self._update_dialog_active()

    async def _release_owner(self, owner: str, only: str | None = None) -> None:
        """Release exactly the named owner's leases (§16.1: never a blanket cleanup)."""
        for lease_id, focus in list(self._focus.items()):
            if focus.owner == owner and (only is None or focus.focus == only):
                del self._focus[lease_id]
                await self._send("focus.release", {"lease_id": lease_id}, focus.generation)
        self._update_dialog_active()

    # --- events -------------------------------------------------------------------------------------

    def _update_dialog_active(self) -> None:
        active = bool(self._focus)
        if active != self._dialog_active:
            self._dialog_active = active
            self._emit(ActorEvent("dialog_focus", self.state, None, active))

    def _set_state(self, state: str) -> None:
        if state not in STATES:
            raise ValueError(f"invalid actor state {state!r}")
        if state != self.state:
            self.state = state
            self._emit(ActorEvent("state", state, None, self._dialog_active))

    def _cue(self, cue: str) -> None:
        self._emit(ActorEvent("cue", self.state, cue, self._dialog_active))

    def _emit(self, event: ActorEvent) -> None:
        for listener in list(self._listeners):
            try:
                result = listener(event)
                if inspect.isawaitable(result):
                    asyncio.ensure_future(result)
            except Exception:
                log.exception("[%s] actor listener failed", self.device_id)
