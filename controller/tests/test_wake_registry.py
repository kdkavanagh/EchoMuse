"""BCResNet model registry (§5.1): pinned deployed entry, upload validation and
probe, content addressing, never-activating uploads, and delete-if-inactive."""

import hashlib
import json
from pathlib import Path

import numpy as np
import pytest

import em_wake_registry as wr

ROOT = Path(__file__).resolve().parents[2]
SIDECAR = {
    "type": "bcresnet", "sampleRate": 16000, "window": 22400,
    "labels": ["noise", "ohphelia", "unknown"], "wakeIndex": 1,
    "nMels": 40, "clipSeconds": 1.4, "normPeak": 0.8,
}
T = wr.Thresholds(idle=0.9, playback=0.65, reference=0.3, near_miss=0.17)


def quiet_loader(p=0.005):
    """A graph stub that scores every probe signal at `p`."""
    def loader(graph, spec):
        rest = (1 - p) / 2
        logits = np.log(np.array([[rest, p, rest]])).astype(np.float32)
        return (lambda x: logits), "stub"
    return loader


def pair(tmp_path, name="m", graph=b"graph-bytes", sidecar=None):
    g = tmp_path / f"{name}.onnx"
    s = tmp_path / f"{name}.json"
    g.write_bytes(graph)
    s.write_text(json.dumps(sidecar or SIDECAR))
    return g, s


def sha(b):
    return hashlib.sha256(b).hexdigest()


@pytest.fixture
def active():
    return {"wakeModel": None}


@pytest.fixture
def reg(tmp_path, active):
    return wr.WakeRegistry(tmp_path / "oww_models", active_getter=lambda: active, infer_loader=quiet_loader())


def test_upload_is_content_addressed_and_never_activates(tmp_path, reg, active):
    g, s = pair(tmp_path)
    model = reg.register(g, s, T, "ophelia", "ophel")
    assert model.graph_sha256 == sha(b"graph-bytes")
    assert (reg.directory / f"{model.graph_sha256}.onnx").read_bytes() == b"graph-bytes"
    assert model.probe == {"silence": pytest.approx(0.005), "noise": pytest.approx(0.005), "tone": pytest.approx(0.005)}
    assert active["wakeModel"] is None
    with pytest.raises(wr.RegistryError, match="not configured"):
        reg.active()


def test_index_survives_reload_and_selection_comes_from_fleet_config(tmp_path, reg, active):
    model = reg.register(*pair(tmp_path), T, "ophelia", "ophel")
    active["wakeModel"] = model.graph_sha256
    again = wr.WakeRegistry(reg.directory, active_getter=lambda: active, infer_loader=quiet_loader())
    assert again.active() == model
    assert again.resolve_asset(model.sidecar_sha256) == reg.directory / model.sidecar_file


def test_reload_refuses_a_file_whose_hash_changed(tmp_path, reg):
    model = reg.register(*pair(tmp_path), T, "ophelia", "ophel")
    (reg.directory / model.graph_file).write_bytes(b"tampered")
    with pytest.raises(wr.RegistryError, match="wrong SHA-256"):
        wr.WakeRegistry(reg.directory, infer_loader=quiet_loader())


@pytest.mark.parametrize("change", [
    {"labels": ["a", "b"], "wakeIndex": 5},
    {"sampleRate": 8000},
    {"type": "oww"},
    {"window": 16000},
])
def test_upload_rejects_sidecar_violating_the_contract(tmp_path, reg, change):
    g, s = pair(tmp_path, sidecar={**SIDECAR, **change})
    with pytest.raises(wr.RegistryError, match="invalid BCResNet pair"):
        reg.register(g, s, T, "ophelia", "ophel")
    assert reg.list() == ()


def test_upload_rejects_output_count_differing_from_labels(tmp_path, active):
    def two_logits(graph, spec):
        return (lambda x: np.zeros((1, 2), np.float32)), "stub"
    reg = wr.WakeRegistry(tmp_path / "r", infer_loader=two_logits)
    with pytest.raises(wr.RegistryError, match="expected"):
        reg.register(*pair(tmp_path), T, "ophelia", "ophel")


def test_upload_rejects_a_model_that_fires_on_the_probe(tmp_path):
    reg = wr.WakeRegistry(tmp_path / "r", infer_loader=quiet_loader(p=0.5))
    with pytest.raises(wr.RegistryError, match="probe score"):
        reg.register(*pair(tmp_path), T, "ophelia", "ophel")


@pytest.mark.parametrize("kwargs", [
    dict(idle=0.9, playback=0.95, reference=0.3, near_miss=0.17),   # playback above idle
    dict(idle=0.9, playback=0.65, reference=0.1, near_miss=0.17),   # near-miss above reference
    dict(idle=1.0, playback=0.65, reference=0.3, near_miss=0.17),
])
def test_threshold_sets_must_be_ordered_probabilities(kwargs):
    with pytest.raises(wr.RegistryError):
        wr.Thresholds(**kwargs)


@pytest.mark.parametrize("phrase, core", [("Ophelia", "ophel"), ("ophelia", ""), ("oph elia", "ophel")])
def test_spoken_forms_are_lowercase_letters(tmp_path, reg, phrase, core):
    with pytest.raises(wr.RegistryError):
        reg.register(*pair(tmp_path), T, phrase, core)


def test_active_model_cannot_be_deleted_inactive_one_can(tmp_path, reg, active):
    a = reg.register(*pair(tmp_path, "a", b"graph-a"), T, "ophelia", "ophel")
    b = reg.register(*pair(tmp_path, "b", b"graph-b"), T, "ophelia", "ophel")
    active["wakeModel"] = a.graph_sha256
    with pytest.raises(wr.RegistryError, match="active"):
        reg.delete(a.graph_sha256)
    reg.delete(b.graph_sha256)
    assert [m.graph_sha256 for m in reg.list()] == [a.graph_sha256]
    assert not (reg.directory / b.graph_file).exists()
    # the shared sidecar is still referenced by `a`
    assert (reg.directory / a.sidecar_file).exists()


def test_same_graph_with_a_different_sidecar_is_refused(tmp_path, reg):
    reg.register(*pair(tmp_path, "a"), T, "ophelia", "ophel")
    g, s = pair(tmp_path, "b", sidecar={**SIDECAR, "labels": ["x", "ohphelia", "y"]})
    with pytest.raises(wr.RegistryError, match="different sidecar"):
        reg.register(g, s, T, "ophelia", "ophel")


def test_deployed_entry_is_pinned_with_its_thresholds_and_spoken_forms(tmp_path):
    pytest.importorskip("onnxruntime")
    reg = wr.WakeRegistry(tmp_path / "oww_models", active_getter=lambda: wr.DEPLOYED_GRAPH_SHA256)
    model = reg.install_deployed(ROOT / "bcresnet_audio.onnx", ROOT / "bcresnet_audio.json")
    assert model.graph_sha256 == wr.DEPLOYED_GRAPH_SHA256
    assert model.sidecar_sha256 == wr.DEPLOYED_SIDECAR_SHA256
    assert model.thresholds == wr.Thresholds(0.90, 0.65, 0.30, 0.17)
    assert (model.wake_phrase, model.verify_core) == ("ophelia", "ophel")
    assert all(0.0033 - 5e-5 <= v <= 0.0106 + 5e-5 for v in model.probe.values())
    assert reg.active() == model


def test_install_deployed_refuses_other_bytes(tmp_path, reg):
    g, s = pair(tmp_path)
    with pytest.raises(wr.RegistryError, match="deployed graph"):
        reg.install_deployed(g, s)
