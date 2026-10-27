"""Cropping saved clips around the windows the wake model scores highest (em_clip_crop), and the
archives that hold the crops (GET /api/near_misses.zip and /api/wakeclips.zip with `crop`)."""

import asyncio
import io
import wave
import zipfile
from pathlib import Path
from types import SimpleNamespace

import numpy as np
import pytest

import em_clip_crop as crop
import em_db as db
import em_recordings
import em_wake_registry
from em_clip_crop import Crop, CropSpec, WindowScore

SIDECAR = Path(__file__).resolve().parents[2] / "bcresnet_audio.json"
SR = 16_000
W = crop.WINDOW


def _clip(seconds=3.0, burst=(1.2, 1.7)):
    """Low noise with one loud burst: the stretch a fake wake model scores."""
    rng = np.random.default_rng(0)
    pcm = rng.normal(0, 200, int(seconds * SR)).astype(np.int16)
    a, b = int(burst[0] * SR), int(burst[1] * SR)
    pcm[a:b] = (20_000 * np.sin(np.arange(b - a) * 0.3)).astype(np.int16)
    return pcm


def _centred_infer(x):
    """A fake graph whose wake probability grows as the window's loud stretch sits nearer its
    centre, and is about zero for a window of noise alone."""
    mag = np.abs(x[0])
    tri = 1 - np.abs(np.arange(mag.size) - mag.size / 2) / (mag.size / 2)
    p = float(np.clip((np.sum(mag * tri) / np.sum(mag) - 0.5) * 2, 1e-3, 0.999))
    return np.array([[np.log(1 - p), np.log(p), -50.0]], dtype=np.float32)


class _Registry:
    """The registry surface ClipScorer reads: `graphs` registered, `active_sha` the fleet's."""

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


def _scorer(graphs=("g1",), active="g1", loaded=None):
    def load(path, spec):
        if loaded is not None:
            loaded.append(path.stem)
        return _centred_infer
    return crop.ClipScorer(_Registry(graphs, active), load=load)


def test_windows_cover_the_clip_on_the_dots_hop_and_end_at_its_end():
    """Every 160 ms from the start, plus one ending at the clip's end when the grid stops short;
    a clip shorter than a window has none."""
    assert crop.window_starts(W) == [0]
    assert crop.window_starts(W + 2 * crop.HOP) == [0, crop.HOP, 2 * crop.HOP]
    assert crop.window_starts(W + crop.HOP + 100) == [0, crop.HOP, crop.HOP + 100]
    assert crop.window_starts(W - 1) == []


def test_a_crop_centres_on_its_window_and_stays_inside_the_clip():
    """The best window alone, or every window at or above window_min in clip order; a crop that
    would run past an end moves inside, one longer than the clip is the clip, and windows whose
    crops land on the same samples give one crop with the higher score."""
    n = W + 10 * crop.HOP
    scores = [WindowScore(0, 0.2), WindowScore(4 * crop.HOP, 0.7), WindowScore(10 * crop.HOP, 0.4)]
    assert crop.plan(n, scores, CropSpec(W)) == [Crop(4 * crop.HOP, 4 * crop.HOP + W, 0.7)]
    assert crop.plan(n, scores, CropSpec(W // 2, window_min=0.3)) == [
        Crop(4 * crop.HOP + W // 4, 4 * crop.HOP + 3 * W // 4, 0.7),
        Crop(10 * crop.HOP + W // 4, 10 * crop.HOP + 3 * W // 4, 0.4)]
    long = 2 * W   # too long to centre: the first two windows' crops both move to the start
    assert crop.plan(n, scores, CropSpec(long, window_min=0.1)) == [Crop(0, long, 0.7), Crop(n - long, n, 0.4)]
    assert crop.plan(n, scores, CropSpec(n + 1)) == [Crop(0, n, 0.7)]
    assert crop.plan(n, scores, CropSpec(W, window_min=0.8)) == []
    assert crop.plan(n, [], CropSpec(W)) == []


def test_a_clip_is_scored_with_the_graph_that_scored_it_while_the_registry_holds_it(tmp_path):
    """The clip's own graph when it is registered; the fleet's active graph when it was deleted
    or never recorded; RegistryError when there is neither."""
    path = tmp_path / "clip.wav"
    path.write_bytes(em_recordings.encode_wav(_clip().tobytes()))
    loaded = []
    scorer = _scorer(graphs=("g1", "g2"), active="g2", loaded=loaded)
    for graph in ("g1", "deleted", None):
        scorer.plan_archive([crop.Clip("clip.wav", path, graph)], CropSpec(W))
    assert loaded == ["g1", "g2"]
    with pytest.raises(em_wake_registry.RegistryError):
        _scorer(graphs=(), active=None).plan_archive([crop.Clip("clip.wav", path, "g1")], CropSpec(W))


def test_the_best_crop_holds_the_stretch_that_scored(tmp_path):
    """Scored every 160 ms, a 3 s clip with a burst at 1.2–1.7 s gives its best window at
    0.80 s, and a 1.4 s crop of exactly that window."""
    path = tmp_path / "clip.wav"
    pcm = _clip()
    path.write_bytes(em_recordings.encode_wav(pcm.tobytes()))
    [(name, source, c)] = _scorer().plan_archive([crop.Clip("kitchen-nearmiss7.wav", path, "g1")], CropSpec(W))
    assert (c.start, c.end) == (12_800, 12_800 + W)
    assert name == f"kitchen-nearmiss7-0.80s-{c.score:.2f}.wav"
    with wave.open(io.BytesIO(crop.cut(source, c.start, c.end))) as w:
        assert np.array_equal(np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16), pcm[c.start:c.end])


@pytest.fixture()
def fresh_db(tmp_path, monkeypatch):
    monkeypatch.setenv("DB_PATH", str(tmp_path / "test.db"))
    db.init(str(tmp_path / "test.db"))
    yield db
    if db._conn is not None:
        db._conn.close()
        db._conn = None


def test_a_cropped_archive_holds_the_crops_and_names_how_it_was_cut(fresh_db, monkeypatch):
    """With `crop`, near_misses.zip holds each recording's crop around its best window, or with
    `window_min` around every window scoring at or above it, and names the archive for both; a
    window_min no window reaches is a 404 that says so, and a bad crop is a 400."""
    em_api = pytest.importorskip("em_api")
    from aiohttp import ClientSession, web
    from aiohttp.test_utils import TestServer

    db.register_new_device("dev1", "1.2.3.4", None, None)
    db.approve_device("dev1", "Kitchen")
    record = db.NearMissRecord(
        ts=1.0, near_miss_uuid="n1", profile=db.em_wake_rules.RuleProfile.IDLE, floor=0.17, peak=0.4,
        wake_threshold=0.9, wake_model_sha256="g1", duration_ms=2040, end_reason=db.NearMissEnd.BELOW,
        recording=db.NearMissRecording.SAVED)
    nm = db.insert_near_miss("dev1", record)
    em_recordings.save("dev1", nm, _clip().tobytes(), kind=em_recordings.RecordingKind.NEAR_MISS)
    scorer = _scorer()
    monkeypatch.setattr(em_api, "_clip_scorer", lambda request: scorer)

    async def main():
        app = web.Application(middlewares=[em_api._error_middleware])
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
    [(name, frames)] = wavs(best_body).items()
    assert name.startswith(f"kitchen-nearmiss{nm}-0.80s-") and frames == W
    crops = wavs(every_body)
    starts = [float(n.split("-")[2].removesuffix("s")) for n in crops]
    assert len(crops) > 1 and starts == sorted(starts) and set(crops.values()) == {W}
    assert all(float(n.split("-")[3].removesuffix(".wav")) >= 0.05 for n in crops)
    assert b"scored at or above 0.99" in none_body
    assert [status for status, *_ in bad] == [400, 400, 400, 400]
