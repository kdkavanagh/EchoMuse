"""Transcript correction service (SPEC §16.8): revisioned tables and the HA calls.

The tables, their pronunciations and every computation on them live in the child process
em_text_fix_worker runs; this side holds only the published revision (`TablesInfo`) and
asks the child for each step. Nothing CPU-heavy runs in the controller process.

A vocabulary refresh that changed nothing the tables read leaves them published. A
vocabulary that differs, a trigger change, an HA disconnect or the child's exit withdraws
them at once and bumps the revision; a debounced rebuild publishes only if no invalidation
came in the meantime, so an in-flight build from an old connection or an old registry
never lands. Reconnecting is not a refresh barrier: until the rebuild publishes,
corrections report `not_ready`.

Pronunciations (em_text_fix.Phonetic) are compiled in the child per lexicon whenever
tables publish, for every lexicon in use, and on first need for any other; a part whose
source did not change is reused.

`correct()` computes one correction under a `FixContext`: HA's matcher on the original,
candidates in the child, HA's matcher on the candidates, then acceptance in the child.
Whether the result may still be used is the caller's to check with `valid()` at the moment
of use.
"""

from __future__ import annotations

import asyncio
import logging
from collections import Counter
from collections.abc import Awaitable, Callable, Iterable, Sequence
from dataclasses import dataclass
from typing import TypeVar

from em_ha_client import HaClient, Vocabulary
from em_phonetics import Lexicon, Pronounced
from em_text_fix import (
    Decision,
    FixConfig,
    FixContext,
    FixOutcome,
    FixRecord,
    HistoryEntry,
    Reason,
    Request,
    TextFixMode,
)
from em_text_fix_worker import (
    AddHistory,
    Build,
    ChildWorker,
    Compute,
    Decide,
    Job,
    Pronounce,
    Propose,
    Stale,
    TablesInfo,
    Unknown,
    Warm,
    WorkerLost,
    history_rows,
)

log = logging.getLogger("em_text_fix")

REBUILD_DEBOUNCE_S = 1.5        # after an invalidation, before the tables are rebuilt
RECOGNIZE_TIMEOUT_S = 5.0       # each matcher call of a correction
HISTORY_LIMIT = 5_000           # newest eligible turns loaded into the history

R = TypeVar("R")


class NotReady(Exception):
    """No current tables, HA's matcher unavailable, or the context no longer holds."""


@dataclass(frozen=True)
class Trial:
    """A transcript tried as a standalone command on one speaker (the dashboard's tester):
    what `on` would send. `route`: the EchoMuse route that takes it before HA
    (em_session.LocalRoute), None for HA; `record`: None when a route other than a timer
    cancel takes it, since correction never sees it."""
    text: str
    mode: TextFixMode             # the speaker's actual `transcriptFix`
    route: str | None
    satellite: str | None         # the HA device whose context the matcher used
    pipeline: str
    language: str | None
    wake_phrase: str | None       # tried as a wake turn: the lead-in rule's phrase
    heard: Pronounced             # the text's words and their pronunciations
    proposed: Pronounced | None   # the proposed correction's
    record: FixRecord | None


class TextFixService:
    def __init__(self, ha: HaClient, load_history: Callable[[], Awaitable[Sequence[HistoryEntry]]],
                 lexicons: Callable[[], Iterable[Lexicon]] = lambda: (),
                 worker: Callable[[Callable[[], None]], Compute] = ChildWorker) -> None:
        """`lexicons`: those of the speakers' current settings, compiled ahead of their turns.
        `worker`: makes where the jobs run, given what to call when it loses its state."""
        self._ha = ha
        self._load_history = load_history
        self._lexicons = lexicons
        self._worker = worker(self._on_worker_lost)
        self.revision = 0
        self.tables: TablesInfo | None = None
        self._vocabulary: Vocabulary | None = None
        self._vocab_pending = True        # a vocabulary refresh is due before tables can be built
        self._unsent: list[HistoryEntry] = []    # history not yet in the child's tables
        self._rebuild: asyncio.Task[None] | None = None
        self._history_build: asyncio.Task[None] | None = None
        self._warming: set[asyncio.Task[int]] = set()

    # ── lifecycle ──

    def start(self) -> None:
        """Start the child; hear HA's disconnects and trigger changes. The vocabulary arrives
        through `on_vocabulary`, wired to HaClient.track_vocabulary."""
        self._worker.start()
        self._ha.add_disconnect_listener(self._on_disconnect)
        self._ha.track_triggers(lambda: self.invalidate("sentence triggers changed"))

    async def close(self) -> None:
        for task in (self._rebuild, self._history_build, *self._warming):
            if task is not None:
                task.cancel()
        await self._worker.close()

    def invalidate(self, why: str) -> None:
        """Not ready from now until a rebuild publishes; results computed before are stale."""
        self.revision += 1
        if self.tables is not None:
            log.info("transcript fix tables invalidated (%s)", why)
        self.tables = None
        self._schedule()

    def on_vocabulary(self, vocabulary: Vocabulary) -> None:
        """A refreshed vocabulary. One equal to the tables' own changes nothing: they stay
        published (or their rebuild goes on), and nothing is rebuilt. After a build that
        failed, any refresh tries again."""
        building = self._rebuild is not None and not self._rebuild.done()
        if not self._vocab_pending and vocabulary == self._vocabulary and (self.tables is not None or building):
            return
        self._vocabulary = vocabulary
        self._vocab_pending = False
        self.invalidate("vocabulary changed")

    def _on_disconnect(self) -> None:
        self._vocab_pending = True        # every connect refreshes the vocabulary
        self.invalidate("HA disconnected")

    def _on_worker_lost(self) -> None:
        self.invalidate("transcript fix worker exited")

    def _schedule(self) -> None:
        if self._rebuild is not None and not self._rebuild.done():
            self._rebuild.cancel()
        self._rebuild = asyncio.get_running_loop().create_task(self._build(), name="text-fix-tables")

    async def _ask(self, job: Job[R]) -> R:
        """`job` run in the child; NotReady when it holds no such tables or exits first."""
        try:
            return await self._worker.ask(job)
        except (Stale, WorkerLost) as err:
            raise NotReady(str(err)) from err

    async def _build(self) -> None:
        await asyncio.sleep(REBUILD_DEBOUNCE_S)
        if self._vocab_pending or self._vocabulary is None or not self._ha.connected:
            return
        revision, generation, vocabulary = self.revision, self._ha.generation, self._vocabulary
        try:
            templates = await self._ha.trigger_sentences()
            history = tuple(await self._load_history())
            tables = await self._ask(Build(revision, vocabulary, tuple(templates), history_rows(history)))
        except Exception as err:
            log.warning("transcript fix tables not built: %s", err)
            return
        if revision != self.revision or generation != self._ha.generation or not self._ha.connected:
            return                         # invalidated while building: the newer build publishes
        loaded = max((e.turn_id for e in history), default=0)
        later = [e for e in self._unsent if e.turn_id > loaded]   # sent while the build ran
        self._unsent = []
        self.tables = tables
        log.info("transcript fix tables %s in %.1f s", tables.summary, tables.seconds)
        self._warm()
        for entry in later:
            self.add_history(entry)

    def add_history(self, entry: HistoryEntry) -> None:
        """A request HA's own agent answered without error, as sent (§16.8 history). The
        child rebuilds the history layer and it is published under a new revision, unless
        the tables were invalidated meanwhile (their rebuild reloads the history)."""
        self._unsent.append(entry)
        if self.tables is not None and (self._history_build is None or self._history_build.done()):
            self._history_build = asyncio.get_running_loop().create_task(self._publish_history())

    async def _publish_history(self) -> None:
        while (tables := self.tables) is not None and self._unsent:
            entries, self._unsent = tuple(self._unsent), []
            self.revision += 1
            try:
                published = await self._ask(AddHistory(tables.revision, self.revision, history_rows(entries)))
            except NotReady:
                return                     # the tables moved on: their rebuild reloads the history
            if self.tables is not tables:
                return
            self.tables = published
            self._warm()

    # ── pronunciations ──

    def _warm(self) -> None:
        """Compile the published tables for every lexicon in use or held, before a turn needs it."""
        tables = self.tables
        if tables is None:
            return
        try:
            lexicons = tuple(self._lexicons())
        except Exception:
            log.exception("transcript fix: lexicons in use")
            lexicons = ()
        task = asyncio.get_running_loop().create_task(self._ask(Warm(tables.revision, lexicons)))
        self._warming.add(task)
        task.add_done_callback(self._warmed)

    def _warmed(self, task: asyncio.Task[int]) -> None:
        self._warming.discard(task)
        if not task.cancelled() and (err := task.exception()) is not None and not isinstance(err, NotReady):
            log.error("transcript fix pronunciations not compiled: %s", err)

    async def unknown_words(self, lexicon: Lexicon) -> Counter[str]:
        """Words of HA's trigger sentences, earlier requests and names with no pronunciation
        under `lexicon`, by how many of those texts use them. NotReady without tables."""
        tables = self.current()
        if tables is None:
            raise NotReady("tables not ready")
        return await self._ask(Unknown(tables.revision, lexicon))

    async def pronounce(self, lexicon: Lexicon, texts: Sequence[str]) -> tuple[Pronounced, ...]:
        """Each text's words and their pronunciations under `lexicon`. NotReady without the child."""
        return await self._ask(Pronounce(lexicon, tuple(texts)))

    # ── corrections ──

    def current(self) -> TablesInfo | None:
        return self.tables if self._ha.connected else None

    def valid(self, context: FixContext) -> bool:
        """Whether a result computed under `context` may be used now (its HA connection and
        table revision both still current)."""
        tables = self.current()
        return (tables is not None and tables.revision == context.tables
                and self._ha.generation == context.connection)

    def context(self, config: FixConfig, device: str | None, pipeline: str, language: str | None,
                wake_phrase: str | None) -> FixContext:
        tables = self.current()
        if tables is None:
            raise NotReady("tables not ready")
        return FixContext(self._ha.generation, tables.revision, device, pipeline, language, config.digest,
                          wake_phrase)

    async def correct(self, key: str, context: FixContext, config: FixConfig) -> FixOutcome:
        """The correction of canonical text `key` under `context`. NotReady when the tables
        moved on before it began; HaError/HaUnavailable from the matcher."""
        if not self.valid(context):
            raise NotReady("context no longer current")
        loop = asyncio.get_running_loop()
        started = loop.time()
        [original] = await self._ha.recognize([key], context.device, language=context.language,
                                              timeout=RECOGNIZE_TIMEOUT_S)
        decision: Decision
        if original is not None and original.match:
            decision = Decision(Reason.ORIGINAL_MATCHED)
        else:
            proposal = await self._ask(Propose(context.tables, Request(key, context.wake_phrase), original,
                                               config.terms, config.phrases, config.lexicon))
            verdicts = (await self._ha.recognize([c.text for c in proposal.validate], context.device,
                                                 language=context.language, timeout=RECOGNIZE_TIMEOUT_S)
                        if proposal.validate else [])
            decision = await self._ask(Decide(context.tables, proposal, tuple(verdicts), original))
        return FixOutcome(key, context, decision, original, round((loop.time() - started) * 1000))
