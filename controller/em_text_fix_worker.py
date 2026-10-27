"""Transcript correction does its CPU work in one child process (SPEC §16.8).

The controller process also runs the speech worker, whose VAD and ASR jobs have a 750 ms
deadline. Building the tables there held the GIL for up to 0.5 s at a time and compiling
pronunciations for up to 0.7 s (long C calls, not GC), enough to miss that deadline and
take every speaker's wakes down; even unpickling built tables costs 0.2 s of GIL. So
everything CPU-heavy in transcript correction runs here instead: the tables and their
history layer, compiled pronunciations, candidates (`propose`, the user's lists),
acceptance (`decide`), unknown words and the tester's pronunciations. The child owns
`Tables` and `Phonetic` by revision; the controller sends small `Job`s and receives small
results, and never holds the tables. HA's matcher is asked by the controller.

Wire: frames of a 4-byte big-endian length and a pickle, `(id, job)` over the child's
stdin and `(id, Reply)` over a private copy of its stdout; the child points its own fd 1
at stderr, so nothing it prints can corrupt a frame. Jobs run on two lanes of one thread
each: BUILD (tables, history, pronunciations: up to a second) and SERVE (a correction's
candidates and acceptance, pronouncing a trial), so a correction is not queued behind a
build. A correction whose lexicon is not yet compiled for its tables goes to BUILD, where
the compile it needs is already queued or done.

`ChildWorker` is the controller's side. It starts the child as a fresh interpreter
(`asyncio.create_subprocess_exec`: fork and exec at once, never a fork of the threaded
controller that keeps running Python), which imports only transcript correction (no
aiohttp, onnxruntime or sherpa-onnx). A child that exits is logged and restarted, and
every job it held fails with `WorkerLost`; its owner treats that as an invalidation. The
child exits when its stdin closes.
"""

from __future__ import annotations

import asyncio
import itertools
import logging
import os
import pickle
import struct
import sys
import threading
import time
from collections import Counter
from collections.abc import Callable, Iterable
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, replace
from enum import StrEnum
from typing import BinaryIO, Final, Generic, Protocol, TypeVar

from em_ha_client import SentenceRecognition, Vocabulary
from em_phonetics import Lexicon, Pronounced, load_dictionary
from em_text_fix import (
    Decision,
    HistoryEntry,
    Phonetic,
    Proposal,
    Request,
    Tables,
    build_history,
    build_tables,
    compile_phonetic,
    decide,
    propose,
    user_vocabulary,
)

log = logging.getLogger("em_text_fix")

MAX_LEXICONS: Final = 4          # compiled pronunciations kept, the most recently used
RECENT_TABLES: Final = 3         # revisions kept, for a correction begun before the newest one
RESTART_MIN_S: Final = 1.0       # a child that dies waits this long before the next one starts,
RESTART_MAX_S: Final = 30.0      # doubling while each dies within STABLE_S of starting
STABLE_S: Final = 60.0
CLOSE_TIMEOUT_S: Final = 2.0     # after stdin closes, before the child is killed

_HEADER: Final = struct.Struct(">I")
_CHILD: Final = ("import sys; sys.path.insert(0, {path!r}); "
                 "import em_text_fix_worker; em_text_fix_worker.serve()")

R = TypeVar("R")
T = TypeVar("T")


class Lane(StrEnum):
    BUILD = "build"     # tables, history layers, pronunciations, unknown words
    SERVE = "serve"     # a correction's candidates and acceptance, a trial's pronunciations


class Stale(Exception):
    """The child holds no tables at the requested revision (or they are no longer its newest)."""


class WorkerLost(Exception):
    """The child exited, or is not running, before it answered."""


class WorkerError(Exception):
    """A job raised in the child; the message is its exception's."""


# A HistoryEntry on the wire: (text, turn_id, ts). Plain tuples pickle 12× faster than the
# dataclass (5,000 entries: 1 ms against 12 ms of the controller's GIL).
HistoryRow = tuple[str, int, float]


def history_rows(entries: Iterable[HistoryEntry]) -> tuple[HistoryRow, ...]:
    return tuple((e.text, e.turn_id, e.ts) for e in entries)


def _entries(rows: Iterable[HistoryRow]) -> list[HistoryEntry]:
    return [HistoryEntry(text, turn_id, ts) for text, turn_id, ts in rows]


def _expect(value: object, kind: type[T]) -> T:
    if not isinstance(value, kind):
        raise WorkerError(f"expected {kind.__name__}, got {type(value).__name__}")
    return value


@dataclass(frozen=True)
class TablesInfo:
    """What the controller holds of tables the child built: their revision and size."""
    revision: int
    summary: dict[str, int]
    seconds: float


# ── The child's state ────────────────────────────────────────────────────────

class Engine:
    """The child's tables (the RECENT_TABLES newest revisions), the history they were built
    from, and pronunciations compiled per lexicon. Used from both lanes."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._tables: dict[int, Tables] = {}               # by revision, oldest first
        self._history: list[HistoryEntry] = []
        self._phonetic: dict[str, Phonetic] = {}           # by lexicon digest, least recently used first
        self._compiling: dict[str, threading.Lock] = {}    # one compile per lexicon at a time

    def _newest(self) -> Tables | None:
        return next(reversed(self._tables.values()), None)

    def _keep(self, tables: Tables) -> None:
        self._tables[tables.revision] = tables
        while len(self._tables) > RECENT_TABLES:
            self._tables.pop(next(iter(self._tables)))

    def tables(self, revision: int) -> Tables:
        with self._lock:
            tables = self._tables.get(revision)
        if tables is None:
            raise Stale(f"no tables at revision {revision}")
        return tables

    def build(self, job: Build) -> TablesInfo:
        started = time.monotonic()
        history = _entries(job.history)
        tables = build_tables(job.vocabulary, job.templates, history, job.revision)
        with self._lock:
            self._history = history
            self._keep(tables)
        return TablesInfo(tables.revision, tables.summary, time.monotonic() - started)

    def add_history(self, job: AddHistory) -> TablesInfo:
        started = time.monotonic()
        with self._lock:
            base = self._newest()
            history = self._history + _entries(job.entries)
        if base is None or base.revision != job.base:
            raise Stale(f"tables moved on from revision {job.base}")
        tables = replace(base, history=build_history(history), revision=job.revision)
        with self._lock:
            if self._newest() is not base:
                raise Stale(f"tables moved on from revision {job.base}")
            self._history = history
            self._keep(tables)
        return TablesInfo(tables.revision, tables.summary, time.monotonic() - started)

    def compiled(self, revision: int, lexicon: Lexicon) -> bool:
        """Whether `lexicon`'s pronunciations of that revision's tables are at hand."""
        with self._lock:
            tables = self._tables.get(revision)
            held = self._phonetic.get(lexicon.digest)
        return tables is not None and held is not None and held.compiled_from(tables, lexicon)

    def phonetic(self, tables: Tables, lexicon: Lexicon) -> Phonetic:
        """`tables` pronounced under `lexicon`, compiled when not yet done for them, reusing
        the parts that did not change. Kept unless it would replace one for the newest tables."""
        digest = lexicon.digest
        with self._lock:
            held = self._phonetic.get(digest)
            if held is not None and held.compiled_from(tables, lexicon):
                self._phonetic[digest] = self._phonetic.pop(digest)      # most recently used
                return held
            gate = self._compiling.setdefault(digest, threading.Lock())
        with gate:
            with self._lock:
                held = self._phonetic.get(digest)          # the other lane may have compiled it meanwhile
            if held is not None and held.compiled_from(tables, lexicon):
                return held
            phonetic = compile_phonetic(tables, lexicon, held)
            with self._lock:
                now, newest = self._phonetic.get(digest), self._newest()
                if now is None or newest is None or not now.compiled_from(newest, lexicon):
                    self._phonetic.pop(digest, None)
                    self._phonetic[digest] = phonetic
                    while len(self._phonetic) > MAX_LEXICONS:
                        self._phonetic.pop(next(iter(self._phonetic)))
            return phonetic

    def warm(self, job: Warm) -> int:
        """Compile the newest tables for `job.lexicons` and every lexicon held; how many compiled."""
        with self._lock:
            newest = self._newest()
            lexicons = {p.lexicon.digest: p.lexicon for p in self._phonetic.values()}
        if newest is None or newest.revision != job.revision:
            raise Stale(f"tables moved on from revision {job.revision}")
        lexicons.update((x.digest, x) for x in job.lexicons)
        compiled = 0
        for lexicon in lexicons.values():
            if not self.compiled(job.revision, lexicon):
                self.phonetic(newest, lexicon)
                compiled += 1
        return compiled


# ── Jobs ─────────────────────────────────────────────────────────────────────

class Job(Generic[R]):
    """One unit of work: `run` executes it in the child, `accept` checks its result in the
    controller, `lane` picks the child's thread."""

    def lane(self, engine: Engine) -> Lane:
        return Lane.BUILD

    def run(self, engine: Engine) -> R:
        raise NotImplementedError

    def accept(self, value: object) -> R:
        raise NotImplementedError


@dataclass(frozen=True)
class Build(Job[TablesInfo]):
    """The tables of one revision; the child keeps them and the history they hold."""
    revision: int
    vocabulary: Vocabulary
    templates: tuple[str, ...]
    history: tuple[HistoryRow, ...]

    def run(self, engine: Engine) -> TablesInfo:
        return engine.build(self)

    def accept(self, value: object) -> TablesInfo:
        return _expect(value, TablesInfo)


@dataclass(frozen=True)
class AddHistory(Job[TablesInfo]):
    """The newest tables (at `base`) with `entries` joining their history, as `revision`."""
    base: int
    revision: int
    entries: tuple[HistoryRow, ...]

    def run(self, engine: Engine) -> TablesInfo:
        return engine.add_history(self)

    def accept(self, value: object) -> TablesInfo:
        return _expect(value, TablesInfo)


@dataclass(frozen=True)
class Warm(Job[int]):
    """Compile the newest tables (at `revision`) for these lexicons and every one held."""
    revision: int
    lexicons: tuple[Lexicon, ...]

    def run(self, engine: Engine) -> int:
        return engine.warm(self)

    def accept(self, value: object) -> int:
        return _expect(value, int)


@dataclass(frozen=True)
class Propose(Job[Proposal]):
    """A correction's candidates (em_text_fix.propose) under the user's lists."""
    revision: int
    request: Request
    original: SentenceRecognition | None
    terms: tuple[str, ...]
    phrases: tuple[str, ...]
    lexicon: Lexicon

    def lane(self, engine: Engine) -> Lane:
        return Lane.SERVE if engine.compiled(self.revision, self.lexicon) else Lane.BUILD

    def run(self, engine: Engine) -> Proposal:
        tables = engine.tables(self.revision)
        return propose(self.request, self.original, tables, engine.phonetic(tables, self.lexicon),
                       user_vocabulary(self.terms, self.phrases, self.lexicon))

    def accept(self, value: object) -> Proposal:
        return _expect(value, Proposal)


@dataclass(frozen=True)
class Decide(Job[Decision]):
    """Acceptance (em_text_fix.decide) of HA's verdicts on a proposal's candidates."""
    revision: int
    proposal: Proposal
    verdicts: tuple[SentenceRecognition | None, ...]
    original: SentenceRecognition | None

    def lane(self, engine: Engine) -> Lane:
        return Lane.SERVE

    def run(self, engine: Engine) -> Decision:
        return decide(self.proposal, self.verdicts, self.original, engine.tables(self.revision))

    def accept(self, value: object) -> Decision:
        return _expect(value, Decision)


@dataclass(frozen=True)
class Unknown(Job[Counter[str]]):
    """Words of the tables' texts with no pronunciation under `lexicon`, by how many use them."""
    revision: int
    lexicon: Lexicon

    def lane(self, engine: Engine) -> Lane:
        return Lane.SERVE if engine.compiled(self.revision, self.lexicon) else Lane.BUILD

    def run(self, engine: Engine) -> Counter[str]:
        return engine.phonetic(engine.tables(self.revision), self.lexicon).unknown

    def accept(self, value: object) -> Counter[str]:
        found: Counter[str] = Counter()
        for word, count in _expect(value, Counter).items():
            found[_expect(word, str)] = _expect(count, int)
        return found


@dataclass(frozen=True)
class Pronounce(Job[tuple[Pronounced, ...]]):
    """Each text's words and their pronunciations under `lexicon`."""
    lexicon: Lexicon
    texts: tuple[str, ...]

    def lane(self, engine: Engine) -> Lane:
        return Lane.SERVE

    def run(self, engine: Engine) -> tuple[Pronounced, ...]:
        return tuple(self.lexicon.pronounce(text) for text in self.texts)

    def accept(self, value: object) -> tuple[Pronounced, ...]:
        return tuple(_expect(p, Pronounced) for p in _expect(value, tuple))


@dataclass(frozen=True)
class Reply:
    """A job's result, or why it has none: `stale` (Stale) or `error` (anything else)."""
    value: object = None
    error: str | None = None
    stale: bool = False

    def unwrap(self, job: Job[R]) -> R:
        if self.stale:
            raise Stale(self.error or "stale")
        if self.error is not None:
            raise WorkerError(self.error)
        return job.accept(self.value)


# ── The child ────────────────────────────────────────────────────────────────

def _frame(message: object) -> bytes:
    body = pickle.dumps(message, protocol=pickle.HIGHEST_PROTOCOL)
    return _HEADER.pack(len(body)) + body


def _read_frame(source: BinaryIO) -> bytes | None:
    header = source.read(_HEADER.size)
    if len(header) < _HEADER.size:
        return None
    (size,) = _HEADER.unpack(header)
    body = source.read(size)
    return body if len(body) == size else None


def _run(engine: Engine, job: Job[object]) -> Reply:
    try:
        return Reply(job.run(engine))
    except Stale as err:
        return Reply(error=str(err), stale=True)
    except Exception as err:
        log.exception("transcript fix job %s failed", type(job).__name__)
        return Reply(error=f"{type(err).__name__}: {err}")


def serve() -> None:
    """The child's main loop: read jobs until stdin closes, run each on its lane, reply."""
    logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(name)s — %(message)s")
    replies = os.fdopen(os.dup(1), "wb")
    os.dup2(2, 1)                                # stray prints go to stderr, never into a frame
    sys.stdout = sys.stderr
    jobs = os.fdopen(0, "rb")
    engine = Engine()
    lanes = {lane: ThreadPoolExecutor(max_workers=1, thread_name_prefix=f"text-fix-{lane}") for lane in Lane}
    writing = threading.Lock()

    def answer(request_id: int, job: Job[object]) -> None:
        reply = _run(engine, job)
        try:
            frame = _frame((request_id, reply))
        except Exception as err:                 # an unpicklable result
            frame = _frame((request_id, Reply(error=f"reply not sent: {err}")))
        try:
            with writing:
                replies.write(frame)
                replies.flush()
        except OSError:
            os._exit(0)                          # the controller is gone

    lanes[Lane.BUILD].submit(load_dictionary)    # CMUdict (0.1–0.4 s) before the first job needs it
    while (frame := _read_frame(jobs)) is not None:
        request_id, job = pickle.loads(frame)
        if isinstance(request_id, int) and isinstance(job, Job):
            lanes[job.lane(engine)].submit(answer, request_id, job)
        else:
            log.error("transcript fix worker: not a job: %r", type(job).__name__)
    os._exit(0)                                  # without waiting for a build nobody will read


# ── The controller's side ────────────────────────────────────────────────────

class Compute(Protocol):
    """Where transcript correction's jobs run: the child (ChildWorker), or a test's stand-in."""

    def start(self) -> None: ...

    async def close(self) -> None: ...

    async def ask(self, job: Job[R]) -> R: ...


class ChildWorker:
    """Starts the child, sends it jobs and restarts it when it exits. `on_lost` is called each
    time a running child exits (not at close): everything it held is gone."""

    def __init__(self, on_lost: Callable[[], None]) -> None:
        self._on_lost = on_lost
        self._proc: asyncio.subprocess.Process | None = None
        self._up = asyncio.Event()
        self._pending: dict[int, asyncio.Future[Reply]] = {}
        self._ids = itertools.count(1)
        self._supervisor: asyncio.Task[None] | None = None
        self._closing = False

    @property
    def pid(self) -> int | None:
        return self._proc.pid if self._proc is not None else None

    def start(self) -> None:
        self._supervisor = asyncio.get_running_loop().create_task(self._supervise(), name="text-fix-worker")

    async def close(self) -> None:
        self._closing = True
        if self._supervisor is not None:
            self._supervisor.cancel()
            await asyncio.gather(self._supervisor, return_exceptions=True)
        proc, self._proc = self._proc, None
        self._up.clear()
        if proc is not None and proc.returncode is None:
            if proc.stdin is not None:
                proc.stdin.close()
            try:
                await asyncio.wait_for(proc.wait(), CLOSE_TIMEOUT_S)
            except TimeoutError:
                proc.kill()
                await proc.wait()
        self._fail_pending("transcript fix worker closed")

    async def ask(self, job: Job[R]) -> R:
        """`job`'s result. WorkerLost when the child exits first, Stale or WorkerError as it
        replied. Waits for a child to be running; the caller bounds the wait."""
        if self._closing:
            raise WorkerLost("transcript fix worker closed")
        await self._up.wait()
        proc = self._proc
        if proc is None or proc.stdin is None:
            raise WorkerLost("transcript fix worker not running")
        request_id = next(self._ids)
        future: asyncio.Future[Reply] = asyncio.get_running_loop().create_future()
        self._pending[request_id] = future
        try:
            proc.stdin.write(_frame((request_id, job)))
            await proc.stdin.drain()
            reply = await future
        except (ConnectionError, OSError) as err:
            raise WorkerLost(f"transcript fix worker: {err}") from err
        finally:
            self._pending.pop(request_id, None)
        return reply.unwrap(job)

    def _fail_pending(self, why: str) -> None:
        pending, self._pending = self._pending, {}
        for future in pending.values():
            if not future.done():
                future.set_exception(WorkerLost(why))

    async def _supervise(self) -> None:
        loop = asyncio.get_running_loop()
        delay = RESTART_MIN_S
        while True:
            started = loop.time()
            try:
                proc = await asyncio.create_subprocess_exec(
                    sys.executable, "-c", _CHILD.format(path=os.path.dirname(os.path.abspath(__file__))),
                    stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE)
            except OSError as err:
                log.error("transcript fix worker did not start: %s; retry in %.0f s", err, delay)
                await asyncio.sleep(delay)
                delay = min(delay * 2, RESTART_MAX_S)
                continue
            self._proc = proc
            self._up.set()
            log.info("transcript fix worker started (pid %d)", proc.pid)
            await self._read(proc)
            self._up.clear()
            self._proc = None
            code = await proc.wait()
            self._fail_pending(f"transcript fix worker exited ({code})")
            log.warning("transcript fix worker (pid %d) exited with %s; restarting", proc.pid, code)
            self._on_lost()
            if loop.time() - started >= STABLE_S:
                delay = RESTART_MIN_S
            await asyncio.sleep(delay)
            delay = min(delay * 2, RESTART_MAX_S)

    async def _read(self, proc: asyncio.subprocess.Process) -> None:
        """Deliver replies until the child's stdout ends."""
        out = proc.stdout
        if out is None:
            return
        while True:
            try:
                header = await out.readexactly(_HEADER.size)
                body = await out.readexactly(_HEADER.unpack(header)[0])
            except (asyncio.IncompleteReadError, ConnectionError):
                return
            request_id, reply = pickle.loads(body)
            future = self._pending.get(request_id)
            if future is not None and not future.done() and isinstance(reply, Reply):
                future.set_result(reply)
