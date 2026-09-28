"""Device speech-asset set and hash-addressed asset reads (§16.1, §16.5)."""

import hashlib
import json

import numpy as np
import pytest

import em_device_assets as da
import em_wake_registry as wr

SIDECAR = {
    "type": "bcresnet", "sampleRate": 16000, "window": 22400,
    "labels": ["noise", "ohphelia", "unknown"], "wakeIndex": 1,
    "nMels": 40, "clipSeconds": 1.4, "normPeak": 0.8,
}
T = wr.Thresholds(0.9, 0.65, 0.3, 0.17)


def stub_loader(graph, spec):
    logits = np.log(np.array([[0.4975, 0.005, 0.4975]])).astype(np.float32)
    return (lambda x: logits), "stub"


@pytest.fixture
def env(tmp_path, monkeypatch):
    runtime = tmp_path / "libonnxruntime.so"
    runtime.write_bytes(b"\x7fELF runtime " * 20_000)
    monkeypatch.setattr(da, "RUNTIME_SHA256", hashlib.sha256(runtime.read_bytes()).hexdigest())
    monkeypatch.setenv(da.RUNTIME_ENV, str(runtime))
    active = {"wakeModel": None}
    reg = wr.WakeRegistry(tmp_path / "oww_models", active_getter=lambda: active, infer_loader=stub_loader)
    g, s = tmp_path / "g.onnx", tmp_path / "g.json"
    g.write_bytes(b"graph" * 100)
    s.write_text(json.dumps(SIDECAR))
    model = reg.register(g, s, T, "ophelia", "ophel")
    active["wakeModel"] = model.graph_sha256
    wav = tmp_path / "alert.wav"
    wav.write_bytes(b"RIFF" + bytes(range(256)) * 10)
    alerts = {hashlib.sha256(wav.read_bytes()).hexdigest(): wav}
    assets = da.DeviceAssets(reg, alerts.get)
    return assets, model, runtime, wav, alerts


def test_session_ready_names_runtime_and_active_graph_pair(env):
    assets, model, *_ = env
    assert assets.speech_assets(model).wire() == {
        "runtime_sha256": da.RUNTIME_SHA256,
        "graph_sha256": model.graph_sha256,
        "sidecar_sha256": model.sidecar_sha256,
    }


def test_assets_fetched_after_hello_count_once_wake_stats_show_the_graph_loaded():
    named = {"runtime_sha256": "r", "graph_sha256": "g", "sidecar_sha256": "s"}
    # A fresh device announces nothing, then downloads and loads the set.
    assert da.installed_speech_assets([], named, None) == []
    assert da.installed_speech_assets([], named, {"graph_sha256": "g", "wake_unavailable": None}) == ["r", "g", "s"]
    # A different loaded graph, or a load failure, proves nothing about this set.
    assert da.installed_speech_assets(["r"], named, {"graph_sha256": "h", "wake_unavailable": None}) == ["r"]
    assert da.installed_speech_assets([], named, {"graph_sha256": "g", "wake_unavailable": "load_failed"}) == []


def test_runtime_with_wrong_hash_is_refused_at_construction(env, tmp_path, monkeypatch):
    assets, *_ = env
    bad = tmp_path / "bad.so"
    bad.write_bytes(b"other")
    with pytest.raises(da.AssetError, match="SHA-256"):
        da.DeviceAssets(assets.registry, lambda s: None, runtime_path=bad)


def test_every_asset_kind_resolves_by_hash(env):
    assets, model, runtime, wav, alerts = env
    assert assets.resolve(da.RUNTIME_SHA256).path == runtime
    assert assets.resolve(model.graph_sha256).size == 500
    assert assets.resolve(model.sidecar_sha256).path.suffix == ".json"
    (alert_sha,) = alerts
    assert assets.resolve(alert_sha).path == wav


@pytest.mark.parametrize("value", ["0" * 64, "ABC", 5, "../../etc/passwd"])
def test_unknown_or_malformed_hash_is_not_found(env, value):
    assets, *_ = env
    with pytest.raises(da.AssetNotFound):
        assets.resolve(value)


def test_changed_file_is_served_as_not_found(env):
    assets, _, _, wav, alerts = env
    (alert_sha,) = alerts
    wav.write_bytes(b"different content")
    with pytest.raises(da.AssetNotFound):
        assets.resolve(alert_sha)


def test_chunks_resume_from_offset_and_never_exceed_64_kib(env):
    assets, _, runtime, *_ = env
    data = runtime.read_bytes()
    chunks = list(assets.chunks(da.RUNTIME_SHA256, 1000))
    assert b"".join(chunks) == data[1000:]
    assert max(map(len, chunks)) == 65_536 and all(len(c) <= 65_536 for c in chunks)
    assert list(assets.chunks(da.RUNTIME_SHA256, len(data))) == []
    assert assets.completion(da.RUNTIME_SHA256) == {"sha256": da.RUNTIME_SHA256, "size": len(data), "done": True}


@pytest.mark.parametrize("offset", [-1, True, 10**9])
def test_invalid_offsets_are_refused(env, offset):
    assets, *_ = env
    with pytest.raises(da.AssetError):
        list(assets.chunks(da.RUNTIME_SHA256, offset))


def test_request_must_be_exactly_sha256_and_offset(env):
    assets, *_ = env
    info, offset = assets.parse_request({"sha256": da.RUNTIME_SHA256, "offset": 7})
    assert offset == 7 and info.sha256 == da.RUNTIME_SHA256
    with pytest.raises(da.AssetError):
        assets.parse_request({"sha256": da.RUNTIME_SHA256})
    with pytest.raises(da.AssetError):
        assets.parse_request({"sha256": da.RUNTIME_SHA256, "offset": 0, "extra": 1})
