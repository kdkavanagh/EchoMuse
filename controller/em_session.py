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
import enum
import inspect
import json
import logging
import time
import uuid
from collections.abc import AsyncGenerator, Mapping
from dataclasses import asdict, dataclass, field, replace
from datetime import date, datetime, timedelta, timezone
from typing import (TYPE_CHECKING, AsyncIterator, Awaitable, Callable, Coroutine, Protocol, TypedDict,
                    assert_never, runtime_checkable)
from zoneinfo import ZoneInfo

import numpy as np

import echomuse_grammar
import em_alert_speech
import em_config_sections
import em_db
import em_recordings
import em_timers
import em_wakeclips
import em_wake_rules
from echomuse_grammar import (
    AMPM_CHOICES,
    AlarmAction,
    AlarmParse,
    AlarmQuery,
    Choice,
    CommandContext,
    ContextKind,
    GrammarClass,
    LocalAction,
    Meridiem,
    MissingSlot,
    RelativeDay,
    TimerCancel,
)
from em_afe import AfeEvidence, AfeSeries, TurnStage, series_range, turn_evidence, turn_series
from em_alert_wire import RingKind, Weekday
from em_alerts import ActSource, AlertResult, JournalSource
from em_attribution import (
    CELL,
    ECHO_WINDOW_CELLS,
    MAX_LAG,
    REPLY_OVERLAP,
    Coverage,
    EchoResult,
    EchoTracker,
    PlaybackVerdict,
    WakeHop,
    compare_reference,
    estimate_lag,
    reference_candidate_overlaps,
    self_playback_verdict,
    wake_trigger_sample,
)
from em_audio_timeline import (
    AFE_PERIOD_SAMPLES,
    CANDIDATE_CELLS_LEAD,
    CANDIDATE_REFERENCE_LEAD,
    AfeTimeline,
    Delivery,
    LeaseEnd,
    LeaseMessage,
    LeaseReason,
    LeaseTimeline,
    ReferenceView,
    StreamId,
    UplinkSession,
    parse_u64,
)
from em_device_link import AckStatus, CloseReason, CommandAck, Envelope, MessageType
from em_endpoint_policy import Route, recognized_completeness, recognizer_sentences, redecode_differs
from em_ha_client import (
    HaError,
    HaTimer,
    HaUnavailable,
    IntentEnded,
    RunEnded,
    RunEvent,
    RunFailed,
    RunLost,
    RunRejected,
    SttEnded,
    TtsReady,
)
from em_pause_asr import PAUSE_ASR_KEY, WyomingServer, parse_pause_asr
from em_render import FinishReason, SourceClass
from em_speech_worker import (
    VERIFICATION_LOOKAHEAD,
    AsrPayload,
    EchoPayload,
    ErrorPayload,
    Observation,
    ObservationKind,
    ReferenceScorePayload,
    SpeechWorker,
    SpeechWorkerError,
    VadPayload,
    VerificationPayload,
    VerificationResult,
)
from em_stt_copy import STT_TAIL_SILENCE, asr_gain_for, stt_copy
from em_utterance import (
    PREROLL,
    REPLY_KINDS,
    CellAssembler,
    Close,
    Commit,
    Decision,
    Pending,
    ReplyWatch,
    Revoked,
    TextSource,
    Utterance,
    UtteranceKind,
    UtteranceTrace,
    UtteranceSpec,
    pause_command,
    redecoded_command,
)
from em_wake_phrase import final_command_text
from em_wake_scorer import ReferenceCandidate

if TYPE_CHECKING:
    from em_alerts import AlertEngine
    from em_arbiter import WakeArbiter
    from em_ha_client import HaClient, Vocabulary
    from em_render import Playback, RenderClient
    from em_wake_registry import WakeModel, WakeRegistry

log = logging.getLogger("echomuse.session")

SAMPLE_RATE = 16_000
RENDER_RATE = 48_000             # dialog output PCM (em_media.RATE)

# §7 / §9 / §16.2 / §16.6 deadlines (seconds of loop time unless noted).
WAKE_VERIFY_S = 0.700            # verification deadline from receipt of wake.candidate
FOCUS_TTL_MS = 3_000             # ephemeral dialog focus; renewed every second
RENEW_S = 1.0
HA_TIMEOUT_S = 30.0              # intent run sent → intent-end
RESPONSE_START_S = 10.0          # intent-end → response audio start
RESPONSE_STALL_S = 2.0           # no output progress
RESPONSE_TOTAL_S = 120.0         # one spoken response
# A reply turn opened at the drain starts this long before its trigger: the 480 ms an answer
# may overlap the question's end, plus the pre-roll (§16.6).
REPLY_LEAD = REPLY_OVERLAP + PREROLL
# Wall-clock bound on a reply window without command speech (§16.2): the 7 s window is sample
# time, so a lease that delivers no mic, or stops, would otherwise leave the Dot listening.
REPLY_WALL_S = 10.0
REPLY_CHAIN_MAX = 5              # no-wake replies per chain
REPLY_CHAIN_S = 60.0
ACTOR_TICK_S = 0.050
LOCAL_ACT_TIMEOUT_S = 2.0
PERSIST_SHUTDOWN_S = 2.0         # shutdown waits this long for pending turn rows (§11.3)
WAKE_ARBITRATION_MS = 700.0      # default `wakeArbitrationMs`
PLAYBACK_POLL_S = 0.05           # dialog output start/progress check while it waits or plays
# HA's sentence matcher on a stable prefix (§16.6): an answer later than this could no longer
# shorten any pause, so the query stops waiting for it.
RECOGNIZE_TIMEOUT_S = 2.0

# A committed turn's response lease (§4.4) carries its AFE records, and while `saveUtterances` is on
# its mic, on through the response: at most this long after the commit, and this long past the
# turn's end, for the AEC settling after the last of the response and the audio still in flight
# when it drained.
RESPONSE_LEASE_MAX_S = 30.0
RESPONSE_LEASE_TAIL_S = 1.0

# Sample-time bounds on evidence the actor waits for.
ECHO_WAIT = SAMPLE_RATE          # reference for a cell's echo label: wait ≤1 s of mic time
ASR_STALL = 4 * SAMPLE_RATE      # utterance ASR this far behind the mic: the stream is dead

# §16.2 priority order of already-received events.
P_MUTE, P_STOP, P_LOSS, P_INTERRUPT, P_GAP, P_SPEECH, P_RESULT, P_DEADLINE = range(8)
P_CLOSE = -1


class VoicePhase(enum.StrEnum):
    """What a turn is doing, as the LED ring, the dashboard and HA's “Voice state”
    sensor show it (§11.2, §16.7)."""

    IDLE = "idle"
    LISTENING = "listening"
    THINKING = "thinking"
    SPEAKING = "speaking"


class ActorState(enum.StrEnum):
    """§7 turn states, as the dashboard and LEDs see them."""

    IDLE = "IDLE"
    ARMED = "ARMED"
    LISTENING = "LISTENING"
    END_PENDING = "END_PENDING"
    COMMITTED = "COMMITTED"
    THINKING = "THINKING"
    SPEAKING = "SPEAKING"
    CLOSING = "CLOSING"

    @property
    def phase(self) -> VoicePhase:
        match self:
            case ActorState.IDLE | ActorState.CLOSING:
                return VoicePhase.IDLE
            case ActorState.ARMED | ActorState.LISTENING | ActorState.END_PENDING:
                return VoicePhase.LISTENING
            case ActorState.COMMITTED | ActorState.THINKING:
                return VoicePhase.THINKING
            case ActorState.SPEAKING:
                return VoicePhase.SPEAKING
            case _:
                assert_never(self)


class ActorEventKind(enum.StrEnum):
    STATE = "state"
    TERMINAL = "terminal"
    CUE = "cue"
    DIALOG_FOCUS = "dialog_focus"


class Cue(enum.StrEnum):
    """§7 terminal-feedback LED cues."""

    ERROR_ANIM = "error_anim"
    NOSPEECH_ANIM = "nospeech_anim"


class TerminalReason(enum.StrEnum):
    """Why a turn, candidate, or reply expectation ended (§7; persisted as `terminal_reason`)."""

    # Normal ends without feedback.
    COMPLETED = "completed"
    SUPERSEDED = "superseded"            # a newer wake/button took over
    REPLY_TIMEOUT = "reply_timeout"
    # §7 feedback reasons.
    SPEECH_UNAVAILABLE = "speech_unavailable"
    HA_TIMEOUT = "ha_timeout"
    RESPONSE_TIMEOUT = "response_timeout"
    STT_FAILED = "stt_failed"
    HA_ERROR = "ha_error"
    EMPTY_TRANSCRIPT = "empty_transcript"
    RETRY = "retry"
    INTERRUPTED = "interrupted"
    AUDIO_OVERRUN = "audio_overrun"
    NO_INPUT = "no_input"
    TOO_LONG = "too_long"
    OUTCOME_UNKNOWN = "outcome_unknown"
    # Ends the device or the session imposed.
    MUTED = "muted"
    SESSION_LOST = "session_lost"
    ALERT_PREEMPTED = "alert_preempted"
    # Rejected wake candidates (§6.1).
    ARBITRATION_LOST = "arbitration_lost"
    VERIFIER_TIMEOUT = "verifier_timeout"
    UNVERIFIED_WAKE = "unverified_wake"
    SELF_OUTPUT = "self_output"
    ECHO_ONLY = "echo_only"


class FollowUp(enum.StrEnum):
    """What became of a turn's follow-up question (turns.continuation, §9.1), besides the
    terminal reason of the expectation (`reply_timeout`, `interrupted`, `superseded`,
    `muted`, `session_lost`, ...). `pending` goes out with the asking turn's row and is
    replaced by exactly one final value."""

    PENDING = "pending"
    ANSWERED = "answered"
    WAKE = "wake"
    PROMPT_CANCELLED = "prompt_cancelled"
    PROMPT_FAILED = "prompt_failed"
    NO_MIC = "no_mic"
    CHAIN_LIMIT = "chain_limit"


Continuation = FollowUp | TerminalReason


class TurnOutcome(enum.StrEnum):
    """Which handler took the request (turns.outcome)."""

    HA = "ha"
    LOCAL_COMMAND = "local_command"
    ALARM = "alarm"
    ALARM_QUERY = "alarm_query"
    TIMER = "timer"
    CLARIFICATION = "clarification"


class WakeAttribution(enum.StrEnum):
    """turns.wake_attribution of an accepted wake; a rejected candidate records its reason."""

    VERIFIED = "verified"
    IDLE = "idle"


class CandidateRefusal(enum.StrEnum):
    """`command.ack` error refusing a `wake.candidate`."""

    MALFORMED = "malformed"
    DIAGNOSTIC = "diagnostic"
    SPEECH_UNAVAILABLE = "speech_unavailable"
    MUTED = "muted"
    CANDIDATE_PENDING = "candidate_pending"


class Focus(enum.StrEnum):
    """`focus.acquire` focus (WIRE §4.3)."""

    DIALOG_INPUT = "dialog_input"
    DIALOG_OUTPUT = "dialog_output"


class DialogEnd(enum.StrEnum):
    """How dialog output ended when the controller stopped it (besides the device's `FinishReason`)."""

    RESPONSE_TIMEOUT = "response_timeout"    # §7 start/progress/length limits
    FENCED = "fenced"                        # the turn or expectation it served is no longer current


PlaybackEnd = FinishReason | DialogEnd

# Terminal reasons of a turn whose spoken question never finished.
FOLLOWUP_PROMPT_FAILED = frozenset({TerminalReason.RESPONSE_TIMEOUT, TerminalReason.HA_ERROR,
                                    TerminalReason.OUTCOME_UNKNOWN})

# SPEC §7 terminal reasons with feedback. `completed`, `reply_timeout` and
# `superseded` (a newer wake/button took over) are normal ends without feedback.
ERROR_LINE_REASONS = frozenset({TerminalReason.SPEECH_UNAVAILABLE, TerminalReason.HA_TIMEOUT,
                                TerminalReason.RESPONSE_TIMEOUT, TerminalReason.STT_FAILED,
                                TerminalReason.HA_ERROR})
CLARIFY_REASONS = frozenset({TerminalReason.EMPTY_TRANSCRIPT, TerminalReason.RETRY})
ERROR_CUE_REASONS = frozenset({TerminalReason.INTERRUPTED, TerminalReason.AUDIO_OVERRUN})

# A lease the device ended under an open utterance ends the turn with this reason.
_LEASE_END_TERMINAL = {LeaseEnd.OVERRUN: TerminalReason.AUDIO_OVERRUN, LeaseEnd.MUTE: TerminalReason.MUTED,
                       LeaseEnd.SESSION: TerminalReason.SESSION_LOST}
_VERDICT_TERMINAL = {PlaybackVerdict.SELF_OUTPUT: TerminalReason.SELF_OUTPUT,
                     PlaybackVerdict.ECHO_ONLY: TerminalReason.ECHO_ONLY}
_MESSAGE_PRIORITY = {
    MessageType.PRIVACY_CHANGED: P_MUTE,
    MessageType.UPLINK_ENDED: P_LOSS,
    MessageType.WAKE_CANDIDATE: P_SPEECH,
    MessageType.WAKE_CANDIDATE_END: P_SPEECH,
    MessageType.ALERT_STATE: P_RESULT,
}

LINE_SORRY = "Sorry, I didn't catch that."
LINE_TOO_LONG = "That was too long. Try a shorter request."
LINE_ERROR = "Something went wrong."
LINE_UNKNOWN = "I'm not sure that worked."
LINE_AMPM = "AM or PM?"
LINE_AMPM_AGAIN = "Please say AM or PM."
LINE_LEFT_IT = "Okay, I left it."

ECHO_TAG = "echo"                # submit_echo tag for per-cell labels
CANDIDATE_TAG = "candidate:"     # submit_echo tag prefix for a candidate comparison
WAKE_CHIME = "builtin:wake_chime"


def _stream_url(url: str) -> AsyncIterator[np.ndarray]:
    """48 kHz mono int16 blocks of an HA media URL (em_media, imported lazily: it spawns ffmpeg)."""
    from em_media import stream_url

    return stream_url(url)


class _Link(Protocol):
    """What the actor uses of `em_device_link.DeviceLink`."""

    closed: bool

    async def send(self, msg_type: MessageType, body: Mapping[str, object], *, generation: int = 0) -> str: ...
    async def request(self, msg_type: MessageType, body: Mapping[str, object], *, generation: int = 0,
                      timeout: float = 2.0) -> CommandAck: ...
    async def ack(self, message_id: str, status: AckStatus, error: str | None = None) -> None: ...


@runtime_checkable
class _Abandonable(Protocol):
    """A dispatched HA run (`em_ha_client.PipelineRun`): `abandon()` fences its events."""

    def abandon(self) -> None: ...


class TurnRow(TypedDict, total=False):
    """One `turns` row (em_db `insert_turn`); a rejected candidate writes only some keys."""

    ts: float
    trigger: UtteranceKind
    wake_model: str | None
    wake_score: float | None
    wake_threshold: float | None
    outcome: TurnOutcome | None
    asr_text: str | None
    stt_raw: str | None
    stt_text: str | None
    response_text: str | None
    response_type: str | None
    intent_local: bool | None
    total_ms: int
    stt_ms: int | None
    intent_ms: int | None
    tts_url_ms: int | None
    playback_ms: int | None
    playback_reason: PlaybackEnd | None
    audio_ms: int | None
    endpoint_ms: int | None
    endpoint_class: GrammarClass | None
    wake_model_sha256: str | None
    policy_hash: str | None
    wake_attribution: WakeAttribution | TerminalReason | None
    reference_coverage: float | None
    commit_route: Route | None
    terminal_reason: TerminalReason | None
    commit_id: str | None
    turn_uuid: str
    conversation_id: str | None
    reply_to: str | None
    continuation: Continuation | None
    first_audio_ms: int | None
    response_latency_ms: int | None
    decision_trace: str | None
    afe_evidence: str | None          # AfeEvidence JSON; None: the device lacks afe_metadata_v1
    afe_series: str | None            # AfeSeries JSON; None: unavailable or no data


class _AlertAct(TypedDict):
    """`alert.act` body (WIRE §4.7)."""

    op_id: str
    target_id: str | None
    action: LocalAction
    source: ActSource


@dataclass
class ActorDeps:
    ha: HaClient
    worker: SpeechWorker
    registry: WakeRegistry
    alerts: AlertEngine
    arbiter: WakeArbiter
    config: Callable[[], Mapping[str, object]]
    ha_device_id: Callable[[], str | None]
    recognizer: Callable[[], bool]      # HA's sentence matcher answers (connected, probe passed; §16.6)
    pipeline_id: Callable[[], Awaitable[str]]
    vocabulary: Callable[[], Vocabulary | None]
    esphome_reply: Callable[[bytes], AsyncIterator[RunEvent]]
    persist_turn: Callable[[TurnRow], Awaitable[int]]
    record_continuation: Callable[[int, str], Awaitable[None]]   # (row id, final follow-up outcome)


@dataclass(frozen=True)
class ActorEvent:
    """`state` on every state change; `terminal` with the §7 reason; `cue` with
    the LED cue; `dialog_focus` when `dialog_active` flips."""

    kind: ActorEventKind
    state: ActorState
    reason: TerminalReason | Cue | None = None
    dialog_active: bool = False


# --- wire bodies ---------------------------------------------------------------------


def _number(value: object, name: str) -> float:
    if not isinstance(value, (int, float, str)):
        raise ValueError(f"{name} must be a number, got {value!r}")
    return float(value)


def _optional_number(value: object, name: str) -> float | None:
    return None if value is None else _number(value, name)


def _config_number(config: Mapping[str, object], key: str, default: float) -> float:
    """A numeric config value, or `default` when it is absent or not a number.
    Numeric strings count: the config API stores values as given."""
    value = config.get(key, default)
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value)
        except ValueError:
            pass
    return default


def _opening_rule(raw: object) -> em_wake_rules.OpenRule | None:
    """`wake.candidate.rule`: None when absent (firmware without open_rules_v1)."""
    if raw is None:
        return None
    try:
        return em_wake_rules.OpenRule.parse(raw)
    except em_wake_rules.RuleError as err:
        raise ValueError(f"rule: {err}") from None


@dataclass(frozen=True, slots=True)
class ActiveAlert:
    """`wake.candidate.active_alert`: the occurrence ringing when the wake was heard (§6.3)."""

    id: str
    kind: RingKind | None
    name: str | None


@dataclass(frozen=True, slots=True)
class WakeCandidate:
    """A `wake.candidate` body (WIRE §4.4), validated as the actor needs it."""

    lease_id: str
    candidate_id: str
    capture_epoch: int
    support_start: int
    hops: tuple[WakeHop, ...]
    open_sample: int                  # the opening (last) hop's end
    threshold: float
    graph_sha256: str
    producing_sound: bool
    profile: object                   # logged with the turn trace
    active_alert: ActiveAlert | None
    chimed: bool                      # the device already played the wake chime (local_wake_chime)
    rule: em_wake_rules.OpenRule | None  # the live rule that opened it (open_rules_v1), else None

    @classmethod
    def parse(cls, body: Mapping[str, object]) -> WakeCandidate:
        """Raises (ProtocolError, KeyError, ValueError) for a malformed body."""
        hops_raw = body.get("hops") or []
        if not isinstance(hops_raw, list):
            raise ValueError("hops must be an array")
        hops: list[WakeHop] = []
        for h in hops_raw:
            if not isinstance(h, Mapping):
                raise ValueError("hops entries must be objects")
            hops.append(WakeHop(parse_u64(h["end_sample"], "hops.end_sample"),
                                _optional_number(h.get("raw"), "hops.raw"),
                                _optional_number(h.get("smoothed"), "hops.smoothed")))
        threshold = _number(body["threshold"], "threshold")
        wake_trigger_sample(hops, threshold)         # some hop must reach the latched threshold
        active = body.get("active_alert")
        alert = None
        if isinstance(active, Mapping) and active.get("id") is not None:
            kind, name = active.get("kind"), active.get("name")
            alert = ActiveAlert(str(active["id"]),
                                RingKind(kind) if isinstance(kind, str) and kind in RingKind else None,
                                name if isinstance(name, str) else None)
        return cls(
            lease_id=str(body["lease_id"]),
            candidate_id=str(body["candidate_id"]),
            capture_epoch=parse_u64(body.get("capture_epoch"), "capture_epoch"),
            support_start=parse_u64(body.get("support_start"), "support_start"),
            hops=tuple(hops),
            open_sample=max(h.end_sample for h in hops),
            threshold=threshold,
            graph_sha256=str(body["graph_sha256"]),
            producing_sound=bool(body.get("producing_sound")),
            profile=body.get("profile"),
            active_alert=alert,
            chimed=body.get("chimed") is True,
            rule=_opening_rule(body.get("rule")),
        )

    @property
    def peak(self) -> float | None:
        """Highest smoothed score over the hops (None when none was scored)."""
        return max((h.smoothed for h in self.hops if h.smoothed is not None), default=None)


# --- reference access ---------------------------------------------------------------


def read_reference(view: ReferenceView, a: int, b: int) -> tuple[np.ndarray, np.ndarray]:
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


@dataclass(frozen=True, slots=True)
class _CellArrays:
    """Mic PCM, laid-out reference, and per-cell evidence of whole cells, for `em_attribution`."""

    mic: np.ndarray
    reference: np.ndarray
    cell_valid: list[bool]
    vad: list[float | None]
    background: list[float | None]
    reference_valid: np.ndarray
    coverage: Coverage


@dataclass(frozen=True, slots=True)
class _FixedLabel:
    result: EchoResult


@dataclass(frozen=True, slots=True)
class _LagStep:
    arrays: _CellArrays


@dataclass(frozen=True, slots=True)
class _LabelStep:
    arrays: _CellArrays


_EchoStep = _FixedLabel | _LagStep | _LabelStep


# --- actor queue events -------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class _Attach:
    pass


@dataclass(frozen=True, slots=True)
class _Detach:
    uplink: UplinkSession | None
    render: RenderClient | None


@dataclass(frozen=True, slots=True)
class _MicEpoch:
    pass


@dataclass(frozen=True, slots=True)
class _Audio:
    deliveries: list[Delivery]


@dataclass(frozen=True, slots=True)
class _Button:
    body: Mapping[str, object]


@dataclass(frozen=True, slots=True)
class _Cancel:
    reason: TerminalReason


@dataclass(frozen=True, slots=True)
class _Diagnostic:
    pass


@dataclass(frozen=True, slots=True)
class _Close:
    pass


@dataclass(frozen=True, slots=True)
class _Recognized:
    """HA's sentence matcher on one stable prefix of an utterance (§16.6); `klass` None: no answer."""

    utterance_id: str
    text: str
    klass: GrammarClass | None
    ms: int
    error: str | None


@dataclass
class _Focus:
    lease_id: str
    owner: str
    focus: Focus
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
    # The lease's native AFE records when it takes the session's `afe` stream
    # (afe_metadata_v1), else None. Held past the lease's release so the turn or
    # candidate can summarise them when it ends; evidence only, never a decision input.
    afe: AfeTimeline | None = None


@dataclass
class _Candidate:
    wire: WakeCandidate
    model: WakeModel
    runtime: _Runtime
    received: float
    peak: float | None
    support_end: int | None = None
    comparison: EchoResult | None = None
    comparison_submitted: bool = False
    verification: VerificationResult | None = None
    verification_submitted: bool = False
    reference_events: list[tuple[int, ReferenceCandidate]] = field(default_factory=list)
    reference_ready: bool = False

    @property
    def candidate_id(self) -> str:
        return self.wire.candidate_id

    @property
    def lease_id(self) -> str:
        return self.wire.lease_id

    @property
    def capture_epoch(self) -> int:
        return self.wire.capture_epoch

    @property
    def open_sample(self) -> int:
        return self.wire.open_sample

    @property
    def support_start(self) -> int:
        return self.wire.support_start

    @property
    def threshold(self) -> float:
        return self.wire.threshold

    @property
    def producing_sound(self) -> bool:
        return self.wire.producing_sound


@dataclass(frozen=True, slots=True)
class _Clip:
    """Canonical PCM read off a lease's mic timeline, starting at capture sample `start`."""

    start: int
    pcm: bytes

    @property
    def span(self) -> tuple[int, int]:
        return self.start, self.end

    @property
    def end(self) -> int:
        return self.start + len(self.pcm) // 2


@dataclass
class _ResponseLease:
    """A committed turn's response `turn` lease (§4.4) and what it received: `afe` always, `mic`
    when the turn recording is made. It closes at `close_at`: RESPONSE_LEASE_MAX_S after the
    commit, or RESPONSE_LEASE_TAIL_S after the turn ends, whichever is first; `done` resolves once
    it has ended, however it ended."""

    lease_id: str
    timeline: LeaseTimeline
    close_at: float
    done: asyncio.Future[None]


@dataclass(frozen=True, slots=True)
class _AmPmPending:
    """"AM or PM?": the alarm waiting for its half of the day, and the turn that asked."""

    parse: AlarmParse
    turn_id: str


@dataclass(frozen=True, slots=True)
class _WhichTimerPending:
    """"Which one?": the timers offered, and the line that asks again."""

    timers: list[HaTimer]
    again: str


_PendingOperation = _AmPmPending | _WhichTimerPending


@dataclass
class _Expectation:
    """A §9.1 reply expectation. `owner` names its focus/uplink leases and becomes
    the reply turn's id."""

    expectation_id: str
    owner: str
    generation: int
    source: UtteranceKind             # REPLY (websocket path) | HA_REPLY (ESPHome path)
    conversation_id: str | None
    originating_turn_id: str | None
    chain_started: float
    chain_count: int
    choices: tuple[Choice, ...] | None = None
    pending_operation: _PendingOperation | None = None
    reprompted: bool = False
    runtime: _Runtime | None = None
    watch: ReplyWatch = field(default_factory=ReplyWatch)
    prompt: Playback | None = None
    awaiting_audio: bool = False      # the window opened before the lease had any mic: open at its first cell
    window_opened: float | None = None   # loop time the reply window opened (drain, or an early answer)
    origin: _Turn | None = None       # the turn that asked; its row records what became of the question


@dataclass
class _Turn:
    turn_id: str
    generation: int
    trigger: UtteranceKind
    opened: float
    utterance: Utterance
    runtime: _Runtime
    model: WakeModel | None = None
    candidate: _Candidate | None = None
    context: CommandContext | None = None
    context_target: str | None = None
    conversation_id: str | None = None
    expectation: _Expectation | None = None
    next_expectation: _Expectation | None = None
    task: asyncio.Task[None] | None = None
    run: _Abandonable | None = None
    playback: Playback | None = None
    playback_reason: PlaybackEnd | None = None   # how the spoken response ended (render.finished reason, or a limit)
    committed_at: float | None = None         # monotonic time the endpoint commit was handled
    commit_frontier: int | None = None        # the newest mic sample received by then: places `stages`
    utterance_end_ns: float | None = None     # device CLOCK_MONOTONIC of commit.boundary; None: no mic clock fit
    audible_at: float | None = None           # monotonic time the response became audible
    continuation: Continuation | None = None  # fate of the follow-up this turn asked for; None: it asked none
    persisted: asyncio.Task[int | None] | None = None   # the row write; its result is the row id
    wake_db: float | None = None
    wake_clip: _Clip | None = None
    capture: _Clip | None = None              # the committed lease's mic for the turn recording; None: not made
    stt_copy: bytes | None = None             # the committed span as sent to STT, sample for sample
    stt_raw: str | None = None                # HA's transcript before wake-phrase removal
    stt_text: str | None = None               # what was routed: stripped HA transcript or local command
    response_text: str | None = None          # the spoken answer, from whichever handler took the request
    response_type: str | None = None          # HA intent-end response_type
    intent_local: bool | None = None          # HA's built-in agent answered (None: not reported)
    outcome: TurnOutcome | None = None
    terminal: TerminalReason | None = None
    commit: Commit | None = None
    coverage: float | None = None
    afe_evidence: AfeEvidence | None = None   # native AFE evidence; None: unavailable (or not yet taken)
    afe_series: AfeSeries | None = None       # the per-period records behind it; None: unavailable or no data
    response_lease: _ResponseLease | None = None   # the lease after the commit; None: none opened
    stages: dict[TurnStage, tuple[float, float]] = field(default_factory=dict)   # monotonic [start, end]
    timings: dict[str, int] = field(default_factory=dict)
    reopened: list[UtteranceTrace] = field(default_factory=list)   # a reply's utterances before a gap


@dataclass
class _Announcement:
    url: str
    preannounce_url: str | None
    start_conversation: bool
    future: asyncio.Future[None]


_Event = (_Attach | _Detach | _MicEpoch | Envelope | _Audio | Observation | _Recognized | _Button | _Cancel
          | _Diagnostic | _Announcement | _Close)


class SessionActor:
    """One per device, outliving sessions. `start()` creates the actor task."""

    def __init__(self, device_id: str, deps: ActorDeps):
        self.device_id = device_id
        self.deps = deps
        self.state = ActorState.IDLE
        self.awaiting_intent = False
        self.link: _Link | None = None
        self.render: RenderClient | None = None
        self.uplink: UplinkSession | None = None
        self._queue: asyncio.PriorityQueue[tuple[int, int, _Event]] = asyncio.PriorityQueue()
        self._sequence = 0
        self._task: asyncio.Task[None] | None = None
        self._closed = False
        self._generation = 0
        self._turn: _Turn | None = None
        self._candidate: _Candidate | None = None
        self._expectation: _Expectation | None = None
        self._runtimes: dict[str, _Runtime] = {}
        self._focus: dict[str, _Focus] = {}
        self._dialog_playback: Playback | None = None
        self._listeners: list[Callable[[ActorEvent], object]] = []
        self._muted = False
        self._alert_foreground: bool | None = None
        self._diagnostic: Callable[[int, np.ndarray], None] | None = None
        self._diagnostic_lease: str | None = None
        self._announcements = asyncio.Lock()
        self._tasks: set[asyncio.Task[object]] = set()
        self._persists: set[asyncio.Task[object]] = set()
        self._listener_futures: set[asyncio.Future[object]] = set()   # awaitables listeners returned
        self._dialog_active = False
        self._response_leases: dict[str, _ResponseLease] = {}   # by lease id, until each has ended

    # --- public surface -------------------------------------------------------

    @property
    def turn_active(self) -> bool:
        """A candidate, turn, reply expectation, or its feedback is live."""
        return self._turn is not None or self._expectation is not None or self.state != ActorState.IDLE

    def add_listener(self, cb: Callable[[ActorEvent], object]) -> Callable[[], None]:
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
        self._queue.put_nowait((P_CLOSE, 0, _Close()))
        if self._task is not None:
            await self._task

    def attach(self, link: _Link, render: RenderClient, *, afe_metadata: bool = False) -> None:
        """Session ready: the new session's link, renderer, and an empty uplink record.
        `afe_metadata`: session.ready granted it, so the session carries the `afe` stream."""
        self.link, self.render = link, render
        self.uplink = UplinkSession(afe_metadata=afe_metadata)
        self._post(P_LOSS, _Attach())

    def detach(self, reason: CloseReason) -> None:
        """Session lost: nothing more is sent on the old link; the actor cleans up in order."""
        old = _Detach(self.uplink, self.render)
        self.link = self.render = self.uplink = None
        self._post(P_LOSS, old)

    def on_message(self, envelope: Envelope) -> None:
        msg_type, body = envelope.type, envelope.body
        if msg_type in (MessageType.STREAM_OPEN, MessageType.STREAM_END):
            # Applied at once: audio of a new epoch may follow on the audio socket
            # before the actor reaches this message (WIRE §4.2).
            if self.uplink is not None:
                if msg_type == MessageType.STREAM_OPEN:
                    self.uplink.stream_open(body)
                else:
                    self.uplink.stream_end(body)
            if msg_type == MessageType.STREAM_OPEN and body.get("stream_id") == StreamId.MIC:
                self._post(P_SPEECH, _MicEpoch())
            return
        priority = _MESSAGE_PRIORITY.get(msg_type)
        if priority is not None:
            self._post(priority, envelope)

    def on_observation(self, obs: Observation) -> None:
        if obs.device_id == self.device_id:
            self._post(P_SPEECH, obs)

    def on_audio(self, frame: bytes) -> None:
        """One uplink EMA1 frame. Raises `em_audio_timeline.ProtocolError` for a WIRE violation."""
        if self.uplink is None:
            return
        # `ingest` has already stored AFE records, and a response lease's mic, in their leases'
        # timelines; they are read only when a turn or candidate ends, so they never reach the
        # queue, the worker, or a decision.
        deliveries = [d for d in self.uplink.ingest(frame)
                      if d.stream_id != StreamId.AFE and d.lease_id not in self._response_leases]
        if deliveries:
            gap = any(d.packet.discontinuity or d.packet.muted for d in deliveries)
            self._post(P_GAP if gap else P_SPEECH, _Audio(deliveries))

    def button_turn(self, action: Mapping[str, object]) -> None:
        self._post(P_INTERRUPT, _Button(action))

    def cancel_turn(self, reason: TerminalReason = TerminalReason.INTERRUPTED) -> None:
        self._post(P_STOP, _Cancel(reason))

    async def announce(self, url: str, *, preannounce_url: str | None,
                       start_conversation: bool) -> None:
        """Play an HA announcement as dialog output; returns after guarded drain or cancellation."""
        if self.link is None or self.render is None or self._closed:
            return
        future: asyncio.Future[None] = asyncio.get_running_loop().create_future()
        self._post(P_RESULT, _Announcement(url, preannounce_url, start_conversation, future))
        await future

    async def open_diagnostic(self, on_mic: Callable[[int, np.ndarray], None]) -> None:
        """Hold a live-mic diagnostic lease (re-opened after every reconnect); wakes are refused meanwhile."""
        self._diagnostic = on_mic
        self._post(P_INTERRUPT, _Diagnostic())

    async def close_diagnostic(self) -> None:
        self._diagnostic = None
        self._post(P_INTERRUPT, _Diagnostic())

    # --- loop ---------------------------------------------------------------------

    def _post(self, priority: int, event: _Event) -> None:
        if self._closed:
            if isinstance(event, _Announcement) and not event.future.done():
                event.future.set_result(None)
            return
        self._sequence += 1
        self._queue.put_nowait((priority, self._sequence, event))

    def _spawn[T](self, coro: Coroutine[object, object, T]) -> asyncio.Task[T]:
        task = asyncio.create_task(coro)
        self._tasks.add(task)
        task.add_done_callback(self._task_done)
        return task

    def _task_done(self, task: asyncio.Task[object]) -> None:
        self._tasks.discard(task)
        self._persists.discard(task)
        if not task.cancelled() and task.exception() is not None:
            log.error("[%s] actor helper failed", self.device_id, exc_info=task.exception())

    async def _run(self) -> None:
        loop = asyncio.get_running_loop()
        last_tick = loop.time()
        while True:
            timeout = max(0.0, ACTOR_TICK_S - (loop.time() - last_tick))
            event: _Event | None
            try:
                _, _, event = await asyncio.wait_for(self._queue.get(), timeout)
            except TimeoutError:
                event = None
            if isinstance(event, _Close):
                break
            if event is not None:
                try:
                    await self._dispatch(event)
                except Exception:
                    log.exception("[%s] actor event %s failed", self.device_id,
                                  event.type if isinstance(event, Envelope) else type(event).__name__)
            if loop.time() - last_tick >= ACTOR_TICK_S:
                last_tick = loop.time()
                try:
                    await self._tick()
                except Exception:
                    log.exception("[%s] actor deadline tick failed", self.device_id)
        await self._shutdown()

    async def _dispatch(self, event: _Event) -> None:
        match event:
            case _Attach() | _MicEpoch() | _Diagnostic():
                await self._sync_diagnostic()
            case _Detach(uplink, render):
                await self._session_lost(uplink, render)
            case Envelope():
                await self._message(event)
            case _Audio(deliveries):
                await self._audio(deliveries)
            case Observation():
                await self._observation(event)
            case _Recognized():
                self._recognized(event)
            case _Button(body):
                await self._button(body)
            case _Cancel(reason):
                await self._cancel(reason)
            case _Announcement():
                self._spawn(self._announcement(event))

    async def _message(self, envelope: Envelope) -> None:
        body = envelope.body
        match envelope.type:
            case MessageType.PRIVACY_CHANGED:
                await self._privacy(body)
            case MessageType.UPLINK_ENDED:
                if self.uplink is not None:
                    lease = self.uplink.leases.device_ended(body)
                    if lease is not None:
                        await self._lease_ended(lease.lease_id, lease.ended or LeaseEnd.CLOSED)
            case MessageType.WAKE_CANDIDATE:
                await self._wake_candidate(envelope)
            case MessageType.WAKE_CANDIDATE_END:
                self._candidate_end(body)
            case MessageType.ALERT_STATE:
                await self._alert_state(body)

    async def _tick(self) -> None:
        loop = asyncio.get_running_loop()
        now = loop.time()
        if self.link is not None and self.uplink is not None:
            for lease in self.uplink.leases.due_renewals():
                await self._lease_message(self.uplink.leases.renew(lease.lease_id))
            for lease in self.uplink.leases.expire():
                await self._lease_ended(lease.lease_id, lease.ended or LeaseEnd.TTL)
        for response in [r for r in self._response_leases.values() if now >= r.close_at]:
            await self._end_response_lease(response)
        for focus in list(self._focus.values()):
            if now - focus.renewed >= RENEW_S:
                focus.renewed = now
                await self._send(MessageType.FOCUS_RENEW, {"lease_id": focus.lease_id, "ttl_ms": FOCUS_TTL_MS},
                                 focus.generation)
        candidate = self._candidate
        if candidate is not None and now - candidate.received >= WAKE_VERIFY_S:
            await self._decide_candidate(candidate, deadline=True)
        await self._reply_wall_clock(now)

    async def _reply_wall_clock(self, now: float) -> None:
        """§16.2 backstop: a reply window without command speech REPLY_WALL_S after it opened
        closes `no_input` as silently as its sample-time end, whatever the mic did."""
        turn = self._turn
        exp = turn.expectation if turn is not None and turn.trigger in REPLY_KINDS else None
        if (turn is not None and exp is not None and turn.commit is None and turn.terminal is None
                and not turn.utterance.has_command_speech and exp.window_opened is not None
                and now - exp.window_opened >= REPLY_WALL_S):
            log.warning("[%s] reply turn %s: no answer %.0f s after its window opened; closing",
                        self.device_id, turn.turn_id, REPLY_WALL_S)
            await self._finish_turn(turn, TerminalReason.NO_INPUT)
        waiting = self._expectation
        if (waiting is not None and waiting.awaiting_audio and waiting.window_opened is not None
                and now - waiting.window_opened >= REPLY_WALL_S):
            log.warning("[%s] reply window got no microphone in %.0f s; closing", self.device_id, REPLY_WALL_S)
            await self._end_expectation(TerminalReason.NO_INPUT, outcome=TerminalReason.REPLY_TIMEOUT)

    async def _shutdown(self) -> None:
        if self._turn is not None:
            await self._finish_turn(self._turn, TerminalReason.SESSION_LOST, feedback=False)
        await self._drop_expectation(TerminalReason.SESSION_LOST)
        await self._end_all_response_leases(close=True)   # the rows wait for none of them
        if self._persists:
            await asyncio.wait(list(self._persists), timeout=PERSIST_SHUTDOWN_S)
        for task in list(self._tasks):
            task.cancel()
        await asyncio.gather(*list(self._tasks), return_exceptions=True)

    # --- session lifecycle -----------------------------------------------------------

    async def _session_lost(self, uplink: UplinkSession | None, render: RenderClient | None) -> None:
        # Every dialog and uplink lease ended with the session (§16.1); nothing is released on the wire.
        self._focus.clear()
        if uplink is not None:
            for lease in uplink.clear(LeaseEnd.SESSION):
                self.deps.worker.close_lease(lease.lease_id)
        await self._end_all_response_leases(close=False)
        self._candidate = None
        if self._turn is not None:
            await self._finish_turn(self._turn, TerminalReason.SESSION_LOST)
        await self._drop_expectation(TerminalReason.SESSION_LOST)
        self._runtimes.clear()
        self._diagnostic_lease = None
        self._alert_foreground = None
        if render is not None:
            render.fail_all(TerminalReason.SESSION_LOST)
        self._update_dialog_active()
        if self._turn is None:
            self._set_state(ActorState.IDLE)

    async def _privacy(self, body: Mapping[str, object]) -> None:
        self._muted = bool(body.get("muted"))
        if not self._muted:
            return
        # Device-sovereign mute ends every lease and erases buffered audio (§3.2 inv. 10).
        if self.uplink is not None:
            for lease in self.uplink.clear(LeaseEnd.MUTE):
                self.deps.worker.close_lease(lease.lease_id)
        await self._end_all_response_leases(close=False)
        self._candidate = None
        self._diagnostic_lease = None
        if self._turn is not None:
            await self._finish_turn(self._turn, TerminalReason.MUTED)
        await self._drop_expectation(TerminalReason.MUTED)
        self._runtimes.clear()
        if self._turn is None:
            self._set_state(ActorState.IDLE)

    async def _alert_state(self, body: Mapping[str, object]) -> None:
        active = body.get("active")
        foreground = bool(active.get("foreground")) if isinstance(active, Mapping) else None
        was, self._alert_foreground = self._alert_foreground, foreground
        turn = self._turn
        # The device foregrounds a backgrounded alert at its 15 s cap (§6.2).
        if was is False and foreground and turn is not None and turn.commit is None:
            await self._finish_turn(turn, TerminalReason.ALERT_PREEMPTED)

    async def _lease_ended(self, lease_id: str, reason: LeaseEnd) -> None:
        if self.uplink is not None:
            self.uplink.release(lease_id)
        runtime = self._runtimes.pop(lease_id, None)
        if runtime is not None:
            self.deps.worker.close_lease(lease_id)
        response = self._response_leases.get(lease_id)
        if response is not None:
            await self._end_response_lease(response, close=False)
            return
        if lease_id == self._diagnostic_lease:
            self._diagnostic_lease = None
            if reason not in (LeaseEnd.TTL, LeaseEnd.CLOSED):   # 30 min cap and explicit closes stay closed
                await self._sync_diagnostic()
            return
        candidate = self._candidate
        if candidate is not None and candidate.lease_id == lease_id:
            await self._reject_candidate(candidate, TerminalReason.VERIFIER_TIMEOUT)
            return
        turn = self._turn
        if turn is not None and turn.runtime.lease_id == lease_id and turn.commit is None:
            await self._finish_turn(turn, _LEASE_END_TERMINAL.get(reason, TerminalReason.INTERRUPTED))
        exp = self._expectation
        if exp is not None and exp.runtime is not None and exp.runtime.lease_id == lease_id:
            exp.runtime = None
            await self._end_expectation(TerminalReason.INTERRUPTED)

    # --- diagnostic lease --------------------------------------------------------------

    async def _sync_diagnostic(self) -> None:
        if self._diagnostic is None:
            if self._diagnostic_lease is not None:
                await self._close_uplink(self._diagnostic_lease, LeaseEnd.CLOSED)
                if self.uplink is not None:
                    self.uplink.release(self._diagnostic_lease)
                self._diagnostic_lease = None
            return
        if self._diagnostic_lease is not None or self.uplink is None or self._muted:
            return
        epoch = self.uplink.streams.current(StreamId.MIC)
        if epoch is None:
            return
        lease_id = str(uuid.uuid4())
        await self._lease_message(self.uplink.open(
            lease_id, LeaseReason.DIAGNOSTIC, f"diagnostic:{self.device_id}", epoch, {StreamId.MIC: None}))
        self._diagnostic_lease = lease_id

    # --- wake candidates -----------------------------------------------------------------

    def _refusal(self) -> CandidateRefusal | None:
        if self._diagnostic is not None:
            return CandidateRefusal.DIAGNOSTIC
        if not self.deps.worker.available:
            return CandidateRefusal.SPEECH_UNAVAILABLE
        if self._muted:
            return CandidateRefusal.MUTED
        return None

    async def _wake_candidate(self, envelope: Envelope) -> None:
        link, uplink = self.link, self.uplink
        if link is None or uplink is None:
            return
        message_id = envelope.message_id
        try:
            wire = WakeCandidate.parse(envelope.body)
            model = self.deps.registry.get(wire.graph_sha256)
            uplink.candidate(wire.lease_id, wire.candidate_id, wire.capture_epoch, wire.support_start)
        except Exception as exc:
            log.warning("[%s] refused malformed wake.candidate: %s", self.device_id, exc)
            await link.ack(message_id, AckStatus.REJECTED, CandidateRefusal.MALFORMED)
            return
        lease_id = wire.lease_id
        refusal = self._refusal() or (CandidateRefusal.CANDIDATE_PENDING if self._candidate is not None else None)
        if refusal is not None:
            uplink.acknowledge(lease_id, False)
            uplink.release(lease_id)
            await link.ack(message_id, AckStatus.REJECTED, refusal)
            if refusal in (CandidateRefusal.DIAGNOSTIC, CandidateRefusal.SPEECH_UNAVAILABLE):
                self._cue(Cue.ERROR_ANIM)
            return
        try:
            await self.deps.worker.load_wake_graph(model.graph_sha256)
            self.deps.worker.open_lease(self.device_id, wire.capture_epoch, lease_id, model.graph_sha256)
        except Exception as exc:
            log.warning("[%s] wake candidate refused, speech worker: %s", self.device_id, exc)
            uplink.acknowledge(lease_id, False)
            uplink.release(lease_id)
            await link.ack(message_id, AckStatus.REJECTED, CandidateRefusal.SPEECH_UNAVAILABLE)
            self._cue(Cue.ERROR_ANIM)
            return
        uplink.acknowledge(lease_id, True)
        await link.ack(message_id, AckStatus.ACCEPTED)
        runtime = self._new_runtime(lease_id)
        candidate = _Candidate(wire, model, runtime, asyncio.get_running_loop().time(), wire.peak)
        self._candidate = candidate
        if self.deps.recognizer():
            # The command's prefixes are asked about in a second or two (§16.6): make sure
            # HA's sentence matcher is resident by then, not paged out after idle.
            self._spawn(self._warm_recognizer())
        if not candidate.producing_sound:
            # Idle profile: the BCResNet threshold alone accepts (§6.1).
            await self._accept_candidate(candidate)

    def _candidate_end(self, body: Mapping[str, object]) -> None:
        candidate = self._candidate
        if candidate is None or body.get("candidate_id") != candidate.candidate_id:
            turn = self._turn
            candidate = turn.candidate if turn is not None else None
            if candidate is None or body.get("candidate_id") != candidate.candidate_id:
                return
        candidate.support_end = parse_u64(body.get("support_end"), "support_end")
        peak = _optional_number(body.get("peak_smoothed"), "peak_smoothed")
        if peak is not None:
            candidate.peak = peak

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
            comparison=candidate.comparison or EchoResult.UNKNOWN,
        )
        if verdict is not None:
            await self._reject_candidate(candidate, _VERDICT_TERMINAL[verdict])
        elif candidate.verification == VerificationResult.PASS:
            await self._accept_candidate(candidate)
        elif candidate.verification == VerificationResult.FAIL:
            await self._reject_candidate(candidate, TerminalReason.UNVERIFIED_WAKE)
        elif candidate.verification == VerificationResult.TIMEOUT or deadline:
            await self._reject_candidate(candidate, TerminalReason.VERIFIER_TIMEOUT)

    async def _reject_candidate(self, candidate: _Candidate, reason: TerminalReason) -> None:
        """Close the candidate lease (also releasing any provisional duck); no turn, focus, or chime
        (a local_wake_chime device may already have chimed an idle candidate; it is not retracted)."""
        self._candidate = None
        afe, series = _afe_snapshot(candidate.runtime, candidate, None)
        close = LeaseEnd.ARBITRATION_LOST if reason == TerminalReason.ARBITRATION_LOST else LeaseEnd.REJECTED
        await self._close_uplink(candidate.lease_id, close)
        self._release_runtime(candidate.lease_id)
        self._emit(ActorEvent(ActorEventKind.TERMINAL, self.state, reason, self._dialog_active))
        self._persists.add(self._spawn(self._persist_candidate(candidate, reason, afe, series)))

    async def _accept_candidate(self, candidate: _Candidate) -> None:
        context, target = self._command_context(candidate)
        if context is None:
            window = _config_number(self._config(), "wakeArbitrationMs", WAKE_ARBITRATION_MS) / 1000.0
            if self.deps.arbiter.claim(self.device_id, window) != self.device_id:
                await self._reject_candidate(candidate, TerminalReason.ARBITRATION_LOST)
                return
        self._candidate = None
        inherited = self._take_expectation()
        await self._supersede()
        uplink = self.uplink
        if uplink is None:
            return
        turn_id = str(uuid.uuid4())
        await self._lease_message(uplink.leases.convert_to_turn(candidate.lease_id, turn_id))
        trigger = wake_trigger_sample(candidate.wire.hops, candidate.threshold)
        lease = uplink.leases.get(candidate.lease_id)
        mic_start = lease.streams.get(StreamId.MIC) if lease is not None else None
        start = max(candidate.support_start - PREROLL, mic_start or 0)
        spec = self._spec(UtteranceKind.WAKE, start, trigger, inherited,
                          seed_start=candidate.support_start, wake_open=candidate.open_sample,
                          wake_phrase=candidate.model.wake_phrase, context=context)
        turn = _Turn(turn_id, self._generation, UtteranceKind.WAKE, asyncio.get_running_loop().time(),
                     Utterance(spec), candidate.runtime, candidate.model, candidate, context, target,
                     inherited.conversation_id if inherited else None, inherited)
        self._turn = turn
        await self._acquire_focus(turn_id, Focus.DIALOG_INPUT, turn.generation)
        # No chime when the command context is non-empty: the ring/reply stopping is the acknowledgement (§16.2);
        # none when the device already chimed at candidate open (idle wakes on local_wake_chime firmware, §11.2).
        if (context is None and em_config_sections.wake_sound(self._config()) and self.render is not None
                and not candidate.wire.chimed):
            try:
                await self.render.play_local(SourceClass.EARCON, WAKE_CHIME, generation=turn.generation)
            except Exception:
                log.warning("[%s] wake chime failed", self.device_id, exc_info=True)
        self._set_state(ActorState.ARMED)
        await self._open_utterance(turn)

    def _command_context(self, candidate: _Candidate) -> tuple[CommandContext | None, str | None]:
        """§6.3 step 1: the alert occurrence, else the dialog output this wake cancels —
        only a reply the user can hear: one whose audio has not started is not yet a context
        (`started` fails only together with `finished`, so an undone playback that has
        started is audible)."""
        active = candidate.wire.active_alert
        if active is not None:
            return CommandContext(ContextKind.ALERT, active.kind, active.name), active.id
        playback = self._dialog_playback
        if playback is not None and not playback.done and playback.started.done():
            return CommandContext(ContextKind.DIALOG), playback.playback_id
        return None, None

    # --- button, cancel, replies --------------------------------------------------------

    async def _button(self, body: Mapping[str, object]) -> None:
        uplink = self.uplink
        if (self.link is None or uplink is None or self._refusal() is not None
                or body.get("capture_epoch") is None or body.get("capture_sample") is None):
            self._cue(Cue.ERROR_ANIM)
            return
        capture_epoch = parse_u64(body["capture_epoch"], "capture_epoch")
        press = parse_u64(body["capture_sample"], "capture_sample")
        inherited = self._take_expectation()
        await self._supersede()
        turn_id, lease_id = str(uuid.uuid4()), str(uuid.uuid4())
        mic = max(0, press - PREROLL)
        streams: dict[StreamId, int | None] = {
            StreamId.MIC: mic,
            StreamId.CELLS: max(0, mic - CANDIDATE_CELLS_LEAD),
            StreamId.REFERENCE: max(0, mic - CANDIDATE_REFERENCE_LEAD),
        }
        if uplink.afe_metadata:
            # From the reference start (the AEC state before the press); the lease rounds it to 1280.
            streams[StreamId.AFE] = streams[StreamId.REFERENCE]
        await self._lease_message(uplink.open(lease_id, LeaseReason.TURN, turn_id, capture_epoch, streams))
        model = self.deps.registry.for_config(self._config())
        await self.deps.worker.load_wake_graph(model.graph_sha256)
        self.deps.worker.open_lease(self.device_id, capture_epoch, lease_id, model.graph_sha256)
        runtime = self._new_runtime(lease_id)
        lease = uplink.leases.get(lease_id)
        mic_start = lease.streams.get(StreamId.MIC) if lease is not None else None
        start = mic if mic_start is None else mic_start      # the lease rounds it to the cell grid
        turn = _Turn(turn_id, self._generation, UtteranceKind.BUTTON, asyncio.get_running_loop().time(),
                     Utterance(self._spec(UtteranceKind.BUTTON, start, press, inherited)), runtime,
                     conversation_id=inherited.conversation_id if inherited else None, expectation=inherited)
        self._turn = turn
        await self._acquire_focus(turn_id, Focus.DIALOG_INPUT, turn.generation)
        self._set_state(ActorState.ARMED)
        await self._open_utterance(turn)

    async def _cancel(self, reason: TerminalReason) -> None:
        turn = self._turn
        if turn is not None:
            # A reply window dismissed before anyone spoke ends quietly, as an expectation does.
            silent = turn.trigger in REPLY_KINDS and not turn.utterance.has_command_speech
            await self._finish_turn(turn, reason, feedback=not silent)
        elif self._expectation is not None:
            await self._end_expectation(reason)
        elif self._dialog_playback is not None and not self._dialog_playback.done:
            await self._dialog_playback.cancel(reason)

    def _take_expectation(self) -> _Expectation | None:
        """A still-valid question survives an explicit wake/button as context (§9.1): the live
        expectation, or the one an uncommitted reply turn listens for. Valid: within the chain's
        REPLY_CHAIN_S (a reply turn without command speech is inside its window by construction)."""
        exp = self._expectation
        turn = self._turn
        if exp is None and turn is not None and turn.trigger in REPLY_KINDS and turn.commit is None:
            exp = turn.expectation
        if exp is None or asyncio.get_running_loop().time() - exp.chain_started >= REPLY_CHAIN_S:
            return None
        self._settle(exp, FollowUp.WAKE)
        return exp

    async def _supersede(self) -> None:
        """A new turn: bump the generation, then cancel prior output and fence the old turn (§16.2)."""
        self._generation += 1
        if self._turn is not None:
            await self._finish_turn(self._turn, TerminalReason.SUPERSEDED, feedback=False)
        await self._drop_expectation(TerminalReason.SUPERSEDED)
        if self._dialog_playback is not None and not self._dialog_playback.done:
            await self._dialog_playback.cancel(TerminalReason.INTERRUPTED)

    def _config(self) -> Mapping[str, object]:
        return self.deps.config() or {}

    def _spec(self, kind: UtteranceKind, start: int, trigger: int, inherited: _Expectation | None = None, *,
              seed_start: int | None = None, wake_open: int | None = None, wake_phrase: str | None = None,
              context: CommandContext | None = None, no_input_at: int | None = None) -> UtteranceSpec:
        vocab = self.deps.vocabulary()
        return UtteranceSpec(
            utterance_id=str(uuid.uuid4()), kind=kind, start=start, trigger=trigger,
            seed_start=seed_start, wake_open=wake_open, wake_phrase=wake_phrase,
            vocabulary=tuple(sorted(vocab.targets())) if vocab is not None else (),
            choices=inherited.choices if inherited is not None else None,
            context=context,
            extended=bool(self._config().get("extendedUtterances")),
            # Route R (the ESPHome reply) reads no completeness: nothing to transcribe at its pauses.
            pause_server=None if kind == UtteranceKind.HA_REPLY else self._pause_server(),
            no_input_at=no_input_at,
        )

    async def _open_utterance(self, turn: _Turn) -> None:
        """Bind the utterance to its lease: ASR from its pre-roll, per-cell echo from its start."""
        runtime, spec = turn.runtime, turn.utterance.spec
        timeline = self._timeline(runtime)
        if timeline is None:
            await self._finish_turn(turn, TerminalReason.SESSION_LOST)
            return
        timeline.set_utterance_start(spec.start)
        try:
            self.deps.worker.open_utterance(runtime.lease_id, spec.utterance_id, spec.start, spec.trigger,
                                            timeline.mic, pause_server=spec.pause_server)
        except SpeechWorkerError as exc:
            log.warning("[%s] utterance ASR failed to open: %s", self.device_id, exc)
            await self._finish_turn(turn, TerminalReason.INTERRUPTED)
            return
        runtime.utterance = turn.utterance
        runtime.echo_open = spec.start
        runtime.echo = None
        runtime.assembler.require_echo_from(spec.start)
        for ev in list(runtime.assembler.history):
            turn.utterance.push_cell(ev)
        await self._pump(runtime)

    async def _open_reply(self, exp: _Expectation, trigger: int, *, early: bool = False) -> None:
        """The reply turn (id = exp.owner) under the expectation's context (§9.1, §16.6), opened
        like a button turn: at the guarded drain with its trigger there, or at an early answer's
        onset. The normal reducer then decides what is the answer; a silent window closes
        `no_input`. The utterance starts REPLY_LEAD before a drain trigger (300 ms before an
        early answer's onset), never before the lease's first retained sample. Unless an answer
        is already under way, the drain is cued with the wake chime (§11.2)."""
        runtime = exp.runtime
        timeline = self._timeline(runtime) if runtime is not None else None
        if exp is not self._expectation or runtime is None or timeline is None:
            return
        exp.awaiting_audio = False
        if exp.window_opened is None:                 # an early answer opens its window here
            exp.window_opened = asyncio.get_running_loop().time()
        # §16.2: the generation increases before prior output is cancelled.
        self._generation += 1
        if exp.prompt is not None and not exp.prompt.done:
            await exp.prompt.cancel("early_answer")
        # The reply turn holds input focus before the asking turn releases its own.
        await self._acquire_focus(exp.owner, Focus.DIALOG_INPUT, self._generation)
        if self._turn is not None:
            await self._finish_turn(self._turn, TerminalReason.COMPLETED, feedback=False)
        self._expectation = None
        first = timeline.mic.first_sample if timeline.mic.first_sample is not None else trigger
        start = min(trigger, max(trigger - (PREROLL if early else REPLY_LEAD), first))
        kind = UtteranceKind.HA_REPLY if exp.source == UtteranceKind.HA_REPLY else UtteranceKind.REPLY
        turn = _Turn(exp.owner, self._generation, kind, asyncio.get_running_loop().time(),
                     Utterance(self._spec(kind, start, trigger, exp)), runtime,
                     conversation_id=exp.conversation_id, expectation=exp)
        self._turn = turn
        self._set_state(ActorState.ARMED)
        log.info("[%s] reply turn %s open for %s at %d%s", self.device_id, turn.turn_id,
                 exp.origin.turn_id if exp.origin is not None else exp.source, trigger,
                 " (early answer)" if early else "")
        await self._open_utterance(turn)
        if (early or not self._current(turn) or turn.utterance.answer_under_way
                or not em_config_sections.wake_sound(self._config()) or self.render is None):
            return
        try:
            await self.render.play_local(SourceClass.EARCON, WAKE_CHIME, generation=turn.generation)
        except Exception:
            log.warning("[%s] reply chime failed", self.device_id, exc_info=True)

    # --- audio and evidence ---------------------------------------------------------------

    def _new_runtime(self, lease_id: str) -> _Runtime:
        timeline = self.uplink.timelines.get(lease_id) if self.uplink is not None else None
        afe = timeline.afe if timeline is not None and StreamId.AFE in timeline.lease.streams else None
        runtime = _Runtime(lease_id, CellAssembler(), afe=afe)
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

    async def _audio(self, deliveries: list[Delivery]) -> None:
        if self.uplink is None:
            return
        touched: dict[str, _Runtime] = {}
        for d in deliveries:
            timeline = self.uplink.timelines.get(d.lease_id)
            if timeline is None:
                continue
            runtime = self._runtimes.get(d.lease_id)
            if d.stream_id == StreamId.MIC:
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
            elif runtime is not None and d.stream_id == StreamId.CELLS:
                for start, end in d.ranges:
                    records = timeline.cells.read(start, end)
                    runtime.assembler.add_cells(start, records.e_db.tolist(), records.flags.tolist())
            elif runtime is not None and d.stream_id == StreamId.REFERENCE:
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
        candidate = self._candidate if self._candidate and self._candidate.runtime is runtime else None
        match obs.payload:
            case VadPayload() as vad:
                runtime.assembler.add_vad(vad.first_cell_sample, vad.probabilities)
            case AsrPayload() as asr:
                if runtime.utterance is not None and obs.utterance_id == runtime.utterance.utterance_id:
                    runtime.utterance.push_asr(asr.tokens, asr.token_emission_sample,
                                               asr.trailing_blank_frames, obs.through_sample,
                                               finalize_ms=asr.finalize_ms, pause_text=asr.pause_text,
                                               pause_decode=asr.pause_decode)
            case EchoPayload() as echo:
                if obs.utterance_id and obs.utterance_id.startswith(CANDIDATE_TAG):
                    if candidate is not None:
                        candidate.comparison = echo.results[-1]
                        await self._decide_candidate(candidate)
                else:
                    runtime.echo_inflight = False
                    runtime.assembler.add_echo(echo.first_cell_sample, echo.results)
                    start = runtime.utterance.spec.start if runtime.utterance is not None else None
                    for i, result in enumerate(echo.results):
                        if start is None or echo.first_cell_sample + i * CELL >= start:
                            runtime.echo_total += 1
                            runtime.echo_known += result != EchoResult.UNKNOWN   # a trusted reference
            case VerificationPayload() as verification:
                if candidate is not None and verification.candidate_id == candidate.candidate_id:
                    candidate.verification = verification.result
                    await self._decide_candidate(candidate)
            case ReferenceScorePayload() as scores:
                if candidate is not None:
                    candidate.reference_events.extend((scores.reference_epoch, c) for c in scores.candidates)
                    mapping = self.uplink.clock_map(candidate.capture_epoch, scores.reference_epoch) \
                        if self.uplink is not None else None
                    end = candidate.support_end or candidate.open_sample
                    if mapping is None or mapping.reference_to_capture(obs.through_sample).sample >= end:
                        candidate.reference_ready = True
                    await self._decide_candidate(candidate)
            case ErrorPayload() as error:
                await self._worker_error(runtime, error)
                return
        await self._pump(runtime)

    async def _worker_error(self, runtime: _Runtime, error: ErrorPayload) -> None:
        """A worker failure aborts the affected utterance without dispatch (§8.1)."""
        log.warning("[%s] speech worker %s failed: %s", self.device_id, error.component, error.reason)
        runtime.echo_inflight = False
        candidate = self._candidate
        if candidate is not None and candidate.runtime is runtime:
            if error.component == ObservationKind.VERIFICATION:
                candidate.verification = VerificationResult.TIMEOUT
            await self._decide_candidate(candidate)
            return
        turn = self._turn
        if turn is not None and turn.runtime is runtime and turn.commit is None:
            await self._finish_turn(turn, TerminalReason.INTERRUPTED)
            return
        exp = self._expectation
        if exp is not None and exp.runtime is runtime:
            await self._end_expectation(TerminalReason.INTERRUPTED)

    async def _pump(self, runtime: _Runtime) -> None:
        """Advance one lease's evidence to its frontier and act on what it decides."""
        timeline = self._timeline(runtime)
        if timeline is None:
            return
        await self._schedule_echo(runtime, timeline)
        released = runtime.assembler.release()
        exp = self._expectation
        if exp is not None and exp.runtime is runtime and runtime.utterance is None:
            if exp.awaiting_audio:
                if runtime.assembler.history:
                    await self._open_reply(exp, runtime.assembler.history[0].start)
                    return
            else:
                for ev in released:
                    onset = exp.watch.push(ev)
                    if onset is not None:
                        await self._open_reply(exp, onset, early=True)
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
        reopen = utterance.reopen_at
        if reopen is not None:
            await self._reopen_reply(turn, reopen)
            return
        self._measure_wake(turn, runtime)
        mic_end = timeline.mic.frontier or utterance.spec.start
        for decision in utterance.advance(mic_end):
            await self._decision(turn, decision)
            if turn.terminal is not None or utterance.done:
                return
        if self.deps.recognizer():
            text = utterance.recognizer_query()
            if text is not None:
                self._spawn(self._recognize(utterance.utterance_id, text))
        if self.state == ActorState.ARMED and utterance.has_command_speech:
            self._set_state(ActorState.LISTENING)
        if mic_end - utterance.asr_through > ASR_STALL:
            log.warning("[%s] utterance ASR stopped at %d with mic at %d", self.device_id,
                        utterance.asr_through, mic_end)
            await self._finish_turn(turn, TerminalReason.INTERRUPTED)

    async def _reopen_reply(self, turn: _Turn, at: int) -> None:
        """A gap before any answer does not end a reply window (§16.6): the reply listens on with
        a fresh utterance from the gap's end, its trigger no earlier than the window's, closing
        `no_input` at the window's original end. The trace keeps the utterance it replaces."""
        old = turn.utterance
        log.info("[%s] reply turn %s: gap before any answer, listening on from %d", self.device_id,
                 turn.turn_id, at)
        turn.reopened.append(old.trace())
        self.deps.worker.close_utterance(turn.runtime.lease_id)
        turn.utterance = Utterance(self._spec(old.spec.kind, at, max(old.spec.trigger, at), turn.expectation,
                                              no_input_at=old.no_input_at))
        await self._open_utterance(turn)

    async def _recognize(self, utterance_id: str, text: str) -> None:
        """Ask HA's sentence matcher about a stable prefix (§16.6). The answer joins the speech
        evidence in arrival order: at a lower priority, blocks already queued behind a pause's
        re-decode would be judged before it."""
        loop = asyncio.get_running_loop()
        started = loop.time()
        klass: GrammarClass | None = None
        error: str | None = None
        try:
            said, probe = await self.deps.ha.recognize(recognizer_sentences(text), self.deps.ha_device_id(),
                                                       timeout=RECOGNIZE_TIMEOUT_S)
            klass = recognized_completeness(said, probe)
        except (HaError, HaUnavailable) as err:
            error = str(err)
        self._post(P_SPEECH, _Recognized(utterance_id, text, klass, round((loop.time() - started) * 1000), error))

    def _recognized(self, answer: _Recognized) -> None:
        turn = self._turn
        if turn is None or turn.utterance.utterance_id != answer.utterance_id:
            return
        turn.utterance.recognized(answer.text, answer.klass, ms=answer.ms, error=answer.error)

    async def _warm_recognizer(self) -> None:
        try:
            await self.deps.ha.warm_recognizer(self.deps.ha_device_id(), timeout=RECOGNIZE_TIMEOUT_S)
        except (HaError, HaUnavailable) as err:
            log.debug("[%s] HA sentence matcher warm-up failed: %s", self.device_id, err)

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

    def _reference_window(self, timeline: LeaseTimeline, a: int,
                          b: int) -> tuple[np.ndarray, np.ndarray, Coverage] | None:
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
        return reference, valid, Coverage.FULL if valid.all() else Coverage.PARTIAL

    def _cell_arrays(self, runtime: _Runtime, timeline: LeaseTimeline, a: int, b: int) -> _CellArrays | None:
        """Mic PCM, reference, and per-cell evidence for whole cells [a, b), or None if incomplete."""
        cells = runtime.assembler.lookup(a, b)
        if len(cells) != (b - a) // CELL or not timeline.mic.covers(a, b):
            return None
        reference = self._reference_window(timeline, a, b)
        if reference is None:
            return None
        ref, ref_valid, coverage = reference
        return _CellArrays(timeline.mic.read(a, b), ref, [ev.cell.valid for ev in cells],
                           [ev.cell.vad for ev in cells], [ev.background for ev in cells], ref_valid, coverage)

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
        steps: list[_EchoStep] = []
        next_cell = pending[0].start
        lag_step = False
        for ev in pending:
            ready = self._reference_ready(timeline, ev.end)
            if ready is not True and mic_frontier - ev.end < ECHO_WAIT:
                break
            while next_cell < ev.start:     # skipped invalid cells keep the result run contiguous
                steps.append(_FixedLabel(EchoResult.UNKNOWN))
                next_cell += CELL
            next_cell = ev.end
            if ready is not True:
                steps.append(_FixedLabel(EchoResult.UNKNOWN))   # timing/reference unknown is never "no playback"
                continue
            if not lag_step:
                window = tracker.estimate_window(ev.end)
                if window is not None:
                    lag_arrays = self._cell_arrays(runtime, timeline, *window)
                    if lag_arrays is None:
                        tracker.update_lag(None)
                    else:
                        steps.append(_LagStep(lag_arrays))
                        lag_step = True
            run_start = ev.start
            while run_start - CELL >= ev.end - ECHO_WINDOW_CELLS * CELL \
                    and timeline.mic.covers(run_start - CELL, run_start) \
                    and len(runtime.assembler.lookup(run_start - CELL, run_start)) == 1:
                run_start -= CELL
            arrays = self._cell_arrays(runtime, timeline, run_start, ev.end)
            steps.append(_LabelStep(arrays) if arrays is not None else _FixedLabel(EchoResult.UNKNOWN))
        if all(isinstance(step, _LagStep) for step in steps):
            return
        runtime.assembler.echo_requested(next_cell)
        runtime.echo_inflight = True

        def compare() -> tuple[tuple[EchoResult, ...], int | None]:
            results: list[EchoResult] = []
            for step in steps:
                match step:
                    case _LagStep(a):
                        tracker.update_lag(estimate_lag(a.mic, a.reference, a.cell_valid, a.reference_valid))
                    case _FixedLabel(result):
                        results.append(result)
                    case _LabelStep(a):
                        results.append(tracker.label(a.mic, a.reference, cell_valid=a.cell_valid, vad=a.vad,
                                                     background=a.background, coverage=a.coverage,
                                                     reference_valid=a.reference_valid))
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
            candidate.comparison = EchoResult.UNKNOWN    # missing timing or audio is unknown, not "no playback"
            candidate.reference_ready = True
            await self._decide_candidate(candidate)
            return
        support = arrays

        def compare() -> tuple[tuple[EchoResult, ...], int | None]:
            result = compare_reference(support.mic, support.reference, cell_valid=support.cell_valid,
                                       vad=support.vad, background=support.background,
                                       coverage=support.coverage, reference_valid=support.reference_valid)
            return (result.result,) * len(support.cell_valid), result.lag

        self.deps.worker.submit_echo(runtime.lease_id, a, b, compare, CANDIDATE_TAG + candidate.candidate_id)

    # --- endpoint decisions and dispatch ----------------------------------------------------

    async def _decision(self, turn: _Turn, decision: Decision) -> None:
        match decision:
            case Pending():
                self._set_state(ActorState.END_PENDING)
            case Revoked():
                self._set_state(ActorState.LISTENING)
            case Close(reason):
                await self._finish_turn(turn, TerminalReason(reason))   # endpoint closes are terminal reasons
            case Commit():
                await self._commit(turn, decision)

    async def _commit(self, turn: _Turn, commit: Commit) -> None:
        timeline = self._timeline(turn.runtime)
        if timeline is None or not timeline.mic.covers(commit.start, commit.end):
            await self._finish_turn(turn, TerminalReason.INTERRUPTED)
            return
        turn.commit = commit
        if turn.trigger in REPLY_KINDS and turn.expectation is not None:
            self._settle(turn.expectation, FollowUp.ANSWERED)   # only a committed reply answers (§9.1)
        turn.committed_at = time.monotonic()
        turn.commit_frontier = timeline.mic.frontier
        fit = self.uplink.clock_fit(StreamId.MIC, timeline.lease.capture_epoch) if self.uplink is not None else None
        turn.utterance_end_ns = fit.to_mono(commit.boundary) if fit is not None else None
        turn.coverage = (turn.runtime.echo_known / turn.runtime.echo_total) if turn.runtime.echo_total else None
        self._set_state(ActorState.COMMITTED)
        pcm = timeline.mic.read(commit.start, commit.end)
        turn.wake_clip = self._wake_clip(turn, timeline)
        turn.afe_evidence, turn.afe_series = _afe_snapshot(turn.runtime, turn.candidate, _utterance_span(turn))
        turn.timings["audio_ms"] = round((commit.end - commit.start) * 1000 / SAMPLE_RATE)
        if turn.runtime.afe is not None and self._config().get("saveUtterances"):
            turn.capture = self._capture(turn, timeline)
        await self._open_response_lease(turn, commit)
        await self._close_uplink(turn.runtime.lease_id, LeaseEnd.COMMITTED)
        self._release_runtime(turn.runtime.lease_id)
        turn.task = self._spawn(self._run_committed(turn, commit, pcm))

    def _current(self, turn: _Turn) -> bool:
        return turn.terminal is None and self._turn is turn

    async def _run_committed(self, turn: _Turn, commit: Commit, pcm: np.ndarray) -> None:
        """§16.7 voice turn order after commit: local command, STT, route, intent→TTS. Never resubmits."""
        if commit.redecode_required:
            spec = turn.utterance.spec
            try:
                if commit.source == TextSource.SERVER and spec.pause_server is not None:
                    # The committed text is the pause server's words: re-decode with the same server.
                    text = await self.deps.worker.transcribe_span(pcm, spec.pause_server)
                    redecoded = pause_command(spec, text, commit.wake_window)
                else:
                    tokens, seconds = await self.deps.worker.decode_span(pcm)
                    redecoded = redecoded_command(spec, tokens, seconds)
            except SpeechWorkerError:
                if self._current(turn):
                    await self._finish_turn(turn, TerminalReason.INTERRUPTED)
                return
            if not self._current(turn):
                return
            if redecode_differs(commit.text, redecoded):
                await self._finish_turn(turn, TerminalReason.RETRY)
                return
        if commit.local_action is not None:
            await self._run_local(turn, commit, commit.local_action)
            return
        self._set_state(ActorState.THINKING)
        ns = bool(self._config().get("nsAsr"))
        loop = asyncio.get_running_loop()
        audio = await loop.run_in_executor(None, lambda: stt_copy(pcm, gain=asr_gain_for(turn.wake_db), ns=ns))
        turn.stt_copy = audio
        if not self._current(turn):
            return
        heard = audio + STT_TAIL_SILENCE   # a last word with no silence after it is dropped
        if turn.trigger == UtteranceKind.HA_REPLY:
            turn.outcome = TurnOutcome.HA
            await self._consume_run(turn, self.deps.esphome_reply(heard))
            return
        try:
            pipeline = await self.deps.pipeline_id()
            started = time.monotonic()
            final = await self.deps.ha.run_stt(pipeline, self.deps.ha_device_id(), heard)
        except (HaUnavailable, HaError) as exc:
            log.warning("[%s] HA STT failed: %s", self.device_id, exc)
            if self._current(turn):
                await self._finish_turn(turn, TerminalReason.STT_FAILED)
            return
        if not self._current(turn):
            return
        turn.timings["stt_ms"] = round((time.monotonic() - started) * 1000)
        _mark(turn, TurnStage.STT, started)
        turn.stt_raw = final
        text = final_command_text(
            final, wake_initiated=turn.trigger == UtteranceKind.WAKE, streaming_window=commit.wake_window,
            wake_phrase=turn.model.wake_phrase if turn.model is not None else "",
        ).strip()
        turn.stt_text = text
        if not text:
            await self._finish_turn(turn, TerminalReason.EMPTY_TRANSCRIPT)
            return
        exp = turn.expectation
        if exp is not None and exp.choices:
            await self._answer_choice(turn, exp, exp.choices, text)
            return
        alarm = echomuse_grammar.parse_alarm(text)
        if alarm is not None:
            await self._alarm(turn, alarm)
            return
        question = echomuse_grammar.parse_alarm_query(text)
        if question is not None:
            await self._alarm_query(turn, question)
            return
        cancel = echomuse_grammar.parse_timer_cancel(text)
        if cancel is not None and (cancel.all or (cancel.start is None and cancel.name is None)) \
                and await self._cancel_timer(turn, cancel):
            return
        turn.outcome = TurnOutcome.HA
        self.awaiting_intent = True
        started = time.monotonic()
        try:
            run = await self.deps.ha.run_intent_tts(pipeline, self.deps.ha_device_id(), text,
                                                    turn.conversation_id)
        except (HaUnavailable, HaError) as exc:
            # Nothing was dispatched, so nothing can have executed.
            log.warning("[%s] HA intent run not sent: %s", self.device_id, exc)
            self.awaiting_intent = False
            if self._current(turn):
                await self._finish_turn(turn, TerminalReason.HA_ERROR)
            return
        turn.run = run
        await self._consume_run(turn, run, sent=started)

    async def _consume_run(self, turn: _Turn, events: AsyncIterator[RunEvent], *,
                           sent: float | None = None) -> None:
        """Intent result, response audio, and continuation for one dispatched run (§7, §16.7)."""
        sent = time.monotonic() if sent is None else sent
        iterator = events.__aiter__()
        intent: IntentEnded | None = None
        tts: TtsReady | None = None
        playback: asyncio.Task[PlaybackEnd] | None = None
        dispatched = sent
        intent_at: float | None = None

        def start_by() -> float:
            # §7: response audio starts ≤ RESPONSE_START_S after intent-end. A response
            # streamed before then is bounded by the intent limit until intent-end tightens it.
            if intent_at is None:
                return dispatched + HA_TIMEOUT_S + RESPONSE_START_S
            return intent_at + RESPONSE_START_S

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
                        _mark(turn, TurnStage.STT, sent)
                        sent = time.monotonic()
                    elif isinstance(event, IntentEnded):
                        intent = event
                    elif isinstance(event, TtsReady):
                        tts = event
                        _tts_ready(turn, sent)
                        if event.streamed and playback is None:
                            playback = self._spawn(self._play_response(turn, event.url, start_by))
                    elif isinstance(event, RunLost):
                        await self._finish_turn(turn, TerminalReason.OUTCOME_UNKNOWN)
                        return
                    elif isinstance(event, (RunFailed, RunRejected, RunEnded)):
                        await self._finish_turn(turn, TerminalReason.HA_ERROR)
                        return
        except TimeoutError:
            await self._close_events(events)
            if self._current(turn):
                await self._finish_turn(turn, TerminalReason.HA_TIMEOUT)
            return
        except StopAsyncIteration:
            if self._current(turn):
                await self._finish_turn(turn, TerminalReason.OUTCOME_UNKNOWN)
            return
        self.awaiting_intent = False
        intent_at = time.monotonic()
        turn.timings["intent_ms"] = round((time.monotonic() - sent) * 1000)
        _mark(turn, TurnStage.INTENT, sent, intent_at)
        turn.response_text = intent.speech or None
        turn.response_type = intent.response_type
        turn.intent_local = intent.processed_locally
        turn.conversation_id = intent.conversation_id or turn.conversation_id
        if intent.continue_conversation:
            if self._chain_allowed(turn):
                turn.next_expectation = self._new_expectation(UtteranceKind.REPLY, turn.conversation_id,
                                                              turn.turn_id, previous=self._chain_parent(turn),
                                                              origin=turn)
                await self._watch_reply(turn)     # a streamed response can already be audible
            else:
                turn.continuation = FollowUp.CHAIN_LIMIT
        if tts is None and intent.speech:
            try:
                async with asyncio.timeout(RESPONSE_START_S):
                    while tts is None:
                        event = await iterator.__anext__()
                        if isinstance(event, TtsReady):
                            tts = event
                            _tts_ready(turn, sent)
                        elif isinstance(event, RunLost):
                            if self._current(turn):
                                await self._finish_turn(turn, TerminalReason.OUTCOME_UNKNOWN)
                            return
                        elif isinstance(event, (RunFailed, RunRejected)):
                            if self._current(turn):
                                await self._finish_turn(turn, TerminalReason.HA_ERROR)
                            return
                        elif isinstance(event, RunEnded):
                            break
            except (TimeoutError, StopAsyncIteration):
                pass
            if tts is None:
                await self._close_events(events)
                if self._current(turn):
                    await self._finish_turn(turn, TerminalReason.RESPONSE_TIMEOUT)
                return
        if not self._current(turn):
            return
        if tts is not None and playback is None:
            playback = self._spawn(self._play_response(turn, tts.url, start_by))
        reason = await playback if playback is not None else FinishReason.DRAINED
        if not self._current(turn):
            return
        if reason not in (FinishReason.DRAINED, FinishReason.CANCELLED):
            await self._finish_turn(turn, TerminalReason.RESPONSE_TIMEOUT)
            return
        exp = turn.next_expectation
        if exp is None or reason == FinishReason.CANCELLED:
            if exp is not None:
                # A cancelled prompt invalidates its expectation (§9.1); an early answer would
                # already have taken the turn over, so nothing owns the next utterance.
                self._settle(exp, FollowUp.PROMPT_CANCELLED)
                if exp is self._expectation:
                    await self._drop_expectation(FollowUp.PROMPT_CANCELLED)
            await self._finish_turn(turn, TerminalReason.COMPLETED, feedback=False)
            return
        await self._continue(turn, exp)

    @staticmethod
    async def _close_events(events: AsyncIterator[RunEvent]) -> None:
        if isinstance(events, _Abandonable):
            events.abandon()
        elif isinstance(events, AsyncGenerator):
            try:
                await events.aclose()
            except Exception:
                pass

    async def _play_response(self, turn: _Turn, url: str,
                             start_by: Callable[[], float] | None = None) -> PlaybackEnd:
        """The turn's response as dialog output; the reply lease opens when its audio starts (§9.1).
        `start_by`: as `_play_dialog`'s."""
        async def started() -> None:
            turn.audible_at = time.monotonic()
            if turn.committed_at is not None:
                turn.timings["first_audio_ms"] = round((turn.audible_at - turn.committed_at) * 1000)
            if TurnStage.TTS in turn.stages:
                _mark(turn, TurnStage.TTS, turn.audible_at, turn.audible_at)
            self._set_state(ActorState.SPEAKING)
            await self._watch_reply(turn)

        def bind(playback: Playback) -> None:
            turn.playback = playback

        reason = await self._play_dialog(turn.turn_id, turn.generation, url, bind=bind, started=started,
                                         start_by=start_by, fence=lambda: self._current(turn))
        turn.playback_reason = reason
        if turn.audible_at is not None:
            turn.timings["playback_ms"] = round((time.monotonic() - turn.audible_at) * 1000)
            _mark(turn, TurnStage.REPLY, turn.audible_at)
        return reason

    async def _watch_reply(self, turn: _Turn) -> None:
        """An audible response that asks a question: open its reply lease so early answers are
        watched while it plays (§9.1). Input focus waits for the drain: taking it now would cancel
        the question itself on the device (§6.2)."""
        exp = turn.next_expectation
        if (exp is None or exp is self._expectation or turn.audible_at is None
                or turn.playback is None or turn.playback.done):
            return
        exp.prompt = turn.playback
        self._expectation = exp
        await self._open_expectation_lease(exp)

    async def _continue(self, turn: _Turn, exp: _Expectation) -> None:
        """Turn the drained response into a live reply expectation, then close the turn."""
        self._expectation = exp
        if exp.runtime is None:
            await self._open_expectation_lease(exp)
        if exp.runtime is None:
            self._settle(exp, FollowUp.NO_MIC)
            self._expectation = None
            await self._release_owner(exp.owner)
            await self._finish_turn(turn, TerminalReason.COMPLETED, feedback=False)
            return
        # Input focus passes to the expectation before the turn releases its own: no restore blip.
        await self._hold_reply_focus(exp)
        await self._finish_turn(turn, TerminalReason.COMPLETED, feedback=False)
        await self._begin_window(exp)

    async def _hold_reply_focus(self, exp: _Expectation) -> None:
        """Dialog-input focus for a live expectation whose prompt has drained (§6.2 "Expected reply")."""
        if exp is self._expectation and exp.runtime is not None:
            await self._acquire_focus(exp.owner, Focus.DIALOG_INPUT, exp.generation)

    async def _begin_window(self, exp: _Expectation) -> None:
        """After the guarded drain (or without prompt audio) the reply turn opens at once, its
        trigger at the mic frontier (§9.1, §16.6); before the lease has any mic, at its first
        released cell."""
        if exp is not self._expectation or exp.runtime is None:
            return
        await self._hold_reply_focus(exp)
        self._set_state(ActorState.ARMED)
        exp.window_opened = asyncio.get_running_loop().time()
        timeline = self._timeline(exp.runtime)
        drain = timeline.mic.frontier if timeline is not None else None
        if drain is None:
            exp.awaiting_audio = True
            log.info("[%s] reply window for %s waits for the microphone", self.device_id,
                     exp.origin.turn_id if exp.origin is not None else exp.source)
            return
        await self._open_reply(exp, drain)

    # --- local commands, alarms, clarifications ------------------------------------------------

    async def _run_local(self, turn: _Turn, commit: Commit, action: LocalAction) -> None:
        """§6.3 step 3: execute on the captured occurrence without HA."""
        turn.outcome = TurnOutcome.LOCAL_COMMAND
        turn.stt_text = commit.text
        if action == LocalAction.STOP:        # dialog context: the wake already cancelled the output
            await self._finish_turn(turn, TerminalReason.COMPLETED, feedback=False)
            return
        link = self.link
        if link is None:
            await self._finish_turn(turn, TerminalReason.SESSION_LOST)
            return
        body = _AlertAct(
            op_id=str(uuid.uuid5(uuid.NAMESPACE_URL, f"{turn.turn_id}|{action}")),
            target_id=turn.context_target,
            action=action,
            source=ActSource.VOICE,
        )
        try:
            ack = await link.request(MessageType.ALERT_ACT, body, generation=turn.generation,
                                     timeout=LOCAL_ACT_TIMEOUT_S)
        except Exception as exc:
            log.warning("[%s] alert.act unanswered: %s", self.device_id, exc)
            if self._current(turn):
                await self._finish_turn(turn, TerminalReason.OUTCOME_UNKNOWN)
            return
        if ack.status == AckStatus.REJECTED:
            # unknown_target/already_handled: the captured occurrence already ended (queue moved
            # on or it expired). Anything else means the ring is still going.
            error = ack.error
            log.log(logging.INFO if error in ("unknown_target", "already_handled") else logging.WARNING,
                    "[%s] alert.act %s on %s rejected: %s", self.device_id, action, turn.context_target, error)
        if self._current(turn):
            await self._finish_turn(turn, TerminalReason.COMPLETED, feedback=False)

    async def _alarm(self, turn: _Turn, parsed: AlarmParse) -> None:
        """§10.5 voice alarm through the alert engine; op_id = UUIDv5 of the turn."""
        turn.outcome = TurnOutcome.ALARM
        if parsed.missing == MissingSlot.AMPM:
            exp = self._new_expectation(UtteranceKind.REPLY, turn.conversation_id, turn.turn_id,
                                        previous=self._chain_parent(turn), choices=AMPM_CHOICES,
                                        pending=_AmPmPending(parsed, turn.turn_id), origin=turn)
            turn.outcome = TurnOutcome.CLARIFICATION
            turn.response_text = LINE_AMPM
            await self._finish_turn(turn, TerminalReason.COMPLETED, feedback=False)
            self._spawn(self._prompt(exp, LINE_AMPM))
            return
        await self._apply_alarm(turn, parsed, turn.turn_id)

    async def _apply_alarm(self, turn: _Turn, parsed: AlarmParse, origin_turn_id: str) -> None:
        op_id = str(uuid.uuid5(uuid.NAMESPACE_URL, origin_turn_id))
        alerts = self.deps.alerts
        clock = None if parsed.hour24 is None else f"{parsed.hour24:02d}:{parsed.minute or 0:02d}"
        result: AlertResult
        try:
            if parsed.action == AlarmAction.SET:
                if clock is None:        # a set parse has its time once AM/PM is known
                    result = AlertResult(ok=False, error="alarm time is incomplete")
                else:
                    days = sorted(parsed.days) if isinstance(parsed.days, frozenset) else ()
                    on_date = await self._alarm_date(parsed.days)
                    result = await alerts.set_alarm(self.device_id, clock, days, on_date=on_date,
                                                    source=JournalSource.VOICE, op_id=op_id)
            elif parsed.action == AlarmAction.CANCEL_ALL:
                result = await alerts.cancel_alarm(self.device_id, all_alarms=True, source=JournalSource.VOICE,
                                                   op_id=op_id)
            elif clock is not None:
                result = await alerts.cancel_alarm(self.device_id, time=clock, source=JournalSource.VOICE,
                                                   op_id=op_id)
            else:
                result = await self._cancel_only_alarm(op_id)
        except HaUnavailable:
            result = AlertResult(ok=False, error="Home Assistant is unreachable")
        if not self._current(turn):
            return
        if not result.get("ok") and result.get("error") not in ("no matching alarm", "which alarm"):
            log.warning("[%s] voice alarm failed: %s", self.device_id, result.get("error"))
            await self._finish_turn(turn, TerminalReason.HA_ERROR)
            return
        if parsed.action == AlarmAction.SET:
            line = em_alert_speech.alarm_set_line(parsed, result, datetime.now(timezone.utc))
        else:
            line = em_alert_speech.alarm_cancel_line(result)
        await self._answer(turn, line)

    async def _cancel_only_alarm(self, op_id: str) -> AlertResult:
        """"cancel my alarm" names no time: it cancels only when exactly one alarm exists."""
        listed = self.deps.alerts.list_alarms(self.device_id)
        alarms = listed.get("alarms") or []
        schedules = {a["schedule_id"]: a for a in alarms}
        if not listed.get("ok"):
            return listed
        if len(schedules) != 1:
            return AlertResult(ok=False, error="which alarm" if schedules else "no matching alarm")
        due = next(iter(schedules.values()))["due"]
        return await self.deps.alerts.cancel_alarm(self.device_id, time=datetime.fromisoformat(due).strftime("%H:%M"),
                                                   source=JournalSource.VOICE, op_id=op_id)

    async def _alarm_date(self, days: frozenset[Weekday] | RelativeDay | None) -> date | None:
        if not isinstance(days, RelativeDay):
            return None
        today = datetime.now(ZoneInfo(await self.deps.ha.time_zone())).date()
        return today if days == RelativeDay.TODAY else today + timedelta(days=1)

    async def _alarm_query(self, turn: _Turn, query: AlarmQuery) -> None:
        """Alarm questions, answered from the alert engine without a conversation agent (§10.5).
        Timer questions are HA's own local `HassTimerStatus` intent."""
        turn.outcome = TurnOutcome.ALARM_QUERY
        alarms = self.deps.alerts.list_alarms(self.device_id)
        await self._answer(turn, em_alert_speech.alarms_line(alarms.get("alarms") or [], datetime.now(timezone.utc),
                                                             next_only=query.next_only)
                           if alarms.get("ok") else em_alert_speech.LINE_ALARMS_UNAVAILABLE)

    async def _timers(self, turn: _Turn) -> tuple[list[HaTimer], list[HaTimer]] | None:
        """(this speaker's timers, every timer in HA) from HA's own `HassTimerStatus`, which
        reports every device's timers; None when HA could not say."""
        device = self.deps.ha_device_id()
        started = time.monotonic()
        try:
            response = await self.deps.ha.handle_intent("HassTimerStatus", {}, device)
        except (HaUnavailable, HaError) as exc:
            log.warning("[%s] HassTimerStatus failed: %s", self.device_id, exc)
            return None
        turn.timings["intent_ms"] = round((time.monotonic() - started) * 1000)
        _mark(turn, TurnStage.INTENT, started)
        turn.response_type = response.get("response_type")
        slots = response.get("speech_slots") or {}
        every = list(slots.get("timers") or [])
        return [t for t in every if t.get("device_id") == device], every

    async def _cancel_timer(self, turn: _Turn, cancel: TimerCancel) -> bool:
        """§10.8: the cancels HA's own intents cannot answer. HA's `HassCancelTimer` response
        cannot say which timer it cancelled, so a bare "cancel the timer" is resolved here:
        this speaker's only timer is cancelled and named ("5 second timer cancelled"),
        several ask which one. "Cancel all timers" cancels only this speaker's and names
        them (HA's `HassCancelAllTimers` cancels every timer in HA). False: HA could not
        list the timers, so HA handles the sentence as an ordinary turn."""
        listed = await self._timers(turn)
        if not self._current(turn) or listed is None:
            return not self._current(turn)
        mine, every = listed
        turn.outcome = TurnOutcome.TIMER
        if cancel.all:
            await self._cancel_timers(turn, mine, every)
        elif len(mine) > 1:
            await self._ask_which_timer(turn, mine)
        elif mine:
            await self._cancel_one_timer(turn, mine[0], every)
        else:
            await self._answer(turn, em_alert_speech.LINE_NO_TIMERS)
        return True

    async def _ask_which_timer(self, turn: _Turn, timers: list[HaTimer]) -> None:
        """ "Which one? Your 5 minute timer or your 10 minute timer?", with a reply window."""
        line = em_alert_speech.which_timer_line(timers)
        exp = self._new_expectation(UtteranceKind.REPLY, turn.conversation_id, turn.turn_id,
                                    previous=self._chain_parent(turn), choices=em_timers.choices(timers),
                                    pending=_WhichTimerPending(timers, em_alert_speech.LINE_WHICH_TIMER_AGAIN),
                                    origin=turn)
        turn.outcome = TurnOutcome.CLARIFICATION
        turn.response_text = line
        await self._finish_turn(turn, TerminalReason.COMPLETED, feedback=False)
        self._spawn(self._prompt(exp, line))

    async def _cancel_one_timer(self, turn: _Turn, timer: HaTimer, every: list[HaTimer]) -> None:
        device = self.deps.ha_device_id()
        slots = em_timers.cancel_slots(timer, every, device)
        if slots is None:
            twins = sum(1 for t in every if t.get("device_id") == device
                        and em_timers.matches(t, em_timers.start_of(timer), timer.get("name") or None))
            await self._answer(turn, em_alert_speech.timers_indistinct_line(timer, twins))
            return
        done = await self._run_timer_cancel(turn, "HassCancelTimer", slots)
        if done is not None:
            await self._answer(turn, em_alert_speech.timer_cancelled_line(timer) if done
                               else em_alert_speech.LINE_TIMER_NOT_CANCELLED)

    async def _cancel_timers(self, turn: _Turn, mine: list[HaTimer], every: list[HaTimer]) -> None:
        """Cancel every timer of this speaker. HA's `HassCancelAllTimers` cancels every timer in
        HA, so it runs only when all of them are this speaker's; otherwise one at a time."""
        if not mine:
            await self._answer(turn, em_alert_speech.LINE_NO_TIMERS)
            return
        if len(mine) == len(every):
            done = await self._run_timer_cancel(turn, "HassCancelAllTimers", {})
            if done is not None:
                await self._answer(turn, em_alert_speech.timers_cancelled_line(mine if done else [], len(mine)))
            return
        device = self.deps.ha_device_id()
        cancelled: list[HaTimer] = []
        for timer in mine:
            slots = em_timers.cancel_slots(timer, every, device)
            if slots is None:
                continue
            done = await self._run_timer_cancel(turn, "HassCancelTimer", slots)
            if done is None:
                return
            if done:
                every = [t for t in every if t.get("id") != timer.get("id")]
                cancelled.append(timer)
        await self._answer(turn, em_alert_speech.timers_cancelled_line(cancelled, len(mine)))

    async def _run_timer_cancel(self, turn: _Turn, intent: str, slots: Mapping[str, object]) -> bool | None:
        """Run one cancel intent: True done, False refused (HA no longer finds the timer, it
        just finished), None once the turn has ended. A cancel whose request may have
        reached HA without an answer is never resent: the turn ends `outcome_unknown`."""
        started = time.monotonic()
        try:
            await self.deps.ha.handle_intent(intent, slots, self.deps.ha_device_id())
        except HaUnavailable as exc:
            log.warning("[%s] %s outcome unknown: %s", self.device_id, intent, exc)
            if self._current(turn):
                await self._finish_turn(turn, TerminalReason.OUTCOME_UNKNOWN)
            return None
        except HaError as exc:
            log.warning("[%s] %s %s refused: %s", self.device_id, intent, slots, exc)
            _mark(turn, TurnStage.INTENT, started)
            return False if self._current(turn) else None
        _mark(turn, TurnStage.INTENT, started)
        return True if self._current(turn) else None

    async def _answer(self, turn: _Turn, line: str) -> None:
        """EchoMuse's own answer (a confirmation or status) as the turn's spoken response,
        so the turn's row records what was said and how it played (§11.3)."""
        turn.response_text = line
        started = time.monotonic()
        url = await self._tts(line)
        if not self._current(turn):
            return
        if url is None:
            turn.playback_reason = FinishReason.FAILED
            self._cue(Cue.ERROR_ANIM)
        else:
            turn.timings["tts_url_ms"] = round((time.monotonic() - started) * 1000)
            _mark(turn, TurnStage.TTS, started)
            reason = await self._play_response(turn, url)
            if not self._current(turn):
                return
            if reason not in (FinishReason.DRAINED, FinishReason.CANCELLED):
                self._cue(Cue.ERROR_ANIM)
        await self._finish_turn(turn, TerminalReason.COMPLETED, feedback=False)

    async def _answer_choice(self, turn: _Turn, exp: _Expectation, choices: tuple[Choice, ...], text: str) -> None:
        """§16.6 Choices: select, re-prompt once, or leave it."""
        value = echomuse_grammar.match_choice(text, choices)
        pending = exp.pending_operation
        if isinstance(pending, _AmPmPending) and isinstance(value, Meridiem):
            parsed = pending.parse
            hour = (parsed.hour12 or 12) % 12 + (12 if value == Meridiem.PM else 0)
            completed = AlarmParse(parsed.action, hour, parsed.minute or 0, parsed.days, None,
                                   parsed.hour12, value)
            turn.outcome = TurnOutcome.ALARM
            await self._apply_alarm(turn, completed, pending.turn_id)
            return
        if isinstance(pending, _WhichTimerPending):
            offered = pending.timers
            if value is None:
                # A full command answers too: "cancel the 5 minute timer", "cancel both timers".
                cancel = echomuse_grammar.parse_timer_cancel(text)
                if cancel is not None:
                    picked = [i for i, t in enumerate(offered) if em_timers.matches(t, cancel.start, cancel.name)]
                    value = em_timers.TimerChoice.ALL if cancel.all else picked[0] if len(picked) == 1 else None
            if isinstance(value, int):
                await self._cancel_chosen_timers(turn, offered, value)
                return
            if value == em_timers.TimerChoice.ALL:
                await self._cancel_chosen_timers(turn, offered, em_timers.TimerChoice.ALL)
                return
        turn.outcome = TurnOutcome.CLARIFICATION
        if value is None and not exp.reprompted:
            again = pending.again if isinstance(pending, _WhichTimerPending) else LINE_AMPM_AGAIN
            retry = self._new_expectation(UtteranceKind.REPLY, exp.conversation_id, exp.originating_turn_id,
                                          previous=exp, choices=exp.choices, pending=exp.pending_operation,
                                          origin=turn)
            retry.reprompted = True
            retry.chain_count = exp.chain_count
            turn.response_text = again
            await self._finish_turn(turn, TerminalReason.COMPLETED, feedback=False)
            self._spawn(self._prompt(retry, again))
            return
        turn.response_text = LINE_LEFT_IT
        await self._finish_turn(turn, TerminalReason.COMPLETED, feedback=False)
        self._spawn(self._speak(LINE_LEFT_IT, turn.turn_id, turn.generation))

    async def _cancel_chosen_timers(self, turn: _Turn, offered: list[HaTimer],
                                    value: int | em_timers.TimerChoice) -> None:
        """The reply to "which one?": one offered timer (its index), or all of them, as they are now."""
        turn.outcome = TurnOutcome.TIMER
        listed = await self._timers(turn)
        if not self._current(turn):
            return
        if listed is None:
            await self._finish_turn(turn, TerminalReason.HA_ERROR)
            return
        mine, every = listed
        chosen = [offered[value]] if isinstance(value, int) else offered
        ids = {t.get("id") for t in chosen}
        still = [t for t in mine if t.get("id") in ids]
        if not isinstance(value, int):
            await self._cancel_timers(turn, still, every)
        elif still:
            await self._cancel_one_timer(turn, still[0], every)
        else:
            await self._answer(turn, em_alert_speech.timer_gone_line(chosen[0]))

    # --- expectations ---------------------------------------------------------------------------

    def _chain_parent(self, turn: _Turn) -> _Expectation | None:
        """Only no-wake replies count toward the chain (§9.1); a wake or button starts a new chain."""
        return turn.expectation if turn.trigger in REPLY_KINDS else None

    def _chain_allowed(self, turn: _Turn) -> bool:
        parent = self._chain_parent(turn)
        if parent is None:
            return True
        now = asyncio.get_running_loop().time()
        return parent.chain_count < REPLY_CHAIN_MAX and now - parent.chain_started < REPLY_CHAIN_S

    def _new_expectation(self, source: UtteranceKind, conversation_id: str | None, originating_turn_id: str | None,
                         *, previous: _Expectation | None = None, choices: tuple[Choice, ...] | None = None,
                         pending: _PendingOperation | None = None, origin: _Turn | None = None) -> _Expectation:
        now = asyncio.get_running_loop().time()
        if origin is not None:
            origin.continuation = FollowUp.PENDING
        return _Expectation(
            expectation_id=str(uuid.uuid4()), owner=str(uuid.uuid4()), generation=self._generation,
            source=source, conversation_id=conversation_id, originating_turn_id=originating_turn_id,
            chain_started=previous.chain_started if previous is not None else now,
            chain_count=previous.chain_count + 1 if previous is not None else 1,
            choices=choices, pending_operation=pending, origin=origin,
        )

    def _pause_server(self) -> WyomingServer | None:
        """§16.6 pause server from config `pauseAsr`, else Kroko alone. The API validates
        every write, so a bad stored value means the database was edited by hand."""
        raw = self._config().get(PAUSE_ASR_KEY)
        if raw is None:
            return None
        try:
            return parse_pause_asr(raw)
        except ValueError as err:
            log.warning("[%s] %s: %s; using Kroko alone", self.device_id, PAUSE_ASR_KEY, err)
            return None

    def _settle(self, exp: _Expectation, outcome: Continuation) -> None:
        """Record what became of `exp` on the asking turn's row. The first final outcome wins."""
        origin = exp.origin
        if origin is None or origin.continuation != FollowUp.PENDING:
            return
        origin.continuation = outcome
        log.info("[%s] follow-up to turn %s: %s", self.device_id, origin.turn_id, outcome)
        if origin.persisted is not None:
            self._persists.add(self._spawn(self._record_continuation(origin, origin.persisted, outcome)))

    async def _record_continuation(self, turn: _Turn, persisted: asyncio.Task[int | None],
                                   outcome: Continuation) -> None:
        row_id = await asyncio.shield(persisted)
        if row_id is None:
            return
        try:
            await self.deps.record_continuation(row_id, outcome)
        except Exception:
            log.exception("[%s] follow-up outcome of turn %s not recorded", self.device_id, turn.turn_id)

    async def _open_expectation_lease(self, exp: _Expectation) -> None:
        """The `reply` uplink lease: live mic, cells, and reference (§9.1). No focus: dialog-input
        focus cancels current dialog output on the device (§6.2), so `_hold_reply_focus` takes it
        only once the prompt has drained or an early answer has cut it off."""
        if exp.runtime is not None or self.uplink is None or self.link is None or self._muted:
            return
        epoch = self.uplink.streams.current(StreamId.MIC)
        if epoch is None:
            return
        lease_id = str(uuid.uuid4())
        streams: dict[StreamId, int | None] = {StreamId.MIC: None, StreamId.CELLS: None, StreamId.REFERENCE: None}
        if self.uplink.afe_metadata:
            streams[StreamId.AFE] = None
        await self._lease_message(self.uplink.open(lease_id, LeaseReason.REPLY, exp.owner, epoch, streams))
        model = self.deps.registry.for_config(self._config())
        await self.deps.worker.load_wake_graph(model.graph_sha256)
        self.deps.worker.open_lease(self.device_id, epoch, lease_id, model.graph_sha256)
        runtime = self._new_runtime(lease_id)
        runtime.assembler.require_echo_from(0)
        exp.runtime = runtime

    async def _prompt(self, exp: _Expectation, text: str) -> None:
        """An EchoMuse question: TTS-only prompt, then its reply window."""
        if self.link is None:
            self._settle(exp, TerminalReason.SESSION_LOST)
            return
        self._expectation = exp
        self._set_state(ActorState.SPEAKING)
        url = await self._tts(text)
        if exp is not self._expectation:
            return
        if url is None:
            self._cue(Cue.ERROR_ANIM)
            await self._end_expectation(TerminalReason.HA_ERROR, outcome=FollowUp.PROMPT_FAILED)
            return

        async def started() -> None:
            await self._open_expectation_lease(exp)

        def bind(playback: Playback) -> None:
            exp.prompt = playback

        reason = await self._play_dialog(exp.owner, exp.generation, url, bind=bind, started=started,
                                         drained=lambda: self._hold_reply_focus(exp),
                                         fence=lambda: exp is self._expectation)
        if exp is not self._expectation:
            return
        if reason != FinishReason.DRAINED or exp.runtime is None:
            # A failed prompt invalidates its expectation (§9.1).
            outcome = (FollowUp.PROMPT_CANCELLED if reason == FinishReason.CANCELLED
                       else FollowUp.NO_MIC if reason == FinishReason.DRAINED else FollowUp.PROMPT_FAILED)
            await self._end_expectation(TerminalReason.RESPONSE_TIMEOUT if reason != FinishReason.CANCELLED
                                        else TerminalReason.INTERRUPTED, outcome=outcome)
            return
        await self._begin_window(exp)

    async def _end_expectation(self, reason: TerminalReason, *, outcome: Continuation | None = None) -> None:
        """Close the live expectation without a turn; a silent window never dispatches (§9.1)."""
        if self._expectation is None:
            return
        await self._drop_expectation(outcome or reason)
        self._emit(ActorEvent(ActorEventKind.TERMINAL, self.state, reason, self._dialog_active))
        if self._turn is None:
            self._set_state(ActorState.IDLE)

    async def _drop_expectation(self, outcome: Continuation) -> None:
        exp, self._expectation = self._expectation, None
        if exp is None:
            return
        self._settle(exp, outcome)
        if exp.prompt is not None and not exp.prompt.done:
            await exp.prompt.cancel("expectation_closed")
        if exp.runtime is not None:
            await self._close_uplink(exp.runtime.lease_id, LeaseEnd.CLOSED)
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
                                               announcement=True) != FinishReason.DRAINED:
                        return
                if not item.start_conversation:
                    await self._play_dialog(owner, generation, item.url, announcement=True)
                    return
                exp = self._new_expectation(UtteranceKind.HA_REPLY, None, None)
                self._expectation = exp

                async def started() -> None:
                    await self._open_expectation_lease(exp)

                def bind(playback: Playback) -> None:
                    exp.prompt = playback

                reason = await self._play_dialog(owner, generation, item.url, announcement=True, bind=bind,
                                                 started=started, drained=lambda: self._hold_reply_focus(exp),
                                                 fence=lambda: exp is self._expectation)
                if exp is not self._expectation:
                    return
                if reason != FinishReason.DRAINED or exp.runtime is None:
                    await self._end_expectation(TerminalReason.INTERRUPTED)
                    return
                await self._begin_window(exp)
        finally:
            if not item.future.done():
                item.future.set_result(None)

    # --- dialog output -------------------------------------------------------------------------

    async def _play_dialog(self, owner: str, generation: int, url: str, *, announcement: bool = False,
                           bind: Callable[[Playback], None] | None = None,
                           started: Callable[[], Awaitable[None]] | None = None,
                           drained: Callable[[], Awaitable[None]] | None = None,
                           start_by: Callable[[], float] | None = None,
                           fence: Callable[[], bool] = lambda: True) -> PlaybackEnd:
        """Play `url` as dialog output under a dialog_output lease for `owner`.

        Returns the finish reason (`drained`, `cancelled`, `failed`, `underrun`),
        `response_timeout` for §7's start/progress/length limits, or `fenced`.
        `start_by`: the monotonic time the audio must start by, re-read while waiting so
        a later intent-end can tighten it; None: RESPONSE_START_S after the stream opens.
        `drained` runs after a drain and before the dialog_output lease is released,
        so focus a prompt hands on never lapses in between.
        """
        render = self.render
        if render is None:
            return FinishReason.FAILED
        await self._acquire_focus(owner, Focus.DIALOG_OUTPUT, generation)
        try:
            try:
                playback = await render.play_stream(SourceClass.DIALOG_OUTPUT, _stream_url(url),
                                                    generation=generation, announcement=announcement)
            except Exception as exc:
                log.warning("[%s] dialog output failed to start: %s", self.device_id, exc)
                return FinishReason.FAILED
            self._dialog_playback = playback
            if bind is not None:
                bind(playback)
            began = time.monotonic()
            deadline = start_by if start_by is not None else (lambda: began + RESPONSE_START_S)
            while not playback.started.done():
                remaining = deadline() - time.monotonic()
                if remaining <= 0:
                    await playback.cancel(DialogEnd.RESPONSE_TIMEOUT)
                    return DialogEnd.RESPONSE_TIMEOUT
                await asyncio.wait({playback.started}, timeout=min(PLAYBACK_POLL_S, remaining))
            if playback.started.exception() is not None:
                return playback.finished.result()["reason"] if playback.done else FinishReason.FAILED
            if not fence():
                return DialogEnd.FENCED
            if started is not None:
                await started()
            audible = last_at = time.monotonic()
            last = playback.last_progress
            while not playback.done:
                await asyncio.wait({playback.finished}, timeout=PLAYBACK_POLL_S)
                if not fence():
                    return DialogEnd.FENCED
                now = time.monotonic()
                if playback.last_progress is not last:
                    last, last_at = playback.last_progress, now
                elif not playback.done and now - last_at >= RESPONSE_STALL_S:
                    await playback.cancel(DialogEnd.RESPONSE_TIMEOUT)
                    return DialogEnd.RESPONSE_TIMEOUT
                if not playback.done and now - began >= RESPONSE_TOTAL_S:
                    await playback.cancel(DialogEnd.RESPONSE_TIMEOUT)
                    return DialogEnd.RESPONSE_TIMEOUT
            reason = playback.finished.result()["reason"]
            # Whether the whole answer was heard: frames the device completed against frames sent.
            log.info("[%s] dialog output %s: %.2f s of %.2f s sent completed, %.2f s audible",
                     self.device_id, reason, playback.completed_frames / RENDER_RATE,
                     playback.sent_frames / RENDER_RATE, time.monotonic() - audible)
            if reason == FinishReason.DRAINED and drained is not None and fence():
                await drained()
            return reason
        finally:
            await self._release_owner(owner, only=Focus.DIALOG_OUTPUT)

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
                self._cue(Cue.ERROR_ANIM)
            return
        reason = await self._play_dialog(owner, generation, url, fence=lambda: generation == self._generation)
        if reason not in (FinishReason.DRAINED, FinishReason.CANCELLED, DialogEnd.FENCED):
            self._cue(Cue.ERROR_ANIM)

    # --- close, feedback, persistence ------------------------------------------------------------

    async def _finish_turn(self, turn: _Turn, reason: TerminalReason, *, feedback: bool = True) -> None:
        """Enter CLOSING exactly once: fence, release owned leases, persist, give §7 feedback."""
        if turn.terminal is not None:
            return
        turn.terminal = reason
        if turn.trigger in REPLY_KINDS and turn.expectation is not None and turn.commit is None:
            # A reply that never committed did not answer its question: a silent window is the
            # question's `reply_timeout`, any other end its own reason (§9.1).
            self._settle(turn.expectation, TerminalReason.REPLY_TIMEOUT if reason == TerminalReason.NO_INPUT
                         else reason)
        if turn.run is not None:
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
        if turn.afe_evidence is None:
            turn.afe_evidence, turn.afe_series = _afe_snapshot(turn.runtime, turn.candidate, _utterance_span(turn))
        await self._close_uplink(turn.runtime.lease_id, LeaseEnd.CLOSED)
        self._release_runtime(turn.runtime.lease_id)
        if turn.response_lease is not None:
            turn.response_lease.close_at = min(turn.response_lease.close_at,
                                               asyncio.get_running_loop().time() + RESPONSE_LEASE_TAIL_S)
        next_exp = turn.next_expectation
        if next_exp is not None and reason != TerminalReason.COMPLETED:
            # A question that was never fully asked invalidates its expectation (§9.1).
            outcome: Continuation = FollowUp.PROMPT_FAILED if reason in FOLLOWUP_PROMPT_FAILED else reason
            self._settle(next_exp, outcome)
            if next_exp is self._expectation:
                await self._drop_expectation(outcome)
        if next_exp is not None and next_exp is not self._expectation:
            if next_exp.runtime is not None:
                await self._close_uplink(next_exp.runtime.lease_id, LeaseEnd.CLOSED)
                self._release_runtime(next_exp.runtime.lease_id)
                next_exp.runtime = None
            await self._release_owner(next_exp.owner)
        await self._release_owner(turn.turn_id)
        self.awaiting_intent = False
        if turn.trigger == UtteranceKind.WAKE and reason != TerminalReason.SUPERSEDED:
            self.deps.arbiter.release(self.device_id)
        if self._turn is turn:
            self._turn = None
        self._set_state(ActorState.CLOSING)
        self._emit(ActorEvent(ActorEventKind.TERMINAL, self.state, reason, self._dialog_active))
        turn.persisted = self._spawn(self._persist(turn))
        self._persists.add(turn.persisted)
        if feedback:
            self._feedback(turn, reason)
        if self._turn is None:
            self._set_state(ActorState.ARMED if self._expectation is not None else ActorState.IDLE)

    def _feedback(self, turn: _Turn, reason: TerminalReason) -> None:
        """Exactly the §7 terminal feedback table; a reply window nobody answered ends without a cue."""
        if reason == TerminalReason.NO_INPUT:
            if turn.trigger not in REPLY_KINDS:
                self._cue(Cue.NOSPEECH_ANIM)
        elif reason in ERROR_CUE_REASONS:
            self._cue(Cue.ERROR_ANIM)
        elif reason in ERROR_LINE_REASONS:
            self._cue(Cue.ERROR_ANIM)
            self._spawn(self._speak(LINE_ERROR, turn.turn_id, turn.generation))
        elif reason == TerminalReason.TOO_LONG:
            self._spawn(self._speak(LINE_TOO_LONG, turn.turn_id, turn.generation))
        elif reason == TerminalReason.OUTCOME_UNKNOWN:
            self._spawn(self._speak(LINE_UNKNOWN, turn.turn_id, turn.generation))
        elif reason in CLARIFY_REASONS:
            if self._chain_allowed(turn):
                exp = self._new_expectation(UtteranceKind.REPLY, turn.conversation_id, turn.turn_id,
                                            previous=self._chain_parent(turn), origin=turn)
                self._spawn(self._prompt(exp, LINE_SORRY))
            else:
                self._spawn(self._speak(LINE_SORRY, turn.turn_id, turn.generation))

    def _wake_clip(self, turn: _Turn, timeline: LeaseTimeline) -> _Clip | None:
        """Accepted candidate support −300 ms … support_end, canonical PCM."""
        candidate = turn.candidate
        if candidate is None:
            return None
        a = max(timeline.mic.floor, candidate.support_start - PREROLL)
        b = candidate.support_end or candidate.open_sample
        if b <= a or not timeline.mic.covers(a, b):
            return None
        return _Clip(a, timeline.mic.read(a, b).tobytes())

    def _capture(self, turn: _Turn, timeline: LeaseTimeline) -> _Clip | None:
        """The committed lease's canonical mic for the turn recording: from where the turn's chart
        starts (or the lease's first contiguous sample, if later) through the newest contiguous
        sample. The response lease takes the mic on from its end."""
        assert turn.commit is not None
        run = timeline.mic.known.containing(turn.commit.start)
        chart = series_range(_support(turn.candidate), _utterance_span(turn))
        if run is None or chart is None:
            return None
        a = max(run[0], chart[0])
        return _Clip(a, timeline.mic.read(a, run[1]).tobytes())

    async def _persist(self, turn: _Turn) -> int | None:
        """The turn row with its §11.3 decision trace; utterance WAV, wake clip and turn recording
        when enabled.
        Returns the row id (None when the row was not written)."""
        candidate, commit = turn.candidate, turn.commit
        now = asyncio.get_running_loop().time()
        row = TurnRow(
            ts=time.time() - (now - turn.opened),
            trigger=turn.trigger,
            wake_model=turn.model.wake_phrase if turn.model is not None else None,
            wake_score=candidate.peak if candidate is not None else None,
            wake_threshold=candidate.threshold if candidate is not None else None,
            outcome=turn.outcome,
            asr_text=turn.utterance.heard or None,
            stt_raw=turn.stt_raw,
            stt_text=turn.stt_text,
            response_text=turn.response_text,
            response_type=turn.response_type,
            intent_local=turn.intent_local,
            total_ms=round((now - turn.opened) * 1000),
            stt_ms=turn.timings.get("stt_ms"),
            intent_ms=turn.timings.get("intent_ms"),
            tts_url_ms=turn.timings.get("tts_url_ms"),
            playback_ms=turn.timings.get("playback_ms"),
            playback_reason=turn.playback_reason,
            audio_ms=turn.timings.get("audio_ms"),
            endpoint_ms=(round((commit.decided_at - commit.boundary) * 1000 / SAMPLE_RATE)
                         if commit is not None else None),
            endpoint_class=commit.completeness if commit is not None else None,
            wake_model_sha256=turn.model.graph_sha256 if turn.model is not None else None,
            # A decision on the pause server's words is a different policy (§16.6).
            policy_hash=self.deps.worker.policy_hash + (
                turn.utterance.spec.pause_server.policy_suffix if turn.utterance.spec.pause_server else ""),
            wake_attribution=(None if candidate is None else
                              WakeAttribution.VERIFIED if candidate.producing_sound else WakeAttribution.IDLE),
            reference_coverage=turn.coverage,
            commit_route=commit.route if commit is not None else None,
            terminal_reason=turn.terminal,
            commit_id=commit.commit_id if commit is not None else None,
            turn_uuid=turn.turn_id,
            conversation_id=turn.conversation_id,
            reply_to=turn.expectation.originating_turn_id if turn.expectation is not None else None,
            continuation=turn.continuation,
            first_audio_ms=turn.timings.get("first_audio_ms"),
            response_latency_ms=_response_latency_ms(turn),
        )
        trace = json.dumps(_trace(turn, row, turn.utterance.trace(), turn.afe_evidence), default=str,
                           separators=(",", ":"))
        log.info("[%s] turn %s trace %s", self.device_id, turn.turn_id, trace)
        row["decision_trace"] = trace
        row["afe_evidence"] = turn.afe_evidence.dumps() if turn.afe_evidence is not None else None
        series = turn.afe_series
        response = turn.response_lease
        if response is not None:
            # The row waits for the response lease, until RESPONSE_LEASE_TAIL_S past the turn's end.
            await response.done
            series = _afe_series(turn, response.timeline.afe) or series
        cfg = self._config()
        # The turn recording is played against the chart, so only a turn with one keeps it.
        recording = _turn_recording(turn) if series is not None and cfg.get("saveUtterances") else None
        if series is not None:
            # Where the recordings and the processing sit on the chart's timeline.
            audio = (_Clip(commit.start, turn.stt_copy) if commit is not None and turn.stt_copy else None)
            series = replace(series, wake_clip=turn.wake_clip.span if turn.wake_clip else None,
                             audio_clip=audio.span if audio else None,
                             recording=recording.span if recording else None, stages=_stage_spans(turn))
            row["afe_series"] = series.dumps()
        else:
            row["afe_series"] = None
        try:
            row_id = await self.deps.persist_turn(row)
        except Exception:
            log.exception("[%s] turn row not persisted", self.device_id)
            return None
        loop = asyncio.get_running_loop()
        saves: tuple[tuple[bytes | None, Callable[[str, int, bytes], str | None],
                           Callable[[int, str | None], None] | None], ...] = (
            (turn.stt_copy if cfg.get("saveUtterances") else None, em_recordings.save, em_db.set_turn_audio),
            (turn.wake_clip.pcm if turn.wake_clip and cfg.get("saveWakeClips") else None, em_wakeclips.save,
             em_db.set_turn_wake),
            (recording.pcm if recording else None, _save_turn_recording, None),   # placed by the series
        )
        for pcm, save, attach in saves:
            if not pcm:
                continue
            try:
                name = await loop.run_in_executor(None, save, self.device_id, row_id, pcm)
                if name and attach is not None:
                    await loop.run_in_executor(None, attach, row_id, name)
            except Exception:
                log.exception("[%s] turn %s audio not saved", self.device_id, row_id)
        return row_id

    async def _persist_candidate(self, candidate: _Candidate, reason: TerminalReason,
                                 afe: AfeEvidence | None, series: AfeSeries | None) -> None:
        """A rejected candidate is a turn row with its attribution reason and no audio (§11.3)."""
        try:
            await self.deps.persist_turn(TurnRow(
                ts=time.time(), trigger=UtteranceKind.WAKE, wake_model=candidate.model.wake_phrase,
                wake_score=candidate.peak, wake_threshold=candidate.threshold,
                total_ms=round((asyncio.get_running_loop().time() - candidate.received) * 1000),
                wake_model_sha256=candidate.model.graph_sha256,
                policy_hash=self.deps.worker.policy_hash,
                wake_attribution=reason, terminal_reason=reason,
                afe_evidence=afe.dumps() if afe is not None else None,
                afe_series=series.dumps() if series is not None else None,
            ))
        except Exception:
            log.exception("[%s] rejected candidate not persisted", self.device_id)

    # --- wire helpers -----------------------------------------------------------------------------

    async def _send(self, msg_type: MessageType, body: Mapping[str, object], generation: int) -> None:
        link = self.link
        if link is None or link.closed:
            return
        try:
            await link.send(msg_type, body, generation=generation)
        except Exception as exc:
            log.debug("[%s] %s not sent: %s", self.device_id, msg_type, exc)

    async def _lease_message(self, message: LeaseMessage) -> None:
        # LeaseCommand values are the uplink message types.
        await self._send(MessageType(message.type), message.body, message.generation)

    async def _close_uplink(self, lease_id: str, reason: LeaseEnd) -> None:
        if self.uplink is None:
            return
        lease = self.uplink.leases.get(lease_id)
        if lease is not None and lease.ended is None:
            await self._lease_message(self.uplink.leases.close(lease_id, reason))

    async def _open_response_lease(self, turn: _Turn, commit: Commit) -> None:
        """Carry a committed turn's AFE records, and while `saveUtterances` is on its mic, on through
        its response (§4.4): a `turn` lease taking `afe` from where the committed lease's records
        reached (the commit's end before any arrived) and `mic` from the end of `turn.capture`,
        opened before the committed lease closes so both run on unbroken. Only after a lease that
        took `afe`. Evidence for the turn's chart and recording; nothing reads it to decide."""
        uplink, store = self.uplink, turn.runtime.afe
        lease = uplink.leases.get(turn.runtime.lease_id) if uplink is not None else None
        if uplink is None or store is None or lease is None:
            return
        streams: dict[StreamId, int | None] = {
            StreamId.AFE: store.frontier * AFE_PERIOD_SAMPLES if store.frontier is not None else commit.end}
        if turn.capture is not None:
            streams[StreamId.MIC] = turn.capture.end
        lease_id = str(uuid.uuid4())
        await self._lease_message(uplink.open(lease_id, LeaseReason.TURN, turn.turn_id, lease.capture_epoch,
                                              streams))
        timeline = uplink.timelines[lease_id]
        if turn.capture is not None:
            timeline.set_utterance_start(turn.capture.end)   # retention anchor: the whole response is kept
        loop = asyncio.get_running_loop()
        response = _ResponseLease(lease_id, timeline, loop.time() + RESPONSE_LEASE_MAX_S, loop.create_future())
        self._response_leases[lease_id] = response
        turn.response_lease = response

    async def _end_response_lease(self, response: _ResponseLease, *, close: bool = True) -> None:
        """End a response lease, sending `uplink.close` when `close` (not for one that has already
        ended on the device). What it received stays with the turn for the row."""
        if self._response_leases.pop(response.lease_id, None) is None:
            return
        if close:
            await self._close_uplink(response.lease_id, LeaseEnd.CLOSED)
        if self.uplink is not None:
            self.uplink.release(response.lease_id)
        response.done.set_result(None)

    async def _end_all_response_leases(self, *, close: bool) -> None:
        for response in list(self._response_leases.values()):
            await self._end_response_lease(response, close=close)

    async def _acquire_focus(self, owner: str, focus: Focus, generation: int) -> None:
        if any(f.owner == owner and f.focus == focus for f in self._focus.values()):
            return
        lease_id = str(uuid.uuid4())
        self._focus[lease_id] = _Focus(lease_id, owner, focus, generation, asyncio.get_running_loop().time())
        await self._send(MessageType.FOCUS_ACQUIRE, {"lease_id": lease_id, "owner": owner, "focus": focus,
                                                     "ttl_ms": FOCUS_TTL_MS}, generation)
        self._update_dialog_active()

    async def _release_owner(self, owner: str, only: Focus | None = None) -> None:
        """Release exactly the named owner's leases (§16.1: never a blanket cleanup)."""
        for lease_id, focus in list(self._focus.items()):
            if focus.owner == owner and (only is None or focus.focus == only):
                del self._focus[lease_id]
                await self._send(MessageType.FOCUS_RELEASE, {"lease_id": lease_id}, focus.generation)
        self._update_dialog_active()

    # --- events -------------------------------------------------------------------------------------

    def _update_dialog_active(self) -> None:
        active = bool(self._focus)
        if active != self._dialog_active:
            self._dialog_active = active
            self._emit(ActorEvent(ActorEventKind.DIALOG_FOCUS, self.state, None, active))

    def _set_state(self, state: ActorState) -> None:
        if state != self.state:
            self.state = state
            self._emit(ActorEvent(ActorEventKind.STATE, state, None, self._dialog_active))

    def _cue(self, cue: Cue) -> None:
        self._emit(ActorEvent(ActorEventKind.CUE, self.state, cue, self._dialog_active))

    def _emit(self, event: ActorEvent) -> None:
        for listener in list(self._listeners):
            try:
                result = listener(event)
                if inspect.isawaitable(result):
                    future = asyncio.ensure_future(result)
                    self._listener_futures.add(future)
                    future.add_done_callback(self._listener_futures.discard)
            except Exception:
                log.exception("[%s] actor listener failed", self.device_id)


def _utterance_span(turn: _Turn) -> tuple[int, int] | None:
    """The committed span, else the closed utterance's audio through its evidence frontier."""
    commit = turn.commit
    if commit is not None:
        return commit.start, commit.end
    start, end = turn.utterance.spec.start, turn.utterance.frontier
    return (start, end) if end > start else None


def _afe_snapshot(runtime: _Runtime, candidate: _Candidate | None,
                  utterance: tuple[int, int] | None) -> tuple[AfeEvidence | None, AfeSeries | None]:
    """Native AFE evidence around the candidate's support window (support_start → support_end,
    else the opening hop's end) and over `utterance`, and the per-period series behind it.
    (None, None) — unavailable — when the lease took no `afe` stream (the device lacks
    afe_metadata_v1); a part without data is None inside the evidence, and a series without
    any record is None. Read only to record evidence: no decision depends on it."""
    if runtime.afe is None:
        return None, None
    support = _support(candidate)
    return turn_evidence(runtime.afe, support, utterance), turn_series(runtime.afe, support, utterance)


def _afe_series(turn: _Turn, response: AfeTimeline) -> AfeSeries | None:
    """The turn's AfeSeries carried on through its response lease's records."""
    if turn.runtime.afe is None:
        return None
    return turn_series(turn.runtime.afe, _support(turn.candidate), _utterance_span(turn), response=response)


def _turn_recording(turn: _Turn) -> _Clip | None:
    """The turn recording: `turn.capture`, and the response lease's mic contiguous from its end.
    Mute and session loss erase the response's mic (UplinkSession.clear), leaving the capture."""
    capture = turn.capture
    if capture is None:
        return None
    response = turn.response_lease
    run = response.timeline.mic.known.containing(capture.end) if response is not None else None
    if response is None or run is None:
        return capture
    return _Clip(capture.start, capture.pcm + response.timeline.mic.read(capture.end, run[1]).tobytes())


def _save_turn_recording(device_id: str, turn_id: int, pcm: bytes) -> str | None:
    return em_recordings.save(device_id, turn_id, pcm, kind=em_recordings.RecordingKind.TURN)


def _mark(turn: _Turn, stage: TurnStage, start: float, end: float | None = None) -> None:
    """Extend `stage` to cover monotonic [start, end] (end: now); a stage run twice spans both."""
    end = time.monotonic() if end is None else end
    was = turn.stages.get(stage)
    turn.stages[stage] = (start, end) if was is None else (min(was[0], start), max(was[1], end))


def _tts_ready(turn: _Turn, sent: float) -> None:
    """HA's TTS URL arrived: the reply's audio can be fetched from now until it is audible."""
    now = time.monotonic()
    turn.timings["tts_url_ms"] = round((now - sent) * 1000)
    _mark(turn, TurnStage.TTS, now, now)


def _stage_spans(turn: _Turn) -> dict[TurnStage, tuple[int, int]]:
    """The turn's stages on the capture timeline: the endpoint wait in sample time; the rest
    mapped from the monotonic clock through the newest mic sample received at the commit. That
    sample is already the uplink's latency old, so they sit early by it; the reply's start also by
    the Dot's output latency (~0.3 s on turn 549, against the AFE playback rise)."""
    commit, at, frontier = turn.commit, turn.committed_at, turn.commit_frontier
    if commit is None:
        return {}
    spans = {TurnStage.ENDPOINT: (commit.boundary, commit.decided_at)}
    if at is not None and frontier is not None:
        spans.update({stage: (frontier + round((a - at) * SAMPLE_RATE), frontier + round((b - at) * SAMPLE_RATE))
                      for stage, (a, b) in turn.stages.items()})
    return spans


def _response_latency_ms(turn: _Turn) -> int | None:
    """The end of the user's last word (the commit boundary) → the response's first frame played,
    both on the device's CLOCK_MONOTONIC, so neither end carries transport latency (§4.2). None: no
    response played, or either end lacks device timing."""
    end, playback = turn.utterance_end_ns, turn.playback
    if end is None or playback is None or playback.first_frame_ns is None:
        return None
    return round((playback.first_frame_ns - end) / 1e6)


def _support(candidate: _Candidate | None) -> tuple[int, int] | None:
    """The candidate's support window: support_start → support_end, else the opening hop's end."""
    return None if candidate is None else (candidate.support_start, candidate.support_end or candidate.open_sample)


def _trace(turn: _Turn, row: TurnRow, utterance: UtteranceTrace, afe: AfeEvidence | None) -> dict[str, object]:
    """The §11.3 decision trace logged and stored with a turn row."""
    candidate = turn.candidate
    wire = candidate.wire if candidate is not None else None
    return {
        "turn_id": turn.turn_id, "generation": turn.generation, "trigger": turn.trigger,
        "candidate_id": wire.candidate_id if wire is not None else None,
        "profile": wire.profile if wire is not None else None,
        "producing_sound": wire.producing_sound if wire is not None else None,
        "chimed": wire.chimed if wire is not None else None,
        "rule": wire.rule.wire() if wire is not None and wire.rule is not None else None,
        "hops": [asdict(h) for h in wire.hops] if wire is not None else None,
        "lease_id": turn.runtime.lease_id, "terminal": turn.terminal,
        "utterance": utterance,
        # A reply that met a gap before any answer listened on with a fresh utterance (§16.6).
        **({"reopened": turn.reopened} if turn.reopened else {}),
        # Evidence only (afe_metadata_v1): null when the device does not report it.
        "afe_evidence": afe.wire() if afe is not None else None,
        **{k: v for k, v in row.items() if k != "ts"},
    }
