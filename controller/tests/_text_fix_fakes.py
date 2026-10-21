"""Transcript correction's jobs run in the test's own process: the em_text_fix_worker Engine
the child would run, without the child (tests/test_text_fix.py covers the child itself)."""

from __future__ import annotations

from em_text_fix_worker import Engine


class InlineWorker:
    """An em_text_fix_worker.Compute that runs each job at once on its own Engine. `lose()`
    stands for the child exiting: the engine's state is gone and the service hears it."""

    def __init__(self, on_lost):
        self.on_lost = on_lost
        self.engine = Engine()
        self.asked = []

    def start(self):
        pass

    async def close(self):
        pass

    async def ask(self, job):
        self.asked.append(job)
        return job.run(self.engine)

    def lose(self):
        self.engine = Engine()
        self.on_lost()


def inline_workers():
    """A worker factory for TextFixService, and the workers it made."""
    made: list[InlineWorker] = []

    def factory(on_lost):
        made.append(InlineWorker(on_lost))
        return made[-1]

    return factory, made
