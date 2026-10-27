"""The wake model's windows over each saved clip (em_clip_windows): scored once in the background
and kept on the clip's row, and the crops the archives cut around them (GET /api/near_misses.zip
and /api/wakeclips.zip with `crop`)."""

import asyncio
import io
import wave
import zipfile
from pathlib import Path
from types import SimpleNamespace

import numpy as np
import pytest

import em_clip_windows as cw
import em_db as db
import em_recordings
import em_wake_registry
import em_wakeclips
from em_clip_windows import ClipWindows, Crop, CropSpec, WindowScore

SIDECAR = Path(__file__).resolve().parents[2] / "bcresnet_audio.json"
SR = 16_000
W = cw.WINDOW


def _clip(seconds=3.0, burst=(1.2, 1.7)):
    """Low noise with one loud burst: the stretch a fake wake model scores."""
    rng = np.random.default_rng(0)
    pcm = rng.normal(0, 200, int(seconds * SR)).astype(np.int16)
    a, b = int(burst[0] * SR), int(burst[1] * SR)
    pcm[a:b] = (20_000 * np.sin(np.arange(b - a) * 0.3)).astype(np.int16)
    return pcm


class _Graph:
    """A fake graph whose wake probability grows as the window's loud stretch sits nearer its
    centre, and is about zero for a window of noise alone. `calls` counts the windows scored."""

    def __init__(self):
        self.calls = 0

    def __call__(self, x):
        self.calls += 1
        mag = np.abs(x[0])
        tri = 1 - np.abs(np.arange(mag.size) - mag.size / 2) / (mag.size / 2)
        p = float(np.clip((np.sum(mag * tri) / np.sum(mag) - 0.5) * 2, 1e-3, 0.999))
        return np.array([[np.log(1 - p), np.log(p), -50.0]], dtype=np.float32)


class _Registry:
    """The registry surface the scorer reads: `graphs` registered, `active_sha` the fleet's."""

    def __init__(self, graphs, active_sha):
        self.graphs, self.active_sha = set(graphs), active_sha

    def get(self, sha):
        if sha not in self.graphs:
            raise em_wake_registry.RegistryError(f"unknown {sha}")
        return SimpleNamespace(graph_sha256=sha)

    def active(self):
        if self.active_sha is None:
            raise em_wake_registry.RegistryError("fleet wakeModel is not configured")
        return SimpleNamespace(graph_sha256=self.active_sha)

    def graph_path(self, sha):
        return Path(f"/graphs/{sha}.onnx")

    def sidecar_path(self, sha):
        return SIDECAR


def _service(graphs=("g1",), active="g1", loaded=None, graph=None):
    graph = graph or _Graph()

    def load(path, spec):
        if loaded is not None:
            loaded.append(path.stem)
        return graph
    return cw.ClipWindowsService(_Registry(graphs, active), load=load)


def _windows(n, scores):
    return ClipWindows("g1", n, W, tuple(scores))


def test_windows_cover_the_clip_on_the_dots_hop_and_end_at_its_end():
    """Every 160 ms from the start, plus one ending at the clip's end when the grid stops short;
    a clip shorter than a window has none."""
    assert cw.window_starts(W) == [0]
    assert cw.window_starts(W + 2 * cw.HOP) == [0, cw.HOP, 2 * cw.HOP]
    assert cw.window_starts(W + cw.HOP + 100) == [0, cw.HOP, cw.HOP + 100]
    assert cw.window_starts(W - 1) == []


def test_a_crop_centres_on_its_window_and_stays_inside_the_clip():
    """The best window alone, or every window at or above window_min in clip order; a crop that
    would run past an end moves inside, one longer than the clip is the clip, and windows whose
    crops land on the same samples give one crop with the higher score."""
    n = W + 10 * cw.HOP
    w = _windows(n, [WindowScore(0, 0.2), WindowScore(4 * cw.HOP, 0.7), WindowScore(10 * cw.HOP, 0.4)])
    assert cw.plan(w, CropSpec(W)) == [Crop(4 * cw.HOP, 4 * cw.HOP + W, 0.7)]
    assert cw.plan(w, CropSpec(W // 2, window_min=0.3)) == [
        Crop(4 * cw.HOP + W // 4, 4 * cw.HOP + 3 * W // 4, 0.7),
        Crop(10 * cw.HOP + W // 4, 10 * cw.HOP + 3 * W // 4, 0.4)]
    long = 2 * W   # too long to centre: the first two windows' crops both move to the start
    assert cw.plan(w, CropSpec(long, window_min=0.1)) == [Crop(0, long, 0.7), Crop(n - long, n, 0.4)]
    assert cw.plan(w, CropSpec(n + 1)) == [Crop(0, n, 0.7)]
    assert cw.plan(w, CropSpec(W, window_min=0.8)) == []
    assert cw.plan(_windows(n, []), CropSpec(W)) == []


def test_a_clip_is_scored_with_the_graph_that_scored_it_while_the_registry_holds_it(tmp_path):
    """The clip's own graph when it is registered; the fleet's active graph when it was deleted
    or never recorded; RegistryError when there is neither."""
    path = tmp_path / "clip.wav"
    path.write_bytes(em_recordings.encode_wav(_clip().tobytes()))
    loaded = []
    scorer = _service(graphs=("g1", "g2"), active="g2", loaded=loaded).scorer
    assert [scorer.score(path, graph).graph_sha256 for graph in ("g1", "deleted", None)] == ["g1", "g2", "g2"]
    assert loaded == ["g1", "g2"]
    with pytest.raises(em_wake_registry.RegistryError):
        _service(graphs=(), active=None).scorer.score(path, "g1")


def test_the_best_crop_holds_the_stretch_that_scored(tmp_path):
    """Scored every 160 ms, a 3 s clip with a burst at 1.2–1.7 s has its best window at
    0.80 s, and a 1.4 s crop of exactly that window."""
    path = tmp_path / "clip.wav"
    pcm = _clip()
    path.write_bytes(em_recordings.encode_wav(pcm.tobytes()))
    windows = _service().scorer.score(path, "g1")
    assert (windows.samples, windows.best.start) == (pcm.size, 12_800)
    [c] = cw.plan(windows, CropSpec(W))
    assert (c.start, c.end) == (12_800, 12_800 + W)
    assert cw.crop_name("kitchen-nearmiss7.wav", c) == f"kitchen-nearmiss7-0.80s-{c.score:.2f}.wav"
    with wave.open(io.BytesIO(cw.cut(path, c.start, c.end))) as w:
        assert np.array_equal(np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16), pcm[c.start:c.end])


@pytest.fixture()
def fresh_db(tmp_path, monkeypatch):
    monkeypatch.setenv("DB_PATH", str(tmp_path / "test.db"))
    db.init(str(tmp_path / "test.db"))
    yield db
    if db._conn is not None:
        db._conn.close()
        db._conn = None


def _near_miss(device, ts, graph="g1", *, saved=True):
    record = db.NearMissRecord(
        ts=ts, near_miss_uuid=f"n{ts:g}", profile=db.em_wake_rules.RuleProfile.IDLE, floor=0.17, peak=0.4,
        wake_threshold=0.9, wake_model_sha256=graph, duration_ms=2040, end_reason=db.NearMissEnd.BELOW,
        recording=db.NearMissRecording.SAVED)
    nm = db.insert_near_miss(device, record)
    if saved:
        em_recordings.save(device, nm, _clip().tobytes(), kind=em_recordings.RecordingKind.NEAR_MISS)
    return nm


def test_the_sweep_scores_each_saved_clip_once_and_keeps_its_windows_on_its_row(fresh_db):
    """A sweep scores every wake clip and near-miss recording on disk that has no windows yet
    and stores them on the row; a row whose file is gone is left alone, the next sweep scores
    nothing again, and a clip saved since is scored by the sweep after it."""
    db.register_new_device("dev1", "1.2.3.4", None, None)
    graph = _Graph()
    service = _service(graph=graph)
    assert service.pending is None
    nm = _near_miss("dev1", 1.0)
    gone = _near_miss("dev1", 2.0, saved=False)
    turn = db.insert_turn("dev1", {"ts": 3.0, "trigger": "wake", "wake_score": 0.95, "wake_model_sha256": "g1"})
    db.set_turn_wake(turn, em_wakeclips.save("dev1", turn, _clip().tobytes()))

    assert asyncio.run(service.sweep()) == 2
    assert service.pending == 0
    [near_row] = [r for r in db.get_near_misses("dev1") if r.id == nm]
    assert ClipWindows.from_json(near_row.windows).best.start == 12_800
    assert [r.windows for r in db.get_near_misses("dev1") if r.id == gone] == [None]
    [turn_row] = db.get_wake_clip_turns("dev1", 0, 1, trigger="wake", refused=())
    assert ClipWindows.from_json(turn_row.windows).graph_sha256 == "g1"

    calls = graph.calls
    assert asyncio.run(service.sweep()) == 0 and graph.calls == calls
    later = _near_miss("dev1", 4.0)
    assert asyncio.run(service.sweep()) == 1
    assert [r.windows is not None for r in db.get_near_misses("dev1") if r.id == later] == [True]


def test_a_cropped_archive_cuts_around_the_kept_windows_and_names_how_it_was_cut(fresh_db):
    """With `crop`, near_misses.zip holds each recording's crop around its best window, or with
    `window_min` around every window scoring at or above it, and names the archive for both. A
    recording the sweep has scored is not scored again; one it has not reached is scored then
    and its windows kept. A window_min no window reaches is a 404 that says so, and a bad crop
    is a 400."""
    em_api = pytest.importorskip("em_api")
    from aiohttp import ClientSession, web
    from aiohttp.test_utils import TestServer

    db.register_new_device("dev1", "1.2.3.4", None, None)
    db.approve_device("dev1", "Kitchen")
    graph = _Graph()
    service = _service(graph=graph)
    swept = _near_miss("dev1", 1.0)
    asyncio.run(service.sweep())
    fresh = _near_miss("dev1", 2.0)
    calls = graph.calls

    async def main():
        app = web.Application(middlewares=[em_api._error_middleware])
        app[em_api.SERVICES] = SimpleNamespace(clip_windows=lambda: service)
        app.router.add_get("/api/near_misses.zip", em_api._get_fleet_near_misses_zip.__wrapped__)
        server = TestServer(app, host="127.0.0.1")
        await server.start_server()
        try:
            async with ClientSession() as http:
                async def get(query):
                    async with http.get(server.make_url(f"/api/near_misses.zip?{query}")) as r:
                        return r.status, r.headers.get("Content-Disposition"), await r.read()
                return [await get(q) for q in (
                    "crop=1.4", "crop=1.4&window_min=0.05", "crop=1.4&window_min=0.99",
                    "crop=0", "crop=abc", "window_min=0.5", "crop=1.4&window_min=2")]
        finally:
            await server.close()

    (best, best_name, best_body), (every, every_name, every_body), (none, _, none_body), *bad = asyncio.run(main())
    per_clip = len(cw.window_starts(_clip().size))
    assert graph.calls == calls + per_clip          # only the recording the sweep had not reached
    assert all(r.windows is not None for r in db.get_near_misses("dev1"))

    def wavs(body):
        with zipfile.ZipFile(io.BytesIO(body)) as z:
            out = {}
            for name in z.namelist():
                with wave.open(io.BytesIO(z.read(name))) as w:
                    out[name] = w.getnframes()
            return out

    assert (best, every, none) == (200, 200, 404)
    assert best_name == 'attachment; filename="echomuse-nearmisses-0.00-1.00-crop1.40s.zip"'
    assert every_name == 'attachment; filename="echomuse-nearmisses-0.00-1.00-crop1.40s-min0.05.zip"'
    assert {n.rsplit("-", 1)[0]: f for n, f in wavs(best_body).items()} == {
        f"kitchen-nearmiss{fresh}-0.80s": W, f"kitchen-nearmiss{swept}-0.80s": W}
    crops = wavs(every_body)
    for nm in (swept, fresh):
        starts = [float(n.split("-")[2].removesuffix("s")) for n in crops if n.startswith(f"kitchen-nearmiss{nm}-")]
        assert len(starts) > 1 and starts == sorted(starts)
    assert set(crops.values()) == {W}
    assert all(float(n.split("-")[3].removesuffix(".wav")) >= 0.05 for n in crops)
    assert b"scored at or above 0.99" in none_body
    assert [status for status, *_ in bad] == [400, 400, 400, 400]
