"""Transcript correction service (SPEC §16.8): revisioned tables and the HA calls.

Holds the one published `em_text_fix.Tables` snapshot. Every relevant change (registry
event, trigger change, HA disconnect) invalidates it at once and bumps the revision; a
debounced rebuild runs off the event loop and publishes only if no invalidation came
in the meantime, so an in-flight build from an old connection or an old registry never
lands. Reconnecting is not a refresh barrier: until the rebuild publishes, corrections
report `not_ready`.

`correct()` computes one correction under a `FixContext`: HA's matcher on the original,
candidates off the event loop, HA's matcher on the candidates, then acceptance. Whether
the result may still be used is the caller's to check with `valid()` at the moment of use.
"""

from __future__ import annotations

import asyncio
import logging
import time
from collections.abc import Awaitable, Callable, Sequence
from concurrent.futures import ThreadPoolExecutor
from dataclasses import replace

from em_ha_client import HaClient, Vocabulary
from em_text_fix import (
    Decision,
    FixConfig,
    FixContext,
    FixOutcome,
    HistoryEntry,
    Reason,
    Request,
    Tables,
    build_history,
    build_tables,
    decide,
    propose,
    user_vocabulary,
)
from em_text_fix_search import PhraseSearch, SearchResult

log = logging.getLogger("em_text_fix")

REBUILD_DEBOUNCE_S = 1.5        # after an invalidation, before the tables are rebuilt
RECOGNIZE_TIMEOUT_S = 5.0       # each matcher call of a correction
HISTORY_LIMIT = 5_000           # newest eligible turns loaded into the history


class NotReady(Exception):
    """No current tables, HA's matcher unavailable, or the context no longer holds."""


class TextFixService:
    def __init__(self, ha: HaClient, load_history: Callable[[], Awaitable[Sequence[HistoryEntry]]]) -> None:
        self._ha = ha
        self._load_history = load_history
        self.revision = 0
        self.tables: Tables | None = None
        self._vocabulary: Vocabulary | None = None
        self._vocab_pending = True        # a vocabulary refresh is due before tables can be built
        self._history: list[HistoryEntry] = []
        self._rebuild: asyncio.Task[None] | None = None
        self._history_build: asyncio.Task[None] | None = None
        self._cpu = ThreadPoolExecutor(max_workers=1, thread_name_prefix="text-fix")
        self._builder = ThreadPoolExecutor(max_workers=1, thread_name_prefix="text-fix-build")
        self._search = PhraseSearch()

    async def search(self, query: str, config: FixConfig, limit: int) -> SearchResult:
        """The dashboard's phrase search over `config`'s saved lists and the published tables.
        Runs off the event loop, never on the executor corrections use."""
        return await asyncio.to_thread(self._search.search, self.current(), query, config.terms,
                                       config.phrases, limit)

    # ── lifecycle ──

    def start(self) -> None:
        """Hear HA's disconnects and trigger changes. The vocabulary arrives through
        `on_vocabulary`/`on_registry_stale`, wired to HaClient.track_vocabulary."""
        self._ha.add_disconnect_listener(self._on_disconnect)
        self._ha.track_triggers(lambda: self.invalidate("sentence triggers changed"))

    def close(self) -> None:
        for task in (self._rebuild, self._history_build):
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
            if len(self._history) == len(entries):
                return

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
        [original] = await self._ha.recognize([key], context.device, language=context.language,
                                              timeout=RECOGNIZE_TIMEOUT_S)
        decision: Decision
        if original is not None and original.match:
            decision = Decision(Reason.ORIGINAL_MATCHED)
        else:
            user = user_vocabulary(config.terms, config.phrases)
            proposal = await loop.run_in_executor(self._cpu, propose, Request(key, context.wake_phrase),
                                                  original, tables, user)
            verdicts = (await self._ha.recognize([c.text for c in proposal.validate], context.device,
                                                 language=context.language, timeout=RECOGNIZE_TIMEOUT_S)
                        if proposal.validate else [])
            decision = decide(proposal, verdicts, original, tables)
        return FixOutcome(key, context, decision, original, round((loop.time() - started) * 1000))
