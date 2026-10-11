import asyncio

import numpy as np
import pytest

pytest.importorskip("websockets")  # em_render imports em_device_link

import em_audio_timeline as tl  # noqa: E402
import em_render  # noqa: E402
from em_device_link import AckStatus, CommandAck, Envelope, MessageType  # noqa: E402


def envelope(msg_type, body, generation):
    return Envelope(MessageType(msg_type), "s", "m", "dev", generation, body)


class FakeLink:
    def __init__(self, *, auto_start=True):
        self.client = None
        self.starts = []
        self.auto_start = auto_start
        self.commands = []
        self.packets = []
        self.sent_frames = 0
        self.completed_frames = 0
        self.max_in_flight = 0
        self.end = asyncio.Event()
        self._message = 0

    async def request(self, msg_type, body, *, generation=0, timeout=2.0):
        assert msg_type == "render.start"
        self.starts.append((body, generation, timeout))
        if self.auto_start:
            asyncio.get_running_loop().call_soon(
                self.progress, body["playback_id"], generation, "start")
        return CommandAck("start", AckStatus.ACCEPTED, None)
    async def send(self, msg_type, body, *, generation=0):
        self._message += 1
        mid = f"message-{self._message}"
        self.commands.append((msg_type, body, generation, mid))
        if msg_type == "render.end":
            self.end.set()
        return mid

    async def send_audio(self, frame):
        packet = tl.parse_packet(frame)
        assert packet.kind is tl.Kind.RENDER
        assert packet.sequence == len(self.packets)
        assert packet.first_sample == self.sent_frames
        self.packets.append(packet)
        self.sent_frames += packet.frame_count
        in_flight = self.sent_frames - self.completed_frames
        self.max_in_flight = max(self.max_in_flight, in_flight)
        assert in_flight <= em_render.MAX_IN_FLIGHT
        # Report one packet complete at the FIFO high-water mark. The next
        # send then proves the renderer really waited for progress.
        if in_flight + em_render.PACKET_FRAMES > em_render.MAX_IN_FLIGHT:
            asyncio.get_running_loop().call_soon(
                self.complete, packet.frame_count, packet.epoch, packet.generation)

    def complete(self, frames, epoch, generation):
        del epoch
        self.completed_frames += frames
        playback_id = self.starts[-1][0]["playback_id"]
        self.progress(playback_id, generation, "progress")

    def progress(self, playback_id, generation, event):
        self.client.on_message(envelope("render.progress", {
            "playback_id": playback_id,
            "event": event,
            "submitted_frames": tl.format_u64(self.sent_frames),
            "completed_frames": tl.format_u64(self.completed_frames),
            "mono_ns": "1",
        }, generation))

    def finish(self, playback, *, generation=None, reason="drained"):
        self.client.on_message(envelope("render.finished", {
            "playback_id": playback.playback_id,
            "last_completed_frame": tl.format_u64(self.completed_frames),
            "reason": reason,
            "timing_quality": "estimated",
        }, playback.generation if generation is None else generation))


def chunks(count, frames=em_render.PACKET_FRAMES, value=1):
    async def source():
        for _ in range(count):
            yield np.full(frames, value, dtype=np.int16)
    return source()


def test_pacing_fifo_bound_and_render_end_at_eof():
    async def run():
        link = FakeLink()
        client = link.client = em_render.RenderClient(link)
        playback = await client.play_stream("content", chunks(90), generation=17)
        await asyncio.wait_for(link.end.wait(), 1.0)
        assert len(link.packets) == 90
        assert link.max_in_flight <= em_render.MAX_IN_FLIGHT
        # end_frame: the device waits for audio still in flight behind render.end.
        assert link.commands[-1][:3] == ("render.end", {
            "playback_id": playback.playback_id,
            "end_frame": tl.format_u64(90 * em_render.PACKET_FRAMES)}, 17)
        assert link.starts[0][0]["epoch"] == tl.format_u64(link.packets[0].epoch)
        assert all(packet.generation == 17 for packet in link.packets)
        assert playback.started.done()
        assert not playback.finished.done()
        link.finish(playback)
        assert (await playback.finished)["reason"] == "drained"

    asyncio.run(run())


def test_network_eq_hook_applies_to_pcm_and_is_created_once():
    class Double:
        def __init__(self):
            self.calls = 0

        def process(self, pcm):
            self.calls += 1
            values = np.frombuffer(pcm, dtype=np.int16).astype(np.int32) * 2
            return np.clip(values, -32768, 32767).astype(np.int16).tobytes()

    async def run():
        filters = []

        def eq():
            filters.append(Double())
            return filters[-1]

        link = FakeLink()
        client = link.client = em_render.RenderClient(link, eq=eq)
        playback = await client.play_stream(
            "dialog_output", chunks(2, frames=100, value=900), generation=2)
        await asyncio.wait_for(link.end.wait(), 1.0)
        assert len(filters) == 1 and filters[0].calls == 2
        pcm = np.concatenate([packet.pcm() for packet in link.packets])
        np.testing.assert_array_equal(pcm, np.full(200, 1800, dtype=np.int16))
        link.finish(playback)

    asyncio.run(run())

def test_cancel_is_idempotent_and_stale_generation_is_fenced():
    async def run():
        link = FakeLink(auto_start=False)
        client = link.client = em_render.RenderClient(link)
        playback = await client.play_local(
            "earcon", "builtin:wake_chime", generation=7, gain_db=-3.0)
        for _ in range(20):
            if link.starts:
                break
            await asyncio.sleep(0)
        assert link.starts[0][0] == {
            "playback_id": playback.playback_id,
            "source_class": "earcon",
            "gain_db": -3.0,
            "format": tl.FORMAT_PCM16,
            "local_asset": "builtin:wake_chime",
            "announcement": False,
        }

        stale_progress = client.on_message(envelope("render.progress", {
            "playback_id": playback.playback_id, "event": "start",
            "submitted_frames": "0", "completed_frames": "0"}, 6))
        assert stale_progress is True
        assert not playback.started.done()
        link.progress(playback.playback_id, 7, "start")
        await playback.started

        await playback.cancel("user")
        await playback.cancel("again")
        cancels = [c for c in link.commands if c[0] == "render.cancel"]
        assert len(cancels) == 1
        assert cancels[0][:3] == (
            "render.cancel", {"playback_id": playback.playback_id, "reason": "user"}, 7)

        link.finish(playback, generation=6, reason="cancelled")
        assert not playback.finished.done()
        link.finish(playback, generation=7, reason="cancelled")
        assert (await playback.finished)["reason"] == "cancelled"

    asyncio.run(run())


def test_source_failure_and_session_loss_finish_failed():
    async def broken():
        yield np.ones(20, dtype=np.int16)
        raise RuntimeError("decoder broke")

    async def run():
        link = FakeLink()
        client = link.client = em_render.RenderClient(link)
        broken_playback = await client.play_stream(
            "dialog_output", broken(), generation=3)
        result = await asyncio.wait_for(broken_playback.finished, 1.0)
        assert result["reason"] == "failed"
        assert "decoder broke" in result["detail"]
        assert [cmd[0] for cmd in link.commands] == ["render.cancel"]

        local = await client.play_local(
            "alert_preview", "f" * 64, generation=4)
        for _ in range(20):
            if len(link.starts) >= 2:
                break
            await asyncio.sleep(0)
        client.fail_all("session_lost")
        result = await local.finished
        assert result["reason"] == "failed"
        assert result["detail"] == "session_lost"
        assert client.playbacks == ()

    asyncio.run(run())


def test_first_frame_time_is_the_first_completion_less_the_frames_it_completed():
    async def run():
        link = FakeLink(auto_start=False)
        client = link.client = em_render.RenderClient(link)
        playback = await client.play_local("earcon", "builtin:wake_chime", generation=5)
        for _ in range(20):
            if link.starts:
                break
            await asyncio.sleep(0)

        def progress(event, completed, mono):
            client.on_message(envelope("render.progress", {
                "playback_id": playback.playback_id, "event": event, "submitted_frames": "9600",
                "completed_frames": str(completed), "mono_ns": str(mono)}, 5))

        progress("start", 0, 5_000_000_000)          # mixed, nothing completed yet: no timing
        await playback.started
        assert playback.first_frame_ns is None
        progress("progress", 4_800, 5_200_000_000)   # 100 ms of it completed by then
        assert playback.first_frame_ns == 5_100_000_000
        progress("progress", 9_600, 5_400_000_000)   # a later stall must not move the start
        assert playback.first_frame_ns == 5_100_000_000

    asyncio.run(run())
