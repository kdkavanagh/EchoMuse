import asyncio

import numpy as np
import pytest

import em_player
from em_player import IDLE, PAUSED, PLAYING, RATE

BLOCK_FRAMES = 3840
DEV = "office"
URL = "http://music/track.flac"
ORIGINAL_STREAM_URL = em_player.em_media.stream_url


class FakePlayback:
    """em_render.Playback per the wave-2 contract: futures + idempotent cancel.
    `consume(n)` pulls n blocks from the source and reports them rendered."""

    def __init__(self, source_class, source, generation):
        loop = asyncio.get_running_loop()
        self.playback_id = f"pb-{generation}"
        self.generation = generation
        self.source_class = source_class
        self.source = source
        self.started = loop.create_future()
        self.finished = loop.create_future()
        self.last_progress = None
        self.cancels = []
        self.completed_frames = 0

    async def consume(self, blocks):
        for _ in range(blocks):
            block = await anext(self.source)
            self.completed_frames += len(block)
        self.last_progress = {"playback_id": self.playback_id, "event": "progress",
                              "submitted_frames": str(self.completed_frames),
                              "completed_frames": str(self.completed_frames)}

    def finish(self, reason):
        if not self.finished.done():
            self.finished.set_result({"playback_id": self.playback_id,
                                      "last_completed_frame": str(self.completed_frames),
                                      "reason": reason, "timing_quality": "estimated"})

    async def cancel(self, reason):
        self.cancels.append(reason)
        self.finish("cancelled")


class FakeRender:
    def __init__(self):
        self.playbacks = []

    async def play_stream(self, source_class, pcm48, *, generation, gain_db=0.0, announcement=False):
        pb = FakePlayback(source_class, pcm48, generation)
        self.playbacks.append(pb)
        return pb


class Env:
    """Wires em_player to a fake render client, a fake decoder, a dialog
    flag, and a recorder of HA state pushes."""

    def __init__(self, blocks=100, seekable=True):
        self.render = FakeRender()
        self.online = True
        self.dialog = False
        self.pushed = []
        self.decodes = []
        self.blocks = blocks
        self.seekable = seekable

        async def notify(device_id, st):
            self.pushed.append(st)

        em_player.init(get_render=lambda d: self.render if self.online and d == DEV else None,
                       notify_state=notify,
                       dialog_active=lambda d: self.dialog)
        em_player.em_media.stream_url = self.decode

    async def decode(self, url, *, rate, start_s):
        assert rate == RATE
        self.decodes.append((url, start_s))
        if start_s > 0 and not self.seekable:
            await asyncio.Event().wait()   # ffmpeg discarding up to an unreachable -ss
        for i in range(self.blocks):
            yield np.full(BLOCK_FRAMES, i, dtype=np.int16)

    @property
    def pb(self):
        return self.render.playbacks[-1]

    async def started(self, count=1):
        for _ in range(200):
            if len(self.render.playbacks) >= count:
                await asyncio.sleep(0)
                return self.pb
            await asyncio.sleep(0.001)
        raise AssertionError("no playback started")


@pytest.fixture(autouse=True)
def _fresh(monkeypatch):
    monkeypatch.setattr(em_player, "_sessions", {})
    monkeypatch.setattr(em_player.em_media, "stream_url", ORIGINAL_STREAM_URL)
    monkeypatch.setattr(em_player, "SEEK_STALL_S", 0.05)
    monkeypatch.setattr(em_player, "CANCEL_WAIT_S", 0.05)


async def _settle():
    for _ in range(5):
        await asyncio.sleep(0)


def test_play_streams_decoded_content_with_fresh_generations():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        assert pb.source_class == "content"
        await pb.consume(2)
        assert em_player.state(DEV) == PLAYING and em_player.is_playing(DEV)
        assert env.decodes == [(URL, 0.0)]
        assert env.pushed == [PLAYING]

        await em_player.play(DEV, "http://music/next.flac")
        pb2 = await env.started(2)
        assert pb.cancels == ["replaced"]
        assert pb2.generation > pb.generation
        await pb2.consume(1)
        assert em_player.state(DEV) == PLAYING
        await em_player.stop(DEV)
    asyncio.run(main())


def test_pause_cancels_and_bookmarks_rendered_frames():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        await pb.consume(3)
        await em_player.pause(DEV)
        s = em_player._sessions[DEV]
        assert pb.cancels == ["paused"]
        assert em_player.state(DEV) == PAUSED
        assert s.position_s == pytest.approx(3 * BLOCK_FRAMES / RATE)
        assert env.pushed[-1] == PAUSED
        assert s._task is None
    asyncio.run(main())


def test_resume_restarts_from_bookmark_and_accumulates_position():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        await pb.consume(3)
        await em_player.pause(DEV)
        await em_player.resume(DEV)
        pb2 = await env.started(2)
        first = await anext(pb2.source)
        pb2.completed_frames += len(first)
        bookmark = 3 * BLOCK_FRAMES / RATE
        assert env.decodes == [(URL, 0.0), (URL, pytest.approx(bookmark))]
        assert em_player.state(DEV) == PLAYING and env.pushed[-1] == PLAYING
        await pb2.consume(1)
        await em_player.pause(DEV)
        assert em_player._sessions[DEV].position_s == pytest.approx(
            bookmark + 2 * BLOCK_FRAMES / RATE)
    asyncio.run(main())


def test_bookmark_uses_last_progress_when_finished_never_arrives():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        await pb.consume(2)

        async def silent_cancel(reason):
            pb.cancels.append(reason)
        pb.cancel = silent_cancel
        await em_player.pause(DEV)
        assert em_player.state(DEV) == PAUSED
        assert em_player._sessions[DEV].position_s == pytest.approx(2 * BLOCK_FRAMES / RATE)
    asyncio.run(main())


def test_stop_cancels_and_clears_session():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        await pb.consume(1)
        await em_player.stop(DEV)
        s = em_player._sessions[DEV]
        assert pb.cancels == ["stopped"]
        assert em_player.state(DEV) == IDLE
        assert s.url is None and s.position_s == 0.0
        assert env.pushed[-1] == IDLE
        await em_player.resume(DEV)
        await _settle()
        assert len(env.render.playbacks) == 1, "nothing to resume after stop"
    asyncio.run(main())


@pytest.mark.parametrize("reason", ["drained", "underrun", "failed", "cancelled"])
def test_playback_ending_by_itself_goes_idle(reason):
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        await pb.consume(1)
        pb.finish(reason)
        await _settle()
        s = em_player._sessions[DEV]
        assert em_player.state(DEV) == IDLE
        assert s.url is None and s._playback is None and s._task is None
        assert env.pushed[-1] == IDLE
    asyncio.run(main())


def test_pause_racing_a_natural_drain_goes_idle_not_paused():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        await pb.consume(1)
        pb.finish("drained")
        await em_player.pause(DEV)    # before the watcher observed the drain
        await _settle()
        assert pb.cancels == [], "no render.cancel for a finished playback"
        assert em_player.state(DEV) == IDLE
    asyncio.run(main())


def test_stale_finish_of_a_replaced_playback_is_ignored():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        old = await env.started()

        async def ignore_cancel(reason):
            old.cancels.append(reason)
        old.cancel = ignore_cancel
        await em_player.play(DEV, "http://music/next.flac")
        await env.started(2)
        old.finish("failed")
        await _settle()
        assert em_player.state(DEV) == PLAYING
        await em_player.stop(DEV)
    asyncio.run(main())


def test_commands_during_dialog_are_deferred_and_the_last_wins():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        await pb.consume(1)

        env.dialog = True
        await em_player.pause(DEV)
        assert pb.cancels == [] and em_player.state(DEV) == PLAYING
        assert em_player.reported_state(DEV) == PAUSED
        assert env.pushed[-1] == PAUSED, "HA is told the intent at once"
        await em_player.stop(DEV)
        assert em_player.reported_state(DEV) == IDLE and env.pushed[-1] == IDLE
        assert pb.cancels == []

        env.dialog = False
        await em_player.dialog_released(DEV)
        assert pb.cancels == ["stopped"]
        assert em_player.state(DEV) == IDLE
        assert em_player._sessions[DEV].deferred is None
    asyncio.run(main())


def test_resume_after_pause_during_dialog_leaves_content_playing():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        env.dialog = True
        await em_player.pause(DEV)
        await em_player.resume(DEV)
        assert em_player.reported_state(DEV) == PLAYING
        env.dialog = False
        await em_player.dialog_released(DEV)
        assert pb.cancels == [] and em_player.state(DEV) == PLAYING
        assert len(env.render.playbacks) == 1
        await em_player.stop(DEV)
    asyncio.run(main())


def test_deferred_resume_of_paused_content_applies_at_release():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        await pb.consume(2)
        await em_player.pause(DEV)
        env.dialog = True
        await em_player.resume(DEV)
        await _settle()
        assert len(env.render.playbacks) == 1 and em_player.state(DEV) == PAUSED
        env.dialog = False
        await em_player.dialog_released(DEV)
        await env.started(2)
        assert em_player.state(DEV) == PLAYING
        assert env.decodes[-1] == (URL, pytest.approx(2 * BLOCK_FRAMES / RATE))
        await em_player.stop(DEV)
    asyncio.run(main())


def test_play_during_dialog_starts_now_and_supersedes_a_deferred_command():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        env.dialog = True
        await em_player.stop(DEV)
        await em_player.play(DEV, "http://music/jazz")
        pb2 = await env.started(2)
        assert pb.cancels == ["replaced"]
        assert em_player.state(DEV) == PLAYING == em_player.reported_state(DEV)
        env.dialog = False
        await em_player.dialog_released(DEV)
        assert pb2.cancels == [] and em_player.state(DEV) == PLAYING
        await em_player.stop(DEV)
    asyncio.run(main())


def test_natural_end_during_dialog_drops_the_deferred_command():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        env.dialog = True
        await em_player.pause(DEV)
        pb.finish("drained")
        await _settle()
        assert em_player.reported_state(DEV) == IDLE and env.pushed[-1] == IDLE
        env.dialog = False
        await em_player.dialog_released(DEV)
        assert em_player.state(DEV) == IDLE
    asyncio.run(main())


def test_no_render_session_means_idle_without_error():
    async def main():
        env = Env()
        env.online = False
        await em_player.play(DEV, URL)
        assert em_player.state(DEV) == IDLE and env.pushed == [IDLE]
        assert env.render.playbacks == [] and env.decodes == []

        env.online = True
        await em_player.play(DEV, URL)
        pb = await env.started()
        await pb.consume(1)
        await em_player.pause(DEV)
        env.online = False
        await em_player.resume(DEV)
        assert em_player.state(DEV) == IDLE and env.pushed[-1] == IDLE
    asyncio.run(main())


def test_device_gone_drops_playback_without_wire_traffic():
    async def main():
        env = Env()
        await em_player.play(DEV, URL)
        pb = await env.started()
        await pb.consume(1)
        s = em_player._sessions[DEV]
        task = s._task
        em_player.device_gone(DEV)
        await _settle()
        assert task.done()
        assert pb.cancels == []
        assert DEV not in em_player._sessions
        assert em_player.state(DEV) == IDLE
        em_player.device_gone("never-seen")
    asyncio.run(main())


def test_stop_while_playback_is_starting_cancels_it_once_it_exists():
    async def main():
        env = Env()
        gate = asyncio.Event()
        plain = env.render.play_stream

        async def slow_play_stream(*a, **kw):
            await gate.wait()
            return await plain(*a, **kw)
        env.render.play_stream = slow_play_stream
        await em_player.play(DEV, URL)
        await _settle()
        await em_player.stop(DEV)
        gate.set()
        await _settle()
        assert env.pb.cancels == ["stopped"]
        assert em_player.state(DEV) == IDLE
    asyncio.run(main())


def test_unseekable_resume_rejoins_the_live_edge_and_is_learned():
    async def main():
        env = Env(seekable=False)
        await em_player.play(DEV, URL)
        pb = await env.started()
        await pb.consume(2)
        await em_player.pause(DEV)
        await em_player.resume(DEV)
        pb2 = await env.started(2)
        await pb2.consume(1)
        assert env.decodes[1:] == [(URL, pytest.approx(2 * BLOCK_FRAMES / RATE)), (URL, 0.0)]
        await em_player.pause(DEV)
        assert em_player._sessions[DEV].position_s == pytest.approx(BLOCK_FRAMES / RATE)
        await em_player.resume(DEV)
        pb3 = await env.started(3)
        await pb3.consume(1)
        assert env.decodes[3:] == [(URL, 0.0)], "known unseekable: no second probe"
        await em_player.stop(DEV)
    asyncio.run(main())


def test_media_ending_before_the_bookmark_goes_idle():
    async def main():
        env = Env(blocks=0)
        s = em_player._session(DEV)
        s.url, s.position_s, s.state = URL, 12.0, PAUSED
        await em_player.resume(DEV)
        await _settle()
        assert em_player.state(DEV) == IDLE and env.render.playbacks == []
    asyncio.run(main())


