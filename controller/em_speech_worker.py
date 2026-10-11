"""Controller speech worker: VAD, streaming ASR, wake verification, reference
wake scoring (§3.1, §8.1, §16.6).

The asyncio thread owns every per-lease record and submits jobs; one
`ThreadPoolExecutor(2)` runs them. Jobs of one lane (a lease's mic analysis,
its reference scoring, or one verification) run strictly in order and never
concurrently with each other, so per-lane state (Silero recurrent state and
context, the utterance's decoder stream, BCResNet smoothing) needs no locks.
Model sessions are loaded once and shared read-only. A remote pause
transcription (`em_pause_asr`) runs on its own small pool while the lane's
Kroko decode runs, bounded so the lane keeps its deadline.

The worker only produces evidence (`Observation`s); it never opens, commits,
or closes a turn.
"""

from __future__ import annotations

import asyncio
import collections
import enum
import inspect
import json
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

SAMPLE_RATE = 16_000
BLOCK_SAMPLES = 1_280                 # one 80 ms capture block (§4.1)
VAD_CELL_SAMPLES = 512                # §16.6 analysis cell
VAD_CONTEXT_SAMPLES = 64
VAD_STATE_SHAPE = (2, 1, 128)
EVIDENCE_GAIN = 10                    # +20 dB evidence copy (§16.6)
VERIFICATION_PREROLL = 4_800          # support_start − 300 ms
VERIFICATION_LOOKAHEAD = 7_680        # candidate open + 480 ms
# Internal zeros after every fresh decode (wake verification, span re-decode,
# finalize at a pause), never counted as audio. Kroko decodes 128-frame (1.28 s)
# chunks and each needs 13 more frames of right context, so audio just past a
# chunk edge needs up to 141 frames (1.41 s) of padding before it is decoded.
ASR_FLUSH = 24_000                    # 1.5 s
FINALIZE_PAUSE = 5_120                # 10 VAD cells (320 ms) past the last speech-positive cell
# A remote pause transcription never holds the lane: Kroko's result is reported
# at once and the server's words replace its text when they arrive. An answer
# later than this could no longer shorten even the 1,792 ms pause.
PAUSE_REMOTE_TIMEOUT_S = 1.5
SPAN_REMOTE_TIMEOUT_S = 2.0           # route B / fallback re-decode, off the lanes
BLANK_FRAME_SAMPLES = 640             # one trailing blank frame = 40 ms
JOB_DEADLINE_S = 0.750                # §4.4
ERROR_WINDOW_S = 60.0                 # §8.1: three errors within 60 s
ERRORS_UNAVAILABLE = 3
PROBE_INTERVAL_S = 10.0
PROBE_DEADLINE_S = 1.0
PROBES_TO_RECOVER = 2
POLICY_REVISION = "post_afe_7"


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
    lease_id: str
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
    the result where it was answered or dropped, including one that gave no text."""

    tokens: tuple[str, ...]
    token_emission_sample: tuple[int, ...]
    text: str
    stable_prefix: str | None
    trailing_blank_frames: int
    grammar_result: GrammarResult | None
    finalize_ms: int | None
    pause_text: str | None = None
    pause_decode: PauseDecode | None = None

    @property
    def finalized(self) -> bool:
        return self.finalize_ms is not None


@dataclass(frozen=True, slots=True)
class VerificationPayload:
    candidate_id: str
    text: str | None
    alias_distance: float | None
    result: VerificationResult


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
    lease_id: str | None = None       # the uplink lease whose audio this covers


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
    """Canonical PCM × +20 dB (×10), clamped to int16 (§16.6)."""
    pcm = np.asarray(canonical_pcm)
    if pcm.dtype != np.int16 or pcm.ndim != 1:
        raise ValueError("canonical PCM must be 1-D int16")
    return np.clip(pcm.astype(np.int32) * EVIDENCE_GAIN, -32768, 32767).astype(np.int16)


# ---------------------------------------------------------------------------
# Per-lane state
# ---------------------------------------------------------------------------

class _VadState:
    """Silero recurrent state plus the 64-sample context, and the cell
    remainder of the block stream (an 80 ms block is 2.5 cells)."""

    def __init__(self) -> None:
        self.window = np.zeros(VAD_CONTEXT_SAMPLES + VAD_CELL_SAMPLES, dtype=np.float32)
        self.state = np.zeros(VAD_STATE_SHAPE, dtype=np.float32)
        self.pending = np.empty(0, dtype=np.int16)
        self.next_sample: int | None = None   # first sample of `pending`

    def reset(self) -> None:
        self.window.fill(0)
        self.state = np.zeros(VAD_STATE_SHAPE, dtype=np.float32)
        self.pending = np.empty(0, dtype=np.int16)
        self.next_sample = None


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
class _Lease:
    device_id: str
    capture_epoch: int
    lease_id: str
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
    lane: str
    component: ObservationKind
    created: float
    run: Callable[[], tuple[Observation, ...]]
    lease: _Lease | None
    stream: StreamId
    through: int
    utterance_id: str | None
    control: bool = False              # state changes: never dropped for age
    candidate_id: str | None = None


_Lane = asyncio.Queue[_Job | None]      # None stops the lane


# ---------------------------------------------------------------------------
# Worker
# ---------------------------------------------------------------------------

class SpeechWorker:
    """The actor's handle on controller speech inference.

    Lifecycle: `await start()`; per lease `open_lease()` … `close_lease()`;
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
        self._leases: dict[str, _Lease] = {}
        self._lanes: dict[str, _Lane] = {}
        self._lane_tasks: dict[str, asyncio.Task[None]] = {}
        self._errors: collections.deque[float] = collections.deque()
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
        """Load (once) the registry graph a lease will name. Leases opened
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
        for queue in self._lanes.values():
            queue.put_nowait(None)
        await asyncio.gather(*list(self._lane_tasks.values()), return_exceptions=True)
        self._lanes.clear()
        self._lane_tasks.clear()
        self._leases.clear()
        if self._executor is not None:
            self._executor.shutdown(wait=True, cancel_futures=True)
            self._executor = None
        self._remote.shutdown(wait=False, cancel_futures=True)

    # -- leases -------------------------------------------------------------

    def open_lease(self, device_id: str, capture_epoch: int, lease_id: str, graph_sha256: str) -> None:
        """Fresh VAD state/context from the lease's first mic sample; empty
        reference smoothing history from its first reference sample."""
        self._require_started()
        if lease_id in self._leases:
            raise SpeechWorkerError(f"lease {lease_id} is already open")
        wake = self._wake.get(graph_sha256)
        if wake is None:
            raise SpeechWorkerError(f"wake graph {graph_sha256} is not loaded")
        self._leases[lease_id] = _Lease(device_id, capture_epoch, lease_id, wake)

    def close_lease(self, lease_id: str) -> None:
        """Discard the lease's state; queued jobs for it still drain."""
        lease = self._leases.pop(lease_id, None)
        if lease is None:
            return
        for lane in (f"mic:{lease_id}", f"reference:{lease_id}"):
            queue = self._lanes.pop(lane, None)
            if queue is not None:
                queue.put_nowait(None)

    def reset_mic(self, lease_id: str) -> None:
        """Epoch change, discontinuity, or mute: reset VAD state and context
        and close any open ASR stream (the actor ends that utterance)."""
        lease = self._lease(lease_id)
        lease.mic_through = None

        def run() -> tuple[Observation, ...]:
            lease.vad.reset()
            lease.utterance = None
            return ()
        self._submit(_Job(f"mic:{lease_id}", ObservationKind.VAD, self._now(), run, lease, StreamId.MIC, 0, None,
                          control=True))

    # -- utterance ASR ------------------------------------------------------

    def open_utterance(
        self,
        lease_id: str,
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
        lease = self._lease(lease_id)
        models = self._require_started()
        through = lease.mic_through if lease.mic_through is not None else start_sample
        preroll = None
        if through > start_sample:
            if not mic.covers(start_sample, through):
                raise SpeechWorkerError(f"pre-roll [{start_sample}, {through}) is not all known")
            preroll = evidence_copy(mic.read(start_sample, through))
        stream = models.recognizer.create_stream()
        utterance = _Utterance(utterance_id, start_sample, stream, TextStability(trigger_sample),
                               text_transform, grammar, start_sample, pause_server=pause_server)

        def run() -> tuple[Observation, ...]:
            lease.utterance = utterance
            if preroll is None:
                return ()
            for i in range(0, preroll.size, BLOCK_SAMPLES):
                models.decode(stream, preroll[i:i + BLOCK_SAMPLES])
            utterance.evidence.append(preroll)
            utterance.fed_through = start_sample + preroll.size
            return (self._asr_observation(models, lease, utterance),)
        self._submit(_Job(f"mic:{lease_id}", ObservationKind.ASR, self._now(), run, lease, StreamId.MIC,
                          max(through, start_sample), utterance_id, control=True))

    def close_utterance(self, lease_id: str) -> None:
        """Commit or close: the stream is dropped, never reused."""
        lease = self._lease(lease_id)

        def run() -> tuple[Observation, ...]:
            if lease.utterance is not None and lease.utterance.pause is not None:
                lease.utterance.pause.answer.cancel()
            lease.utterance = None
            return ()
        self._submit(_Job(f"mic:{lease_id}", ObservationKind.ASR, self._now(), run, lease, StreamId.MIC, 0, None,
                          control=True))

    # -- mic blocks ---------------------------------------------------------

    def submit_mic_block(self, lease_id: str, first_sample: int, canonical_pcm: np.ndarray,
                         *, flags: int = 0) -> None:
        """One uploaded mic block (canonical int16). The evidence copy is
        built once here and shared by VAD and the utterance's ASR."""
        lease = self._lease(lease_id)
        models = self._require_started()
        pcm = np.asarray(canonical_pcm)
        if pcm.dtype != np.int16 or pcm.ndim != 1 or pcm.size == 0:
            raise ValueError("canonical PCM must be a non-empty 1-D int16 array")
        if flags & (FLAG_DISCONTINUITY | FLAG_MUTED):
            self.reset_mic(lease_id)
            if flags & FLAG_MUTED:
                return
        evidence = evidence_copy(pcm)
        end = first_sample + pcm.size
        lease.mic_through = end
        utterance_id = lease.utterance.utterance_id if lease.utterance else None
        self._submit(_Job(f"mic:{lease_id}", ObservationKind.VAD, self._now(),
                          lambda: self._run_mic(models, lease, first_sample, evidence),
                          lease, StreamId.MIC, end, utterance_id))

    def _run_mic(self, models: SpeechModels, lease: _Lease, first: int,
                 evidence: np.ndarray) -> tuple[Observation, ...]:
        if lease.dirty:
            lease.vad.reset()
            lease.utterance = None
            lease.dirty = False
        out: list[Observation] = []
        vad = lease.vad
        if vad.next_sample is not None and first != vad.next_sample + vad.pending.size:
            # Missing range: VAD context never crosses a gap (the actor closes
            # any open utterance as interrupted); the ASR stream is dropped.
            vad.reset()
            lease.utterance = None
        if vad.next_sample is None:
            skip = (-first) % VAD_CELL_SAMPLES   # cells lie on the epoch's 512 grid
            vad.next_sample = first + skip
            vad.pending = evidence[skip:].copy()
        else:
            vad.pending = np.concatenate((vad.pending, evidence))
        n_cells = vad.pending.size // VAD_CELL_SAMPLES
        if n_cells:
            first_cell = vad.next_sample
            cells = vad.pending[:n_cells * VAD_CELL_SAMPLES].astype(np.float32) / np.float32(32768.0)
            probs = tuple(models.vad(vad, cells[i * VAD_CELL_SAMPLES:(i + 1) * VAD_CELL_SAMPLES])
                          for i in range(n_cells))
            vad.pending = vad.pending[n_cells * VAD_CELL_SAMPLES:]
            vad.next_sample = first_cell + n_cells * VAD_CELL_SAMPLES
            u = lease.utterance
            if u is not None:
                for i, p in enumerate(probs):
                    if p >= SPEECH_POSITIVE_VAD:
                        u.speech_end = first_cell + (i + 1) * VAD_CELL_SAMPLES
            out.append(self._observe(lease, StreamId.MIC, ObservationKind.VAD, vad.next_sample,
                                     VadPayload(first_cell, probs), u.utterance_id if u else None))
        u = lease.utterance
        if u is not None:
            end = first + evidence.size
            if first > u.fed_through:
                lease.utterance = None   # gap inside the utterance: stream is dead
            elif end > u.fed_through:
                fed = evidence[u.fed_through - first:]
                models.decode(u.stream, fed)
                u.evidence.append(fed)
                u.fed_through = end
                out.append(self._asr_observation(models, lease, u))
        return tuple(out)

    def _asr_observation(self, models: SpeechModels, lease: _Lease, u: _Utterance) -> Observation:
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
        another."""
        analyzed = lease.vad.next_sample
        finalize = (u.speech_end is not None and u.speech_end != u.finalized_end
                    and analyzed is not None and analyzed - u.speech_end >= FINALIZE_PAUSE)
        live: AsrResult | None = None
        decode: PauseDecode | None = None
        finalize_ms: int | None = None
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
                             pause_text=pause_text, pause_decode=decode)
        return self._observe(lease, StreamId.MIC, ObservationKind.ASR, u.fed_through, payload, u.utterance_id)

    @staticmethod
    def _emissions(u: _Utterance, result: AsrResult) -> tuple[int, ...]:
        return tuple(u.start_sample + round(t * SAMPLE_RATE) for t in result.timestamps)

    # -- echo ----------------------------------------------------------------

    def submit_echo(self, lease_id: str, first_cell_sample: int, through_sample: int,
                    compare: Callable[[], tuple[tuple[EchoResult, ...], int | None]],
                    utterance_id: str | None = None) -> None:
        """Run the pure per-cell reference comparison (`em_attribution`) on
        the executor, serialized with this lease's mic analysis."""
        lease = self._lease(lease_id)

        def run() -> tuple[Observation, ...]:
            results, lag = compare()
            if not results or any(not isinstance(r, EchoResult) for r in results):
                raise SpeechWorkerError(f"echo comparison returned {results!r}")
            return (self._observe(lease, StreamId.MIC, ObservationKind.ECHO, through_sample,
                                  EchoPayload(first_cell_sample, results, lag), utterance_id),)
        self._submit(_Job(f"mic:{lease_id}", ObservationKind.ECHO, self._now(), run, lease, StreamId.MIC,
                          through_sample, utterance_id))

    # -- reference scoring --------------------------------------------------

    def submit_reference(self, lease_id: str, reference: ReferenceView) -> None:
        """Score every newly complete reference hop (windows ending on the
        epoch's multiples of 2,560) of one reference epoch. Digital-silence
        windows are not scored; a window with missing samples is invalid."""
        lease = self._lease(lease_id)
        first, frontier = reference.first_sample, reference.frontier
        if first is None or frontier is None:
            return
        epoch = reference.epoch
        next_end = lease.ref_next.get(epoch)
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
        lease.ref_next[epoch] = next_end
        if slots:
            self._submit(_Job(f"reference:{lease_id}", ObservationKind.REFERENCE_SCORE, self._now(),
                              lambda: self._run_reference(lease, epoch, slots),
                              lease, StreamId.REFERENCE, slots[-1][0], None))

    def _run_reference(self, lease: _Lease, epoch: int,
                       slots: list[tuple[int, np.ndarray | None, bool]]) -> tuple[Observation, ...]:
        detector = lease.detector
        if detector is None or lease.reference_epoch != epoch:
            detector = lease.detector = ReferenceDetector(BcresnetScorer(lease.wake.infer, lease.wake.spec),
                                                          lease.wake.reference_threshold)
            lease.reference_epoch = epoch
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
        return (self._observe(lease, StreamId.REFERENCE, ObservationKind.REFERENCE_SCORE, slots[-1][0], payload),)

    # -- wake verification --------------------------------------------------

    def submit_verification(self, lease_id: str, candidate_id: str, mic: SampleTimeline,
                            support_start: int, open_sample: int, verify_core: str) -> None:
        """Fresh greedy decode of the evidence copy over
        [support_start − 300 ms, open + 480 ms), flushed with `ASR_FLUSH` zeros.
        Call once the mic timeline covers that range."""
        lease = self._lease(lease_id)
        models = self._require_started()
        a = max(0, support_start - VERIFICATION_PREROLL)
        b = open_sample + VERIFICATION_LOOKAHEAD
        a = max(a, mic.floor)
        if not mic.covers(a, b):
            raise SpeechWorkerError(f"verification input [{a}, {b}) is not all known")
        evidence = evidence_copy(mic.read(a, b))
        self._submit(_Job(f"verification:{candidate_id}", ObservationKind.VERIFICATION, self._now(),
                          lambda: self._run_verification(models, lease, candidate_id, evidence, verify_core, b),
                          lease, StreamId.MIC, b, None, candidate_id=candidate_id))

    def _run_verification(self, models: SpeechModels, lease: _Lease, candidate_id: str, evidence: np.ndarray,
                          core: str, through: int) -> tuple[Observation, ...]:
        text = models.decode_fresh(evidence).text
        v = verify_wake(text, core)
        payload = VerificationPayload(candidate_id, text, v.distance,
                                      VerificationResult.PASS if v.passed else VerificationResult.FAIL)
        return (self._observe(lease, StreamId.MIC, ObservationKind.VERIFICATION, through, payload),)

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
            self._record_error()
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

    # -- lanes, deadlines, errors, availability --------------------------------

    def _submit(self, job: _Job) -> None:
        if self._executor is None or self._closed:
            raise SpeechWorkerError("worker is not running")
        queue = self._lanes.get(job.lane)
        if queue is None:
            queue = self._lanes[job.lane] = _Lane()
            self._lane_tasks[job.lane] = asyncio.create_task(self._lane(job.lane, queue))
        queue.put_nowait(job)

    async def _lane(self, name: str, queue: _Lane) -> None:
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
                    await self._fail(job, "job deadline exceeded", deadline=True)
                    continue
                for obs in observations:
                    await self._emit(obs)
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                await self._fail(job, f"{type(exc).__name__}: {exc}")
            if one_shot and queue.empty():
                break
        if self._lanes.get(name) is queue:
            del self._lanes[name]
        if self._lane_tasks.get(name) is asyncio.current_task():
            del self._lane_tasks[name]

    async def _fail(self, job: _Job, reason: str, *, deadline: bool = False) -> None:
        """Report a failed job; a verification past its deadline is also a `timeout` verdict (§16.6)."""
        lease = job.lease
        if lease is not None:
            if job.stream == StreamId.MIC and not job.candidate_id:
                lease.dirty = True   # the lane's recurrent state skipped audio
            if job.candidate_id is not None and deadline:
                await self._emit(self._observe(
                    lease, StreamId.MIC, ObservationKind.VERIFICATION, job.through,
                    VerificationPayload(job.candidate_id, None, None, VerificationResult.TIMEOUT)))
            await self._emit(self._observe(lease, job.stream, ObservationKind.ERROR, job.through,
                                           ErrorPayload(job.component, reason), job.utterance_id))
        self._record_error()

    def _record_error(self) -> None:
        now = self._now()
        self._errors.append(now)
        while self._errors and now - self._errors[0] > ERROR_WINDOW_S:
            self._errors.popleft()
        if self.available and len(self._errors) >= ERRORS_UNAVAILABLE:
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

    def _revision(self, lease: _Lease, stream: StreamId) -> str | None:
        if stream == StreamId.REFERENCE:
            return lease.wake.sha256
        return None if self.models is None else self.models.revision

    def _observe(self, lease: _Lease, stream: StreamId, kind: ObservationKind, through: int,
                 payload: Payload, utterance_id: str | None = None) -> Observation:
        return Observation(lease.device_id, lease.capture_epoch, stream, ObservationSource.CONTROLLER, kind,
                           through, utterance_id, self._revision(lease, stream), self.policy_hash, payload,
                           int(time.time() * 1000), lease.lease_id)

    def _lease(self, lease_id: str) -> _Lease:
        lease = self._leases.get(lease_id)
        if lease is None:
            raise SpeechWorkerError(f"unknown lease {lease_id}")
        return lease

    def _require_started(self) -> SpeechModels:
        if self._executor is None or self.models is None:
            raise SpeechWorkerError("worker is not started")
        return self.models
