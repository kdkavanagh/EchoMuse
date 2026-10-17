"""Transcript correction service (SPEC §16.8): revisioned tables and the HA calls.

Holds the one published `em_text_fix.Tables` snapshot. Every relevant change (registry
event, trigger change, HA disconnect) invalidates it at once and bumps the revision; a
debounced rebuild runs off the event loop and publishes only if no invalidation came
in the meantime, so an in-flight build from an old connection or an old registry never
lands. Reconnecting is not a refresh barrier: until the rebuild publishes, corrections
report `not_ready`.

Pronunciations (em_text_fix.Phonetic) are compiled per lexicon, off the loop, the first time
one is needed for the published tables, and again, for every lexicon in use, when the
tables move on; a part whose source did not change is reused.

`correct()` computes one correction under a `FixContext`: HA's matcher on the original,
candidates off the event loop, HA's matcher on the candidates, then acceptance. Whether
the result may still be used is the caller's to check with `valid()` at the moment of use.
"""

from __future__ import annotations

import asyncio
import logging
import time
from collections import Counter
from collections.abc import Awaitable, Callable, Iterable, Sequence
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, replace

from em_ha_client import HaClient, SentenceRecognition, Vocabulary
from em_phonetics import Lexicon, Pronounced
from em_text_fix import (
    Decision,
    FixConfig,
    FixContext,
    FixOutcome,
    FixRecord,
    HistoryEntry,
    Phonetic,
    Proposal,
    Reason,
    Request,
    Tables,
    TextFixMode,
    build_history,
    build_tables,
    compile_phonetic,
    decide,
    propose,
    user_vocabulary,
)

log = logging.getLogger("em_text_fix")

REBUILD_DEBOUNCE_S = 1.5        # after an invalidation, before the tables are rebuilt
RECOGNIZE_TIMEOUT_S = 5.0       # each matcher call of a correction
HISTORY_LIMIT = 5_000           # newest eligible turns loaded into the history
MAX_LEXICONS = 4                # compiled pronunciations kept, the most recently used


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


def _propose(request: Request, original: SentenceRecognition | None, tables: Tables, phonetic: Phonetic,
             config: FixConfig) -> Proposal:
    """Candidates, in the CPU executor: the user's lists are pronounced here on first use."""
    return propose(request, original, tables, phonetic,
                   user_vocabulary(config.terms, config.phrases, config.lexicon))


class TextFixService:
    def __init__(self, ha: HaClient, load_history: Callable[[], Awaitable[Sequence[HistoryEntry]]],
                 lexicons: Callable[[], Iterable[Lexicon]] = lambda: ()) -> None:
        """`lexicons`: those of the speakers' current settings, compiled ahead of their turns."""
        self._ha = ha
        self._load_history = load_history
        self._lexicons = lexicons
        self.revision = 0
        self.tables: Tables | None = None
        self._vocabulary: Vocabulary | None = None
        self._vocab_pending = True        # a vocabulary refresh is due before tables can be built
        self._history: list[HistoryEntry] = []
        self._rebuild: asyncio.Task[None] | None = None
        self._history_build: asyncio.Task[None] | None = None
        self._phonetic: dict[str, Phonetic] = {}             # by lexicon digest, least recently used first
        self._compiling: dict[tuple[str, int], asyncio.Future[Phonetic]] = {}
        self._warming: set[asyncio.Task[Phonetic]] = set()
        self._cpu = ThreadPoolExecutor(max_workers=1, thread_name_prefix="text-fix")
        self._builder = ThreadPoolExecutor(max_workers=1, thread_name_prefix="text-fix-build")

    # ── lifecycle ──

    def start(self) -> None:
        """Hear HA's disconnects and trigger changes. The vocabulary arrives through
        `on_vocabulary`/`on_registry_stale`, wired to HaClient.track_vocabulary."""
        self._ha.add_disconnect_listener(self._on_disconnect)
        self._ha.track_triggers(lambda: self.invalidate("sentence triggers changed"))

    def close(self) -> None:
        for task in (self._rebuild, self._history_build, *self._warming):
            if task is not None:
                task.cancel()
        self._cpu.shutdown(wait=False, cancel_futures=True)
        self._builder.shutdown(wait=False, cancel_futures=True)

    def invalidate(self, why: str) -> None:
        """Not ready from now until a rebuild publishes; results computed before are stale."""
        self.revision += 1
        if self.tables is not None:
            log.info("transcript fix tables invalidated (%s)", why)
        self.tables = None
        self._schedule()

    def on_registry_stale(self) -> None:
        self._vocab_pending = True
        self.invalidate("registry changed")

    def on_vocabulary(self, vocabulary: Vocabulary) -> None:
        self._vocabulary = vocabulary
        self._vocab_pending = False
        self.invalidate("vocabulary refreshed")

    def _on_disconnect(self) -> None:
        self._vocab_pending = True        # every connect refreshes the vocabulary
        self.invalidate("HA disconnected")

    def _schedule(self) -> None:
        if self._rebuild is not None and not self._rebuild.done():
            self._rebuild.cancel()
        self._rebuild = asyncio.get_running_loop().create_task(self._build(), name="text-fix-tables")

    async def _build(self) -> None:
        await asyncio.sleep(REBUILD_DEBOUNCE_S)
        if self._vocab_pending or self._vocabulary is None or not self._ha.connected:
            return
        revision, generation, vocabulary = self.revision, self._ha.generation, self._vocabulary
        started = time.monotonic()
        try:
            templates = await self._ha.trigger_sentences()
            history = list(await self._load_history())
        except Exception as err:
            log.warning("transcript fix tables not built: %s", err)
            return
        tables = await asyncio.get_running_loop().run_in_executor(
            self._builder, build_tables, vocabulary, templates, history, revision)
        if revision != self.revision or generation != self._ha.generation or not self._ha.connected:
            return                         # invalidated while building: the newer build publishes
        loaded = max((e.turn_id for e in history), default=0)
        later = [e for e in self._history if e.turn_id > loaded]   # sent while the build ran
        self._history = history
        self.tables = tables
        log.info("transcript fix tables %s in %.1f s", tables.summary, time.monotonic() - started)
        self._warm()
        for entry in later:
            self.add_history(entry)

    def add_history(self, entry: HistoryEntry) -> None:
        """A request HA's own agent answered without error, as sent (§16.8 history). The
        history layer is rebuilt off the loop and published under a new revision, unless
        the tables were invalidated meanwhile (their rebuild reloads the history)."""
        self._history.append(entry)
        if self.tables is not None and (self._history_build is None or self._history_build.done()):
            self._history_build = asyncio.get_running_loop().create_task(self._publish_history())

    async def _publish_history(self) -> None:
        tables = self.tables
        while tables is not None:
            entries = list(self._history)
            layer = await asyncio.get_running_loop().run_in_executor(self._builder, build_history, entries)
            if self.tables is not tables:
                return
            self.revision += 1
            self.tables = tables = replace(tables, history=layer, revision=self.revision)
            self._warm()
            if len(self._history) == len(entries):
                return

    # ── pronunciations ──

    async def phonetic(self, tables: Tables, lexicon: Lexicon) -> Phonetic:
        """`tables` pronounced under `lexicon`; compiled off the loop when not yet done for
        these tables, reusing the parts that did not change."""
        held = self._phonetic.get(lexicon.digest)
        if held is not None and held.compiled_from(tables, lexicon):
            self._phonetic[lexicon.digest] = self._phonetic.pop(lexicon.digest)     # most recently used
            return held
        key = (lexicon.digest, tables.revision)
        future = self._compiling.get(key)
        if future is None:
            future = asyncio.get_running_loop().run_in_executor(self._builder, compile_phonetic, tables, lexicon,
                                                                 held)
            self._compiling[key] = future
            future.add_done_callback(lambda _f: self._compiling.pop(key, None))
        phonetic = await asyncio.shield(future)
        now, published = self._phonetic.get(lexicon.digest), self.tables
        if now is None or published is None or not now.compiled_from(published, lexicon):
            self._phonetic.pop(lexicon.digest, None)          # never replace one for the published tables
            self._phonetic[lexicon.digest] = phonetic
            while len(self._phonetic) > MAX_LEXICONS:
                self._phonetic.pop(next(iter(self._phonetic)))
        return phonetic

    def _warm(self) -> None:
        """Compile the published tables for every lexicon in use or held, before a turn needs it."""
        tables = self.tables
        if tables is None:
            return
        lexicons = {p.lexicon.digest: p.lexicon for p in self._phonetic.values()}
        try:
            lexicons.update((x.digest, x) for x in self._lexicons())
        except Exception:
            log.exception("transcript fix: lexicons in use")
        for lexicon in lexicons.values():
            task = asyncio.get_running_loop().create_task(self.phonetic(tables, lexicon))
            self._warming.add(task)
            task.add_done_callback(self._warmed)

    def _warmed(self, task: asyncio.Task[Phonetic]) -> None:
        self._warming.discard(task)
        if not task.cancelled() and (err := task.exception()) is not None:
            log.error("transcript fix pronunciations not compiled: %s", err)

    async def unknown_words(self, lexicon: Lexicon) -> Counter[str]:
        """Words of HA's trigger sentences, earlier requests and names with no pronunciation
        under `lexicon`, by how many of those texts use them. NotReady without tables."""
        tables = self.current()
        if tables is None:
            raise NotReady("tables not ready")
        return (await self.phonetic(tables, lexicon)).unknown

    # ── corrections ──

    def current(self) -> Tables | None:
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
        tables = self.current()
        if tables is None or not self.valid(context):
            raise NotReady("context no longer current")
        loop = asyncio.get_running_loop()
        started = loop.time()
        phonetic = await self.phonetic(tables, config.lexicon)
        [original] = await self._ha.recognize([key], context.device, language=context.language,
                                              timeout=RECOGNIZE_TIMEOUT_S)
        decision: Decision
        if original is not None and original.match:
            decision = Decision(Reason.ORIGINAL_MATCHED)
        else:
            proposal = await loop.run_in_executor(self._cpu, _propose, Request(key, context.wake_phrase),
                                                  original, tables, phonetic, config)
            verdicts = (await self._ha.recognize([c.text for c in proposal.validate], context.device,
                                                 language=context.language, timeout=RECOGNIZE_TIMEOUT_S)
                        if proposal.validate else [])
            decision = decide(proposal, verdicts, original, tables)
        return FixOutcome(key, context, decision, original, round((loop.time() - started) * 1000))
