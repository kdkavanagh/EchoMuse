"""Controller speech worker: VAD, streaming ASR, wake verification, reference
wake scoring (§3.1, §8.1, §16.6).

The asyncio thread owns the stream state of every audio subscription and submits
jobs; one `ThreadPoolExecutor(2)` runs them. The jobs of one serial work queue (a
subscription's mic analysis, its reference scoring, or one verification) run strictly
in order and never at the same time as each other, so the state each queue
keeps (Silero recurrent state and context, the utterance's decoder stream,
BCResNet smoothing) needs no locks. Model sessions are loaded once and shared
read-only. A remote pause transcription (`em_pause_asr`) runs on its own small
pool while the mic queue's Kroko decode runs, and has a time limit so that
queue still meets its deadline.

The worker only produces evidence (`Observation`s); it never opens, commits,
or closes a turn.
"""

from __future__ import annotations

import asyncio
import collections
import enum
import inspect
import json
import logging
import math
import time
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass, field, replace
from typing import Callable

import numpy as np

import em_pause_asr
from echomuse_grammar import Result as GrammarResult
from em_attribution import SPEECH_POSITIVE_VAD, EchoResult, WakeHop
from em_audio_timeline import (
    FLAG_DISCONTINUITY, FLAG_MUTED, REFERENCE_HOP, ReferenceView, SampleTimeline, StreamId, ceil_to,
)
from em_endpoint_policy import TextStability
from em_pause_asr import PauseDecode, WyomingServer
from em_speech_bundle import Recognizer, RecognizerStream, SpeechBundle, create_recognizer, verify_bundle
from em_wake_phrase import StreamingTranscript, verify_wake
from em_wake_registry import WakeRegistry
from em_wake_scorer import (
    BcresnetScorer, BcresnetSpec, ReferenceCandidate, ReferenceDetector, WINDOW_SAMPLES,
    load_spec, onnx_infer, wake_probability,
)

log = logging.getLogger("echomuse.speech")

SAMPLE_RATE = 16_000
BLOCK_SAMPLES = 1_280                 # one 80 ms capture block (§4.1)
VAD_CELL_SAMPLES = 512                # §16.6 analysis cell
VAD_CONTEXT_SAMPLES = 64
VAD_STATE_SHAPE = (2, 1, 128)
EVIDENCE_GAIN = 10                    # +20 dB evidence copy: every ASR decode (§16.6)
# Silero v5 is level-sensitive and Kroko is not: the VAD reads its own +30 dB
# input, so quiet commands clear RUN_OPEN_VAD (§16.6 "Evidence copy and VAD input", [P3]).
VAD_GAIN = 10 ** 1.5                  # +30 dB VAD input (§16.6)
VERIFICATION_PREROLL = 4_800          # support_start − 300 ms
VERIFICATION_LOOKAHEAD = 7_680        # candidate open + 480 ms
# Internal zeros after every fresh decode (wake verification, span re-decode,
# finalize at a pause), never counted as audio. Kroko decodes 128-frame (1.28 s)
# chunks and each needs 13 more frames of right context, so audio just past a
# chunk edge needs up to 141 frames (1.41 s) of padding before it is decoded.
ASR_FLUSH = 24_000                    # 1.5 s
FINALIZE_PAUSE = 5_120                # 10 VAD cells (320 ms) past the last speech-positive cell
# A remote pause transcription never holds up the mic serial work queue: Kroko's result is reported
# at once and the server's words replace its text when they arrive. An answer
# later than this could no longer shorten even the 1,792 ms pause.
PAUSE_REMOTE_TIMEOUT_S = 1.5
SPAN_REMOTE_TIMEOUT_S = 2.0           # route B / fallback re-decode, outside the serial work queues
BLANK_FRAME_SAMPLES = 640             # one trailing blank frame = 40 ms
JOB_DEADLINE_S = 0.750                # §4.4
ERROR_WINDOW_S = 60.0                 # §8.1: three errors within 60 s
ERRORS_UNAVAILABLE = 3
PROBE_INTERVAL_S = 10.0
PROBE_DEADLINE_S = 1.0
PROBES_TO_RECOVER = 2
POLICY_REVISION = "post_afe_10"


class ObservationSource(enum.StrEnum):
    DEVICE = "device"
    CONTROLLER = "controller"


class ObservationKind(enum.StrEnum):
    """§8.1 observation kinds; the controller-side ones also name the worker component that failed."""

    CANDIDATE = "candidate"
    LEVEL = "level"
    VAD = "vad"
    REFERENCE_SCORE = "reference_score"
    ECHO = "echo"
    ASR = "asr"
    VERIFICATION = "verification"
    ERROR = "error"


class VerificationResult(enum.StrEnum):
    PASS = "pass"
    FAIL = "fail"
    TIMEOUT = "timeout"


# ---------------------------------------------------------------------------
# Observations (§8.1). `None` is "unavailable", never 0/false.
# ---------------------------------------------------------------------------

@dataclass(frozen=True, slots=True)
class CandidatePayload:
    candidate_id: str
    subscription_id: str
    profile: str
    threshold: float
    producing_sound: bool
    first_crossing_sample: int
    support_start: int
    support_end: int | None           # None while the candidate is open
    hops: tuple[WakeHop, ...]


@dataclass(frozen=True, slots=True)
class LevelPayload:
    first_cell_sample: int
    e_db: tuple[float, ...]
    flags: tuple[int, ...]
    source_mask: tuple[int, ...]
    background_db: float | None       # B
    foreground_db: float | None       # F


@dataclass(frozen=True, slots=True)
class VadPayload:
    first_cell_sample: int
    probabilities: tuple[float, ...]  # one per 512-sample cell


@dataclass(frozen=True, slots=True)
class ReferenceScorePayload:
    first_hop_end_sample: int         # reference-epoch sample
    reference_epoch: int
    raw: tuple[float | None, ...]
    smoothed: tuple[float | None, ...]
    candidates: tuple[ReferenceCandidate, ...]


@dataclass(frozen=True, slots=True)
class EchoPayload:
    first_cell_sample: int
    results: tuple[EchoResult, ...]
    lag_samples: int | None


@dataclass(frozen=True, slots=True, eq=False)
class VoiceSnapshot:
    """Silero's recurrent state (2×1×128) and 64-sample context as an utterance's pause
    finalized (§16.6): tuned to that speaker's voice in that room just after they stopped.
    `sample` is the mic frontier the utterance had been fed through then (VAD had analyzed
    every whole cell before it). Restored into a reply subscription as its window opens
    (`SpeechWorker.prime_vad`), it hears an answer from the first cell."""

    state: np.ndarray
    context: np.ndarray
    sample: int


@dataclass(frozen=True, slots=True)
class AsrPayload:
    """`text` is the raw streaming text; `stable_prefix` and `grammar_result`
    are judged on the utterance's transformed (wake-cut) text.

    `finalize_ms` marks a result made at a pause by a fresh decode of the whole
    utterance flushed with zeros (§16.6), and is that decode's wall-clock time. While the
    live stream's tokens are a prefix of it, later results repeat it without
    `finalize_ms`. Whenever that result is reported, `trailing_blank_frames` counts real
    audio since the end of the last speech-positive VAD cell, never the flush: Kroko
    emits the last word and punctuation late, often inside the flush.

    `pause_text` is a Wyoming server's transcript of the same audio (config
    `pauseAsr`), wake word included. The finalized result is reported at once
    without it; from the first result after the server answers, every result
    that repeats the finalized one carries it. It stands in for the text, never
    for the tokens, which stay Kroko's. `pause_decode` reports the request on
    the result where it was answered or dropped, including one that gave no text.

    `voice` is the mic serial work queue's Silero state at a finalize, on that result only."""

    tokens: tuple[str, ...]
    token_emission_sample: tuple[int, ...]
    text: str
    stable_prefix: str | None
    trailing_blank_frames: int
    grammar_result: GrammarResult | None
    finalize_ms: int | None
    pause_text: str | None = None
    pause_decode: PauseDecode | None = None
    voice: VoiceSnapshot | None = None

    @property
    def finalized(self) -> bool:
        return self.finalize_ms is not None


@dataclass(frozen=True, slots=True)
class VerificationPayload:
    """The verifier's verdict on one candidate. `queued_ms` is how long its decode waited for an
    executor thread after the input was complete, `decode_ms` how long the decode itself took;
    both are None when it never ran. A `timeout` whose decode ran past the job deadline still
    carries the text and distance it reached: what the verifier would have said."""

    candidate_id: str
    text: str | None
    alias_distance: float | None
    result: VerificationResult
    queued_ms: int | None = None
    decode_ms: int | None = None


@dataclass(frozen=True, slots=True)
class ErrorPayload:
    component: ObservationKind        # the job kind that failed
    reason: str


Payload = (CandidatePayload | LevelPayload | VadPayload | ReferenceScorePayload | EchoPayload
           | AsrPayload | VerificationPayload | ErrorPayload)


@dataclass(frozen=True, slots=True)
class Observation:
    device_id: str
    capture_epoch: int
    stream: StreamId                  # mic or reference
    source: ObservationSource
    kind: ObservationKind
    through_sample: int               # evidence covers samples before this index
    utterance_id: str | None
    model_revision: str | None
    policy_hash: str | None
    payload: Payload
    computed_at_ms: int               # host time, diagnostics only
    subscription_id: str | None = None   # the audio subscription whose audio this covers


class SpeechWorkerError(Exception):
    pass


def _pause_transcribe(server: WyomingServer, pcm: np.ndarray, through: int, sent: float) -> PauseDecode:
    """The remote pause transcription, on the remote pool. It is optional evidence: any
    failure is reported, and Kroko's result stands alone. Its time counts from `sent`, when
    the pause submitted it, as Kroko's decode of the same pause does, so a wait for the pool
    counts against the server."""
    text: str | None = None
    error: str | None = None
    try:
        text = em_pause_asr.transcribe(server, pcm, timeout=PAUSE_REMOTE_TIMEOUT_S).strip()
    except TimeoutError:
        error = "timeout"
    except Exception as exc:     # network, protocol, or server error
        error = f"{type(exc).__name__}: {exc}"
    return PauseDecode(through, text, round((time.monotonic() - sent) * 1000), error)


@dataclass(frozen=True, slots=True)
class _PendingPause:
    """A pause server request still out: its answer, the audio it covers, when it was sent."""

    answer: Future[PauseDecode]
    through: int
    sent: float

    def drop(self, reason: str) -> PauseDecode:
        self.answer.cancel()      # never sent if it was still queued
        return PauseDecode(self.through, None, round((time.monotonic() - self.sent) * 1000), reason)


@dataclass(frozen=True, slots=True)
class AsrResult:
    """One Kroko result: tokens with emission times (s from stream start) and trailing blank frames."""

    text: str
    tokens: tuple[str, ...]
    timestamps: tuple[float, ...]
    trailing_blanks: int

    @classmethod
    def parse(cls, raw: str) -> AsrResult:
        try:
            result = json.loads(raw)
        except (TypeError, ValueError) as exc:
            raise SpeechWorkerError(f"invalid Kroko result JSON: {exc}") from None
        if not isinstance(result, dict):
            raise SpeechWorkerError("Kroko result JSON is not an object")
        tokens = result.get("tokens", [])
        stamps = result.get("timestamps", [])
        blanks = result.get("num_trailing_blanks")
        if not isinstance(tokens, list) or not isinstance(stamps, list) or len(tokens) != len(stamps):
            raise SpeechWorkerError("Kroko tokens and timestamps disagree")
        if not all(isinstance(t, (int, float)) and math.isfinite(t) for t in stamps):
            raise SpeechWorkerError("Kroko timestamps are not finite")
        if isinstance(blanks, bool) or not isinstance(blanks, int) or blanks < 0:
            raise SpeechWorkerError("Kroko result lacks a valid num_trailing_blanks")
        return cls(str(result.get("text", "")), tuple(str(t) for t in tokens),
                   tuple(float(t) for t in stamps), blanks)


# ---------------------------------------------------------------------------
# Shared models
# ---------------------------------------------------------------------------

@dataclass(frozen=True, slots=True)
class WakeGraph:
    sha256: str
    spec: BcresnetSpec
    infer: Callable[[np.ndarray], np.ndarray]
    reference_threshold: float


class SpeechModels:
    """Immutable sessions: Silero v5 (raw ONNX Runtime), Kroko recognizer,
    and BCResNet graphs by hash. One intra-op thread, sequential execution."""

    def __init__(self, bundle: SpeechBundle):
        import onnxruntime as ort

        opts = ort.SessionOptions()
        opts.intra_op_num_threads = 1
        opts.inter_op_num_threads = 1
        opts.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
        self.vad_session = ort.InferenceSession(str(bundle.vad), opts, providers=["CPUExecutionProvider"])
        inputs = {i.name: i.type for i in self.vad_session.get_inputs()}
        outputs = {o.name for o in self.vad_session.get_outputs()}
        if inputs != {"input": "tensor(float)", "state": "tensor(float)", "sr": "tensor(int64)"} \
                or outputs != {"output", "stateN"}:
            raise SpeechWorkerError(f"Silero v5 graph has unexpected IO {inputs} → {outputs}")
        self.recognizer: Recognizer = create_recognizer(bundle)
        m = bundle.manifest
        self.revision = (f"sherpa-onnx {bundle.package_version}; kroko {m.archive.sha256[:12]}; "
                         f"silero {m.vad.sha256[:12]}")
        self._sr = np.array(SAMPLE_RATE, dtype=np.int64)

    def vad(self, state: "_VadState", cell: np.ndarray) -> float:
        """One 512-sample float32 cell; advances `state` in place."""
        state.window[VAD_CONTEXT_SAMPLES:] = cell
        prob, state_n = self.vad_session.run(
            ["output", "stateN"], {"input": state.window[None, :], "state": state.state, "sr": self._sr})
        p = float(np.asarray(prob).reshape(-1)[0])
        state_n = np.asarray(state_n, dtype=np.float32)
        if not math.isfinite(p) or state_n.shape != VAD_STATE_SHAPE or not np.all(np.isfinite(state_n)):
            raise SpeechWorkerError("Silero emitted non-finite or malformed output")
        state.state = state_n
        state.window[:VAD_CONTEXT_SAMPLES] = cell[-VAD_CONTEXT_SAMPLES:]
        return p

    def decode(self, stream: RecognizerStream, evidence: np.ndarray) -> None:
        """Feed int16 evidence as float32 in [-1, 1] and decode while ready."""
        stream.accept_waveform(SAMPLE_RATE, evidence.astype(np.float32) / np.float32(32768.0))
        while self.recognizer.is_ready(stream):
            self.recognizer.decode_stream(stream)

    def decode_fresh(self, evidence: np.ndarray) -> AsrResult:
        """Greedy decode of int16 evidence by a new stream, fed in 80 ms blocks
        and flushed with `ASR_FLUSH` zeros so its last words are decoded."""
        stream = self.recognizer.create_stream()
        for i in range(0, evidence.size, BLOCK_SAMPLES):
            self.decode(stream, evidence[i:i + BLOCK_SAMPLES])
        self.decode(stream, np.zeros(ASR_FLUSH, dtype=np.int16))
        return self.result(stream)

    def result(self, stream: RecognizerStream) -> AsrResult:
        return AsrResult.parse(self.recognizer.get_result_as_json_string(stream))

    def probe(self) -> None:
        """1 s of zeros through fresh VAD state and a fresh ASR stream."""
        vad = _VadState()
        cell = np.zeros(VAD_CELL_SAMPLES, dtype=np.float32)
        for _ in range(SAMPLE_RATE // VAD_CELL_SAMPLES):
            self.vad(vad, cell)
        stream = self.recognizer.create_stream()
        block = np.zeros(BLOCK_SAMPLES, dtype=np.int16)
        for _ in range(SAMPLE_RATE // BLOCK_SAMPLES):
            self.decode(stream, block)
        self.result(stream)


def load_wake_graph(registry: WakeRegistry, sha256: str) -> WakeGraph:
    model = registry.get(sha256)
    graph, sidecar = registry.graph_path(sha256), registry.sidecar_path(sha256)
    spec = load_spec(graph, sidecar)
    infer, _ = onnx_infer(graph, spec=spec)
    p = wake_probability(infer, spec, np.zeros(spec.window, dtype=np.int16), allow_silence=True)
    if p is None or not math.isfinite(p):
        raise SpeechWorkerError(f"BCResNet {sha256} failed warm-up")
    return WakeGraph(sha256, spec, infer, model.thresholds.reference)


def evidence_copy(canonical_pcm: np.ndarray) -> np.ndarray:
    """Canonical PCM × +20 dB (×10), clamped to int16 (§16.6): what every
    ASR decode reads."""
    pcm = np.asarray(canonical_pcm)
    if pcm.dtype != np.int16 or pcm.ndim != 1:
        raise ValueError("canonical PCM must be 1-D int16")
    return np.clip(pcm.astype(np.int32) * EVIDENCE_GAIN, -32768, 32767).astype(np.int16)


def vad_input(canonical_pcm: np.ndarray) -> np.ndarray:
    """Canonical PCM × +30 dB (×10**1.5), saturated to int16 full scale as the
    evidence copy is, as float32 in [-1, 1]: what Silero reads (§16.6)."""
    pcm = np.asarray(canonical_pcm)
    if pcm.dtype != np.int16 or pcm.ndim != 1:
        raise ValueError("canonical PCM must be 1-D int16")
    scaled = np.clip(pcm.astype(np.float64) * VAD_GAIN, -32768, 32767).astype(np.int16)
    return scaled.astype(np.float32) / np.float32(32768.0)


# ---------------------------------------------------------------------------
# State kept by each serial work queue
# ---------------------------------------------------------------------------

class _VadState:
    """Silero recurrent state plus the 64-sample context, and the cell
    remainder of the block stream (an 80 ms block is 2.5 cells). `prime`
    restores a model state from the first cell at or after its sample
    (§16.6 reply window); a reset of the cell stream leaves it pending."""

    def __init__(self) -> None:
        self.window = np.zeros(VAD_CONTEXT_SAMPLES + VAD_CELL_SAMPLES, dtype=np.float32)
        self.state = np.zeros(VAD_STATE_SHAPE, dtype=np.float32)
        self.pending = np.empty(0, dtype=np.float32)   # VAD input of the incomplete cell
        self.next_sample: int | None = None   # first sample of `pending`
        self.prime: tuple[int, VoiceSnapshot | None] | None = None   # (from sample, state; None: fresh)

    def reset(self) -> None:
        self.window.fill(0)
        self.state = np.zeros(VAD_STATE_SHAPE, dtype=np.float32)
        self.pending = np.empty(0, dtype=np.float32)
        self.next_sample = None

    def snapshot(self, sample: int) -> VoiceSnapshot:
        return VoiceSnapshot(self.state.copy(), self.window[:VAD_CONTEXT_SAMPLES].copy(), sample)

    def restore(self, voice: VoiceSnapshot | None) -> None:
        """The model state and context from `voice`, or fresh ones; the cell stream is untouched."""
        if voice is None:
            self.state = np.zeros(VAD_STATE_SHAPE, dtype=np.float32)
            self.window[:VAD_CONTEXT_SAMPLES] = 0
        else:
            self.state = voice.state.copy()
            self.window[:VAD_CONTEXT_SAMPLES] = voice.context


@dataclass(frozen=True, slots=True)
class _Finalized:
    """A finalize decode's result, emissions as capture-epoch samples; `pause_text`
    is the Wyoming server's transcript of the same audio, once it answered."""

    tokens: tuple[str, ...]
    emissions: tuple[int, ...]
    text: str
    pause_text: str | None = None


@dataclass
class _Utterance:
    utterance_id: str
    start_sample: int
    stream: RecognizerStream
    stability: TextStability
    text_transform: Callable[[StreamingTranscript], str] | None
    grammar: Callable[[str], GrammarResult] | None
    fed_through: int
    evidence: list[np.ndarray] = field(default_factory=list)   # evidence copy of [start_sample, fed_through)
    speech_end: int | None = None        # end of the last speech-positive VAD cell since open
    finalized_end: int | None = None     # the speech end a finalize decode already covered
    finalized: _Finalized | None = None  # reported instead of the live result until the live one adds to it
    pause_server: WyomingServer | None = None   # also transcribes at each pause (config `pauseAsr`)
    pause: _PendingPause | None = None          # the latest finalize's request, until answered or dropped


@dataclass
class _StreamState:
    device_id: str
    capture_epoch: int
    subscription_id: str
    wake: WakeGraph
    vad: _VadState = field(default_factory=_VadState)
    utterance: _Utterance | None = None
    detector: ReferenceDetector | None = None
    reference_epoch: int | None = None
    # Loop-thread bookkeeping (never touched by jobs).
    mic_through: int | None = None
    ref_next: dict[int, int] = field(default_factory=dict)
    dirty: bool = False


@dataclass(frozen=True, slots=True)
class _Job:
    queue_name: str
    component: ObservationKind
    created: float
    run: Callable[[], tuple[Observation, ...]]
    stream_state: _StreamState | None
    stream: StreamId
    through: int
    utterance_id: str | None
    control: bool = False              # state changes: never dropped for age
    candidate_id: str | None = None


_SerialWorkQueue = asyncio.Queue[_Job | None]      # None stops the queue's drain task


@dataclass(frozen=True, slots=True)
class _WorkerError:
    """One error counted toward `speech_unavailable` (§8.1)."""

    at: float
    component: ObservationKind
    reason: str
    device_id: str | None             # None: no subscription (a committed-span re-decode)
    queue_name: str

    def __str__(self) -> str:
        return f"{self.component} on {self.device_id or '-'} ({self.queue_name}): {self.reason}"


# ---------------------------------------------------------------------------
# Worker
# ---------------------------------------------------------------------------

class SpeechWorker:
    """The actor's handle on controller speech inference.

    Lifecycle: `await start()`; for each subscription `open_subscription()` … `close_subscription()`;
    submissions return immediately; results go to `on_observation(obs)` or,
    without that callback, to the `observations` asyncio queue. Availability
    changes arrive in `availability` and `on_availability(bool)`.
    """

    def __init__(
        self,
        registry: WakeRegistry,
        *,
        bundle_dir: str | None = None,
        policy_hash: str = POLICY_REVISION,
        on_observation: Callable[[Observation], object] | None = None,
        on_availability: Callable[[bool], object] | None = None,
        now: Callable[[], float] = time.monotonic,
        job_deadline_s: float = JOB_DEADLINE_S,
        probe_interval_s: float = PROBE_INTERVAL_S,
    ):
        self.registry = registry
        self.bundle_dir = bundle_dir
        self.policy_hash = policy_hash
        self.on_observation = on_observation
        self.on_availability = on_availability
        self._now = now
        self.job_deadline_s = job_deadline_s
        self.probe_interval_s = probe_interval_s
        self.observations: asyncio.Queue[Observation] = asyncio.Queue()
        self.availability: asyncio.Queue[bool] = asyncio.Queue()
        self.available = True
        self.models: SpeechModels | None = None
        self._executor: ThreadPoolExecutor | None = None
        self._wake: dict[str, WakeGraph] = {}
        self._stream_states: dict[str, _StreamState] = {}
        self._serial_work_queues: dict[str, _SerialWorkQueue] = {}
        self._drain_tasks: dict[str, asyncio.Task[None]] = {}
        self._errors: collections.deque[_WorkerError] = collections.deque()
        # `_now()` at the last counted deadline failure: a deadline failure of a job submitted
        # by then is that stall's backlog, reported but not counted again (§8.1).
        self._stall_mark: float | None = None
        self._probe_task: asyncio.Task[None] | None = None
        self._callbacks: set[asyncio.Future[object]] = set()   # awaitables on_availability returned
        self._closed = False
        # Remote pause transcriptions (§16.6): network waits, never model work.
        self._remote = ThreadPoolExecutor(max_workers=2, thread_name_prefix="pause-asr")

    # -- startup / shutdown ------------------------------------------------

    async def start(self) -> None:
        """Verify every bundle hash, load shared sessions, load the active wake
        graph, and warm each model on 1 s of zeros before accepting audio."""
        if self._executor is not None:
            raise SpeechWorkerError("worker already started")
        bundle = verify_bundle(self.bundle_dir)
        self._executor = ThreadPoolExecutor(max_workers=2, thread_name_prefix="speech")
        loop = asyncio.get_running_loop()
        try:
            models = await loop.run_in_executor(self._executor, SpeechModels, bundle)
            self.models = models
            await loop.run_in_executor(self._executor, models.probe)
            await self.load_wake_graph(self.registry.active().graph_sha256)
        except BaseException:
            self._executor.shutdown(wait=True, cancel_futures=True)
            self._executor = None
            self.models = None
            raise

    async def load_wake_graph(self, sha256: str) -> None:
        """Load (once) the registry graph a subscription will name. Subscriptions opened
        earlier keep their graph: revisions change between utterances."""
        if sha256 in self._wake:
            return
        if self._executor is None:
            raise SpeechWorkerError("worker is not started")
        loop = asyncio.get_running_loop()
        self._wake[sha256] = await loop.run_in_executor(
            self._executor, load_wake_graph, self.registry, sha256)

    async def close(self) -> None:
        self._closed = True
        if self._probe_task is not None:
            self._probe_task.cancel()
            await asyncio.gather(self._probe_task, return_exceptions=True)
        for queue in self._serial_work_queues.values():
            queue.put_nowait(None)
        await asyncio.gather(*list(self._drain_tasks.values()), return_exceptions=True)
        self._serial_work_queues.clear()
        self._drain_tasks.clear()
        self._stream_states.clear()
        if self._executor is not None:
            self._executor.shutdown(wait=True, cancel_futures=True)
            self._executor = None
        self._remote.shutdown(wait=False, cancel_futures=True)

    # -- subscriptions ------------------------------------------------------

    def open_subscription(self, device_id: str, capture_epoch: int, subscription_id: str,
                          graph_sha256: str) -> None:
        """Fresh VAD state/context from the subscription's first mic sample; empty
        reference smoothing history from its first reference sample."""
        self._require_started()
        if subscription_id in self._stream_states:
            raise SpeechWorkerError(f"subscription {subscription_id} is already open")
        wake = self._wake.get(graph_sha256)
        if wake is None:
            raise SpeechWorkerError(f"wake graph {graph_sha256} is not loaded")
        self._stream_states[subscription_id] = _StreamState(device_id, capture_epoch, subscription_id, wake)

    def close_subscription(self, subscription_id: str) -> None:
        """Discard the subscription's stream state; queued jobs for it still drain."""
        stream_state = self._stream_states.pop(subscription_id, None)
        if stream_state is None:
            return
        for queue_name in (f"mic:{subscription_id}", f"reference:{subscription_id}"):
            queue = self._serial_work_queues.pop(queue_name, None)
            if queue is not None:
                queue.put_nowait(None)

    def reset_mic(self, subscription_id: str) -> None:
        """Epoch change, discontinuity, or mute: reset VAD state and context
        and close any open ASR stream (the actor ends that utterance)."""
        stream_state = self._stream_state(subscription_id)
        stream_state.mic_through = None

        def run() -> tuple[Observation, ...]:
            stream_state.vad.reset()
            stream_state.utterance = None
            return ()
        self._submit(_Job(f"mic:{subscription_id}", ObservationKind.VAD, self._now(), run, stream_state, StreamId.MIC,
                          0, None, control=True))

    def prime_vad(self, subscription_id: str, from_sample: int, voice: VoiceSnapshot | None) -> None:
        """§16.6 reply window: restore `voice` (None: a fresh state) into the subscription's VAD from
        the first cell at or after `from_sample` it has yet to analyze; earlier cells keep the
        state they carried. Ordered with the subscription's mic blocks: blocks submitted before it
        are analyzed first."""
        stream_state = self._stream_state(subscription_id)

        def run() -> tuple[Observation, ...]:
            stream_state.vad.prime = (from_sample, voice)
            return ()
        self._submit(_Job(f"mic:{subscription_id}", ObservationKind.VAD, self._now(), run, stream_state, StreamId.MIC,
                          0, None, control=True))

    # -- utterance ASR ------------------------------------------------------

    def open_utterance(
        self,
        subscription_id: str,
        utterance_id: str,
        start_sample: int,
        trigger_sample: int,
        mic: SampleTimeline,
        *,
        text_transform: Callable[[StreamingTranscript], str] | None = None,
        grammar: Callable[[str], GrammarResult] | None = None,
        pause_server: WyomingServer | None = None,
    ) -> None:
        """Open the utterance's ASR stream at `start_sample` (the pre-roll
        start) and catch it up on the canonical mic already submitted.

        `text_transform` maps the streaming transcript to the text stability
        and grammar judge (the §16.6 wake-phrase cut for wake turns);
        `grammar(text)` supplies `grammar_result`; `pause_server` also
        transcribes the utterance at each pause."""
        stream_state = self._stream_state(subscription_id)
        models = self._require_started()
        through = stream_state.mic_through if stream_state.mic_through is not None else start_sample
        preroll = None
        if through > start_sample:
            if not mic.covers(start_sample, through):
                raise SpeechWorkerError(f"pre-roll [{start_sample}, {through}) is not all known")
            preroll = evidence_copy(mic.read(start_sample, through))
        stream = models.recognizer.create_stream()
        utterance = _Utterance(utterance_id, start_sample, stream, TextStability(trigger_sample),
                               text_transform, grammar, start_sample, pause_server=pause_server)

        def run() -> tuple[Observation, ...]:
            stream_state.utterance = utterance
            if preroll is None:
                return ()
            for i in range(0, preroll.size, BLOCK_SAMPLES):
                models.decode(stream, preroll[i:i + BLOCK_SAMPLES])
            utterance.evidence.append(preroll)
            utterance.fed_through = start_sample + preroll.size
            return (self._asr_observation(models, stream_state, utterance),)
        self._submit(_Job(f"mic:{subscription_id}", ObservationKind.ASR, self._now(), run, stream_state, StreamId.MIC,
                          max(through, start_sample), utterance_id, control=True))

    def close_utterance(self, subscription_id: str) -> None:
        """Commit or close: the stream is dropped, never reused."""
        stream_state = self._stream_state(subscription_id)

        def run() -> tuple[Observation, ...]:
            if stream_state.utterance is not None and stream_state.utterance.pause is not None:
                stream_state.utterance.pause.answer.cancel()
            stream_state.utterance = None
            return ()
        self._submit(_Job(f"mic:{subscription_id}", ObservationKind.ASR, self._now(), run, stream_state, StreamId.MIC,
                          0, None, control=True))

    # -- mic blocks ---------------------------------------------------------

    def submit_mic_block(self, subscription_id: str, first_sample: int, canonical_pcm: np.ndarray,
                         *, flags: int = 0) -> None:
        """One uploaded mic block (canonical int16). Its VAD input and its
        evidence copy for the utterance's ASR are each built once here."""
        stream_state = self._stream_state(subscription_id)
        models = self._require_started()
        pcm = np.asarray(canonical_pcm)
        if pcm.dtype != np.int16 or pcm.ndim != 1 or pcm.size == 0:
            raise ValueError("canonical PCM must be a non-empty 1-D int16 array")
        if flags & (FLAG_DISCONTINUITY | FLAG_MUTED):
            self.reset_mic(subscription_id)
            if flags & FLAG_MUTED:
                return
        vad_in = vad_input(pcm)
        evidence = evidence_copy(pcm)
        end = first_sample + pcm.size
        stream_state.mic_through = end
        utterance_id = stream_state.utterance.utterance_id if stream_state.utterance else None
        self._submit(_Job(f"mic:{subscription_id}", ObservationKind.VAD, self._now(),
                          lambda: self._run_mic(models, stream_state, first_sample, vad_in, evidence),
                          stream_state, StreamId.MIC, end, utterance_id))

    def _run_mic(self, models: SpeechModels, stream_state: _StreamState, first: int,
                 vad_in: np.ndarray, evidence: np.ndarray) -> tuple[Observation, ...]:
        if stream_state.dirty:
            stream_state.vad.reset()
            stream_state.utterance = None
            stream_state.dirty = False
        out: list[Observation] = []
        vad = stream_state.vad
        if vad.next_sample is not None and first != vad.next_sample + vad.pending.size:
            # Missing range: VAD context never crosses a gap (the actor closes
            # any open utterance as interrupted); the ASR stream is dropped.
            vad.reset()
            stream_state.utterance = None
        if vad.next_sample is None:
            skip = (-first) % VAD_CELL_SAMPLES   # cells lie on the epoch's 512 grid
            vad.next_sample = first + skip
            vad.pending = vad_in[skip:]
        else:
            vad.pending = np.concatenate((vad.pending, vad_in))
        n_cells = vad.pending.size // VAD_CELL_SAMPLES
        if n_cells:
            first_cell = vad.next_sample
            cells = vad.pending[:n_cells * VAD_CELL_SAMPLES]
            probs: list[float] = []
            for i in range(n_cells):
                if vad.prime is not None and first_cell + i * VAD_CELL_SAMPLES >= vad.prime[0]:
                    vad.restore(vad.prime[1])
                    vad.prime = None
                probs.append(models.vad(vad, cells[i * VAD_CELL_SAMPLES:(i + 1) * VAD_CELL_SAMPLES]))
            vad.pending = vad.pending[n_cells * VAD_CELL_SAMPLES:]
            vad.next_sample = first_cell + n_cells * VAD_CELL_SAMPLES
            u = stream_state.utterance
            if u is not None:
                for i, p in enumerate(probs):
                    if p >= SPEECH_POSITIVE_VAD:
                        u.speech_end = first_cell + (i + 1) * VAD_CELL_SAMPLES
            out.append(self._observe(stream_state, StreamId.MIC, ObservationKind.VAD, vad.next_sample,
                                     VadPayload(first_cell, tuple(probs)), u.utterance_id if u else None))
        u = stream_state.utterance
        if u is not None:
            end = first + evidence.size
            if first > u.fed_through:
                stream_state.utterance = None   # gap inside the utterance: stream is dead
            elif end > u.fed_through:
                fed = evidence[u.fed_through - first:]
                models.decode(u.stream, fed)
                u.evidence.append(fed)
                u.fed_through = end
                out.append(self._asr_observation(models, stream_state, u))
        return tuple(out)

    def _asr_observation(self, models: SpeechModels, stream_state: _StreamState, u: _Utterance) -> Observation:
        """The block's ASR result (§16.6 finalize at the pause).

        Once VAD has analyzed `FINALIZE_PAUSE` past the last speech-positive
        cell, a fresh stream decodes the whole utterance with the flush, once
        per pause. That result stands in for the live one while the live tokens
        are a prefix of it: the live stream decodes 1.28 s chunks, and its
        trailing-blank count stops at its last chunk edge. The first live result
        that adds or changes a token replaces it. While it stands, its trailing
        blanks are the real audio since the last speech-positive cell, so new
        speech drops them before the live stream has new tokens.

        With a `pause_server`, the same audio also goes to that Wyoming server
        when Kroko decodes, but the result is reported without waiting for it.
        Its transcript is the judged text from the first block after it
        arrives, for as long as Kroko's finalized result stands. A request is
        dropped when new speech replaces that result or a later pause sends
        another.

        A finalize also copies the mic serial work queue's Silero state: the utterance's voice snapshot."""
        analyzed = stream_state.vad.next_sample
        finalize = (u.speech_end is not None and u.speech_end != u.finalized_end
                    and analyzed is not None and analyzed - u.speech_end >= FINALIZE_PAUSE)
        live: AsrResult | None = None
        decode: PauseDecode | None = None
        finalize_ms: int | None = None
        voice: VoiceSnapshot | None = None
        if finalize:
            evidence = np.concatenate(u.evidence)
            if u.pause is not None:
                decode = u.pause.drop("superseded")
                u.pause = None
            sent = time.monotonic()
            if u.pause_server is not None:
                u.pause = _PendingPause(self._remote.submit(_pause_transcribe, u.pause_server, evidence,
                                                            u.fed_through, sent), u.fed_through, sent)
            shadow = models.decode_fresh(evidence)
            finalize_ms = round((time.monotonic() - sent) * 1000)
            u.finalized = _Finalized(shadow.tokens, self._emissions(u, shadow), shadow.text)
            u.finalized_end = u.speech_end
            voice = stream_state.vad.snapshot(u.fed_through)
        elif u.finalized is not None:
            live = models.result(u.stream)
            if live.tokens != u.finalized.tokens[:len(live.tokens)]:
                u.finalized = None
                if u.pause is not None:    # its words would describe audio the speaker has added to
                    decode = u.pause.drop("superseded")
                    u.pause = None
        if decode is None and u.pause is not None and u.pause.answer.done():
            decode = u.pause.answer.result()
            u.pause = None
            if decode.text and u.finalized is not None:
                u.finalized = replace(u.finalized, pause_text=decode.text)
        pause_text: str | None = None
        if u.finalized is not None:
            tokens, emissions, raw_text = u.finalized.tokens, u.finalized.emissions, u.finalized.text
            pause_text = u.finalized.pause_text
            # Real audio since the latest speech-positive cell; token emissions
            # can fall late, even inside the flush, and the flush never counts.
            speech_end = u.speech_end if u.speech_end is not None else u.start_sample
            blanks = max(0, u.fed_through - speech_end) // BLANK_FRAME_SAMPLES
        else:
            result = live if live is not None else models.result(u.stream)
            tokens, emissions, raw_text = result.tokens, self._emissions(u, result), result.text
            blanks = result.trailing_blanks
        if pause_text is not None:
            text = pause_text        # no word timings: the time-based transform cannot apply
        elif u.text_transform is not None:
            text = u.text_transform(StreamingTranscript.from_tokens(tokens, emissions))
        else:
            text = raw_text
        stable = u.stability.push(text, u.fed_through, blanks)
        payload = AsrPayload(tokens, emissions, raw_text, stable.prefix if stable.prefix_sample is not None else None,
                             blanks, None if u.grammar is None else u.grammar(text), finalize_ms,
                             pause_text=pause_text, pause_decode=decode, voice=voice)
        return self._observe(stream_state, StreamId.MIC, ObservationKind.ASR, u.fed_through, payload, u.utterance_id)

    @staticmethod
    def _emissions(u: _Utterance, result: AsrResult) -> tuple[int, ...]:
        return tuple(u.start_sample + round(t * SAMPLE_RATE) for t in result.timestamps)

    # -- echo ----------------------------------------------------------------

    def submit_echo(self, subscription_id: str, first_cell_sample: int, through_sample: int,
                    compare: Callable[[], tuple[tuple[EchoResult, ...], int | None]],
                    utterance_id: str | None = None) -> None:
        """Run the pure per-cell reference comparison (`em_attribution`) on
        the executor, serialized with this subscription's mic analysis."""
        stream_state = self._stream_state(subscription_id)

        def run() -> tuple[Observation, ...]:
            results, lag = compare()
            if not results or any(not isinstance(r, EchoResult) for r in results):
                raise SpeechWorkerError(f"echo comparison returned {results!r}")
            return (self._observe(stream_state, StreamId.MIC, ObservationKind.ECHO, through_sample,
                                  EchoPayload(first_cell_sample, results, lag), utterance_id),)
        self._submit(_Job(f"mic:{subscription_id}", ObservationKind.ECHO, self._now(), run, stream_state, StreamId.MIC,
                          through_sample, utterance_id))

    # -- reference scoring --------------------------------------------------

    def submit_reference(self, subscription_id: str, reference: ReferenceView) -> None:
        """Score every newly complete reference hop (windows ending on the
        epoch's multiples of 2,560) of one reference epoch. Digital-silence
        windows are not scored; a window with missing samples is invalid."""
        stream_state = self._stream_state(subscription_id)
        first, frontier = reference.first_sample, reference.frontier
        if first is None or frontier is None:
            return
        epoch = reference.epoch
        next_end = stream_state.ref_next.get(epoch)
        if next_end is None:
            next_end = ceil_to(first + WINDOW_SAMPLES, REFERENCE_HOP)
        slots: list[tuple[int, np.ndarray | None, bool]] = []
        while next_end <= frontier:
            start = next_end - WINDOW_SAMPLES
            if reference.covers(start, next_end):
                silent = reference.is_silent(start, next_end)
                slots.append((next_end, None if silent else reference.read(start, next_end), silent))
            else:
                slots.append((next_end, None, False))
            next_end += REFERENCE_HOP
        stream_state.ref_next[epoch] = next_end
        if slots:
            self._submit(_Job(f"reference:{subscription_id}", ObservationKind.REFERENCE_SCORE, self._now(),
                              lambda: self._run_reference(stream_state, epoch, slots),
                              stream_state, StreamId.REFERENCE, slots[-1][0], None))

    def _run_reference(self, stream_state: _StreamState, epoch: int,
                       slots: list[tuple[int, np.ndarray | None, bool]]) -> tuple[Observation, ...]:
        detector = stream_state.detector
        if detector is None or stream_state.reference_epoch != epoch:
            detector = stream_state.detector = ReferenceDetector(
                BcresnetScorer(stream_state.wake.infer, stream_state.wake.spec), stream_state.wake.reference_threshold)
            stream_state.reference_epoch = epoch
        raw: list[float | None] = []
        smoothed: list[float | None] = []
        events: list[ReferenceCandidate] = []
        for end, pcm, silent in slots:
            if pcm is None and not silent:
                score = detector.scorer.invalid(end)
                events.extend(detector.push(score, invalid=True))
            else:
                score = detector.scorer.score_window(
                    end, pcm if pcm is not None else np.zeros(WINDOW_SAMPLES, np.int16), digital_silence=silent)
                events.extend(detector.push(score))
            raw.append(score.raw)
            smoothed.append(score.smoothed)
        payload = ReferenceScorePayload(slots[0][0], epoch, tuple(raw), tuple(smoothed), tuple(events))
        return (self._observe(stream_state, StreamId.REFERENCE, ObservationKind.REFERENCE_SCORE, slots[-1][0],
                              payload),)

    # -- wake verification --------------------------------------------------

    def submit_verification(self, subscription_id: str, candidate_id: str, mic: SampleTimeline,
                            support_start: int, open_sample: int, verify_core: str) -> None:
        """Fresh greedy decode of the evidence copy over
        [support_start − 300 ms, open + 480 ms), flushed with `ASR_FLUSH` zeros.
        Call once the mic timeline covers that range."""
        stream_state = self._stream_state(subscription_id)
        models = self._require_started()
        a = max(0, support_start - VERIFICATION_PREROLL)
        b = open_sample + VERIFICATION_LOOKAHEAD
        a = max(a, mic.floor)
        if not mic.covers(a, b):
            raise SpeechWorkerError(f"verification input [{a}, {b}) is not all known")
        evidence = evidence_copy(mic.read(a, b))
        created = self._now()
        self._submit(_Job(f"verification:{candidate_id}", ObservationKind.VERIFICATION, created,
                          lambda: self._run_verification(models, stream_state, candidate_id, evidence, verify_core, b,
                                                         created),
                          stream_state, StreamId.MIC, b, None, candidate_id=candidate_id))

    def _run_verification(self, models: SpeechModels, stream_state: _StreamState, candidate_id: str,
                          evidence: np.ndarray, core: str, through: int, created: float) -> tuple[Observation, ...]:
        queued_ms = round((self._now() - created) * 1000)
        started = time.monotonic()
        text = models.decode_fresh(evidence).text
        decode_ms = round((time.monotonic() - started) * 1000)
        v = verify_wake(text, core)
        payload = VerificationPayload(candidate_id, text, v.distance,
                                      VerificationResult.PASS if v.passed else VerificationResult.FAIL,
                                      queued_ms, decode_ms)
        return (self._observe(stream_state, StreamId.MIC, ObservationKind.VERIFICATION, through, payload),)

    # -- committed-span re-decode ---------------------------------------------

    async def decode_span(self, canonical_pcm: np.ndarray) -> tuple[tuple[str, ...], tuple[float, ...]]:
        """Fresh greedy decode of a committed span's evidence copy, flushed with
        `ASR_FLUSH` internal zeros (§16.6 route B / fallback re-decode). Returns
        tokens and their emission times in seconds from the span start. A
        failure counts toward `speech_unavailable` and raises SpeechWorkerError."""
        models = self._require_started()
        evidence = evidence_copy(canonical_pcm)

        def run() -> tuple[tuple[str, ...], tuple[float, ...]]:
            result = models.decode_fresh(evidence)
            return result.tokens, result.timestamps
        try:
            return await asyncio.get_running_loop().run_in_executor(self._executor, run)
        except Exception as exc:
            self._record_error(ObservationKind.ASR, f"span re-decode: {type(exc).__name__}: {exc}", None, "span")
            raise SpeechWorkerError(f"span re-decode failed: {type(exc).__name__}: {exc}") from exc

    async def transcribe_span(self, canonical_pcm: np.ndarray, server: WyomingServer) -> str:
        """The Wyoming server's transcript of a committed span's evidence copy, for
        the route B / fallback re-decode of text that came from it (§16.6). The
        local models are not involved, so a failure only raises SpeechWorkerError."""
        evidence = evidence_copy(canonical_pcm)
        try:
            return await asyncio.get_running_loop().run_in_executor(
                self._remote, lambda: em_pause_asr.transcribe(server, evidence, timeout=SPAN_REMOTE_TIMEOUT_S))
        except (OSError, em_pause_asr.WyomingError) as exc:
            raise SpeechWorkerError(f"span transcription failed: {type(exc).__name__}: {exc}") from exc

    # -- serial work queues, deadlines, errors, availability --------------------

    def _submit(self, job: _Job) -> None:
        if self._executor is None or self._closed:
            raise SpeechWorkerError("worker is not running")
        queue = self._serial_work_queues.get(job.queue_name)
        if queue is None:
            queue = self._serial_work_queues[job.queue_name] = _SerialWorkQueue()
            self._drain_tasks[job.queue_name] = asyncio.create_task(self._drain(job.queue_name, queue))
        queue.put_nowait(job)

    async def _drain(self, name: str, queue: _SerialWorkQueue) -> None:
        loop = asyncio.get_running_loop()
        one_shot = name.startswith("verification:")
        while True:
            job = await queue.get()
            if job is None:
                break
            if not job.control and self._now() - job.created > self.job_deadline_s:
                await self._fail(job, "job deadline exceeded before it ran", deadline=True)
                continue
            try:
                observations = await loop.run_in_executor(self._executor, job.run)
                if not job.control and self._now() - job.created > self.job_deadline_s:
                    await self._fail(job, "job deadline exceeded", deadline=True, ran=observations)
                    continue
                for obs in observations:
                    await self._emit(obs)
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                await self._fail(job, f"{type(exc).__name__}: {exc}")
            if one_shot and queue.empty():
                break
        if self._serial_work_queues.get(name) is queue:
            del self._serial_work_queues[name]
        if self._drain_tasks.get(name) is asyncio.current_task():
            del self._drain_tasks[name]

    async def _fail(self, job: _Job, reason: str, *, deadline: bool = False,
                    ran: tuple[Observation, ...] = ()) -> None:
        """Report a failed job; a verification past its deadline is also a `timeout` verdict (§16.6),
        carrying the text, distance and timings of its decode when that ran (`ran`, its observations).

        One stall is one error: a missed deadline is counted unless the job was submitted
        by the last counted deadline failure (in any serial work queue, since the GIL and the executor
        are shared), which makes it that stall's backlog. Its error is still reported."""
        now = self._now()
        backlog = deadline and self._stall_mark is not None and job.created <= self._stall_mark
        if deadline and not backlog:
            self._stall_mark = now
        stream_state = job.stream_state
        device_id = None if stream_state is None else stream_state.device_id
        if stream_state is not None:
            if job.stream == StreamId.MIC and not job.candidate_id:
                stream_state.dirty = True   # the mic serial work queue's recurrent state skipped audio
            if job.candidate_id is not None and deadline:
                late = next((o.payload for o in ran if isinstance(o.payload, VerificationPayload)), None)
                verdict = (VerificationPayload(job.candidate_id, None, None, VerificationResult.TIMEOUT)
                           if late is None else replace(late, result=VerificationResult.TIMEOUT))
                await self._emit(self._observe(stream_state, StreamId.MIC, ObservationKind.VERIFICATION, job.through,
                                               verdict))
            await self._emit(self._observe(stream_state, job.stream, ObservationKind.ERROR, job.through,
                                           ErrorPayload(job.component, reason), job.utterance_id))
        if backlog:
            log.debug("speech worker: %s on %s (%s): %s; backlog of the stall at %.3f, not counted",
                      job.component, device_id or "-", job.queue_name, reason, self._stall_mark)
            return
        self._record_error(job.component, reason, device_id, job.queue_name, now)

    def _record_error(self, component: ObservationKind, reason: str, device_id: str | None, queue_name: str,
                      now: float | None = None) -> None:
        error = _WorkerError(self._now() if now is None else now, component, reason, device_id, queue_name)
        self._errors.append(error)
        while self._errors and error.at - self._errors[0].at > ERROR_WINDOW_S:
            self._errors.popleft()
        log.warning("speech worker error: %s (%d within %.0f s)", error, len(self._errors), ERROR_WINDOW_S)
        if self.available and len(self._errors) >= ERRORS_UNAVAILABLE:
            log.warning("speech worker unavailable (speech_unavailable) after %d errors within %.0f s: %s",
                        len(self._errors), ERROR_WINDOW_S, "; ".join(str(e) for e in self._errors))
            self._set_available(False)
            self._probe_task = asyncio.create_task(self._probe_loop())

    async def _probe_loop(self) -> None:
        """While unavailable: every 10 s, 1 s of zeros through VAD and ASR;
        two consecutive probes each finishing without error within 1 s clear it."""
        loop = asyncio.get_running_loop()
        good = 0
        while not self._closed:
            await asyncio.sleep(self.probe_interval_s)
            started = time.monotonic()
            try:
                models = self._require_started()
                await asyncio.wait_for(loop.run_in_executor(self._executor, models.probe),
                                       PROBE_DEADLINE_S)
                ok = time.monotonic() - started < PROBE_DEADLINE_S
            except asyncio.CancelledError:
                raise
            except Exception:
                ok = False
            good = good + 1 if ok else 0
            if good >= PROBES_TO_RECOVER:
                self._errors.clear()
                log.info("speech worker available again: %d consecutive probes passed", good)
                self._set_available(True)
                self._probe_task = None
                return

    def _set_available(self, available: bool) -> None:
        self.available = available
        self.availability.put_nowait(available)
        if self.on_availability is not None:
            value = self.on_availability(available)
            if inspect.isawaitable(value):
                future = asyncio.ensure_future(value)
                self._callbacks.add(future)
                future.add_done_callback(self._callbacks.discard)

    async def _emit(self, obs: Observation) -> None:
        if self.on_observation is None:
            self.observations.put_nowait(obs)
            return
        value = self.on_observation(obs)
        if inspect.isawaitable(value):
            await value

    def _revision(self, stream_state: _StreamState, stream: StreamId) -> str | None:
        if stream == StreamId.REFERENCE:
            return stream_state.wake.sha256
        return None if self.models is None else self.models.revision

    def _observe(self, stream_state: _StreamState, stream: StreamId, kind: ObservationKind, through: int,
                 payload: Payload, utterance_id: str | None = None) -> Observation:
        return Observation(stream_state.device_id, stream_state.capture_epoch, stream, ObservationSource.CONTROLLER,
                           kind, through, utterance_id, self._revision(stream_state, stream), self.policy_hash,
                           payload, int(time.time() * 1000), stream_state.subscription_id)

    def _stream_state(self, subscription_id: str) -> _StreamState:
        stream_state = self._stream_states.get(subscription_id)
        if stream_state is None:
            raise SpeechWorkerError(f"unknown subscription {subscription_id}")
        return stream_state

    def _require_started(self) -> SpeechModels:
        if self._executor is None or self.models is None:
            raise SpeechWorkerError("worker is not started")
        return self.models
