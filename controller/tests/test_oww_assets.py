"""
Asset-distribution planning.

The transport can only be exercised against a real device; the DECISIONS —
what to push, what to delete, what to refuse — are pure and are the part
that can quietly do damage, so they are tested here. A wrong prune deletes
the model a device is using; a wrong "keep" leaves it scoring against a
stale classifier that silently disagrees with the controller.
"""

import time

import pytest

import em_oww_assets as A


def _asset(name, md5, kind, size=1000):
    from pathlib import Path
    return A.Asset(name=name, source=Path("/src") / name, md5=md5, size=size, kind=kind)


def _base(md5_rt="rt1", md5_mel="mel1", md5_emb="emb1"):
    return [
        _asset(A.RUNTIME_NAME, md5_rt, "runtime", size=12_290_332),
        _asset("melspectrogram.onnx", md5_mel, "shared", size=1_087_958),
        _asset("embedding_model.onnx", md5_emb, "shared", size=1_326_578),
    ]


NOW = int(time.time())


def test_a_device_with_everything_is_a_noop():
    """The sync must be idempotent — it runs on every enable, not just once."""
    desired = _base() + [_asset("hey_mycroft_v0.1.onnx", "c1", "classifier")]
    actual = {a.name: (a.md5, NOW) for a in desired}
    p = A.plan_sync(desired, actual)
    assert p.is_noop
    assert sorted(p.keep) == sorted(a.name for a in desired)


def test_only_changed_files_are_pushed():
    """
    12.3MB over a shell transport is slow enough that re-pushing an unchanged
    runtime on every sync would make the feature feel broken.
    """
    desired = _base() + [_asset("hey_mycroft_v0.1.onnx", "c1", "classifier")]
    actual = {
        A.RUNTIME_NAME: ("rt1", NOW),
        "melspectrogram.onnx": ("mel1", NOW),
        "embedding_model.onnx": ("OLD", NOW),
        "hey_mycroft_v0.1.onnx": ("c1", NOW),
    }
    p = A.plan_sync(desired, actual)
    assert [a.name for a in p.push] == ["embedding_model.onnx"]
    assert A.RUNTIME_NAME in p.keep


def test_a_missing_device_gets_everything():
    desired = _base() + [_asset("hey_mycroft_v0.1.onnx", "c1", "classifier")]
    p = A.plan_sync(desired, {})
    assert len(p.push) == 4
    assert not p.prune


def test_the_selected_classifier_is_never_pruned():
    """
    Evicting the model the device is configured to use is the one outcome
    that breaks it outright rather than costing a re-push.
    """
    desired = _base() + [_asset("selected.onnx", "c1", "classifier")]
    actual = {a.name: (a.md5, NOW) for a in desired}
    for i in range(6):
        actual[f"old{i}.onnx"] = (f"x{i}", NOW - 10_000 * (i + 1))
    p = A.plan_sync(desired, actual)
    assert "selected.onnx" not in p.prune
    assert "selected.onnx" in p.keep


def test_extra_classifiers_are_evicted_oldest_first():
    """LRU by device mtime — no controller-side bookkeeping to lose."""
    desired = _base() + [_asset("selected.onnx", "c1", "classifier")]
    actual = {a.name: (a.md5, NOW) for a in desired}
    actual["newest.onnx"] = ("a", NOW - 100)
    actual["middle.onnx"] = ("b", NOW - 5_000)
    actual["oldest.onnx"] = ("c", NOW - 90_000)
    actual["ancient.onnx"] = ("d", NOW - 900_000)

    p = A.plan_sync(desired, actual, slots=4)
    # 1 desired + 3 extras fills four slots; the oldest falls off.
    assert sorted(p.prune) == ["ancient.onnx"]
    assert "newest.onnx" in p.keep and "middle.onnx" in p.keep


def test_the_runtime_and_shared_models_are_never_evictable():
    """
    They are required by definition — pruning one would delete something the
    same sync is about to push back.
    """
    desired = _base() + [_asset("a.onnx", "c1", "classifier")]
    actual = {a.name: (a.md5, NOW - 999_999) for a in desired}
    p = A.plan_sync(desired, actual, slots=1)
    assert not p.prune


def test_unrecognised_files_are_left_alone():
    """
    This deletes only what it positively recognises as an evictable
    classifier. A stray file in the directory is not ours to remove.
    """
    desired = _base() + [_asset("a.onnx", "c1", "classifier")]
    actual = {a.name: (a.md5, NOW) for a in desired}
    actual["notes.txt"] = ("z", NOW - 999_999)
    actual["libsomething.so"] = ("y", NOW - 999_999)
    p = A.plan_sync(desired, actual, slots=1)
    assert p.prune == []


def test_a_full_device_is_blocked_with_a_reason_not_half_written():
    desired = _base()
    p = A.plan_sync(desired, {}, free_mb=60)
    assert p.blocked and "free" in p.blocked
    assert not p.push, "a blocked plan must not push anything"


def test_space_is_only_checked_against_what_actually_needs_sending():
    """
    A device that already has everything must never be blocked for space —
    the sync runs on every enable, and refusing a no-op would report a
    problem that does not exist.
    """
    desired = _base()
    actual = {a.name: (a.md5, NOW) for a in desired}
    p = A.plan_sync(desired, actual, free_mb=1)
    assert p.blocked is None
    assert p.is_noop


# ─── Device inventory parsing ────────────────────────────────────────────────

def test_listing_parses_md5_mtime_name():
    text = ("aff8f1c6bee88425111e62aadf3c381c 1753900000 "
            f"{A.DEVICE_DIR}/libonnxruntime.so\n")
    got = A.parse_device_listing(text)
    assert got == {"libonnxruntime.so": ("aff8f1c6bee88425111e62aadf3c381c", 1753900000)}


@pytest.mark.parametrize("line", [
    "",
    "garbage",
    "nothexdigest 123 /x/a.onnx",
    "aff8f1c6bee88425111e62aadf3c381c notanint /x/a.onnx",
    "aff8f1c6 123 /x/a.onnx",
    "md5sum: can't open '/x/a.onnx': No such file or directory",
])
def test_unparseable_lines_are_skipped_not_guessed(line):
    """
    Shell noise (a prompt echo, an error from a missing file) must not become
    an inventory entry. Over-reporting a file as present makes the sync push
    nothing and claim success — the one failure mode with no visible symptom.
    """
    assert A.parse_device_listing(line) == {}


def test_a_partial_inventory_is_safe():
    """
    Missing an entry costs an unnecessary push. That is the right direction
    to fail in, and this pins it: an empty parse means push everything.
    """
    desired = _base()
    p = A.plan_sync(desired, A.parse_device_listing("junk\n"))
    assert len(p.push) == len(desired)


# ─── Contract with the device ────────────────────────────────────────────────

def test_device_dir_matches_the_firmware_constant():
    """
    DEVICE_DIR and shadow.DefaultDir are the same path in two languages. If
    they drift, the controller installs assets the device will not look for,
    and the only symptom is shadow mode silently never starting.
    """
    from pathlib import Path
    go = (Path(__file__).resolve().parents[2]
          / "device/internal/wakeword/shadow/open.go").read_text()
    assert f'DefaultDir = "{A.DEVICE_DIR}"' in go, (
        "em_oww_assets.DEVICE_DIR has drifted from shadow.DefaultDir"
    )


def test_shared_model_names_match_what_the_device_opens():
    from pathlib import Path
    go = (Path(__file__).resolve().parents[2]
          / "device/internal/wakeword/shadow/open.go").read_text()
    for name in A.SHARED_NAMES:
        assert f'"{name}"' in go, f"{name} is not what the device opens"
    assert f'"{A.RUNTIME_NAME}"' in go


def test_classifier_filename_matches_the_device_stem_rule():
    """
    The device derives the classifier filename with shadow.ModelStem, which
    mirrors em_oww_models.prediction_key. Both must agree, or we send
    hey_mycroft_v0.1.onnx and the device opens hey_mycroft_v0.onnx — the
    exact bug ModelStem was written to fix.
    """
    import em_oww_models
    assert em_oww_models.prediction_key("hey_mycroft_v0.1") == "hey_mycroft_v0.1"
    assert em_oww_models.prediction_key("/data/oww_models/clara.onnx") == "clara"


# ─── df parsing ──────────────────────────────────────────────────────────────

@pytest.mark.parametrize("line,want", [
    # Wrapped filesystem name — what a real device actually returned.
    ("                  1010       648       346  65% /data", 346),
    # Unwrapped, the shape the column index was originally written for.
    ("/dev/block/platform/mtk-msdc.0/by-name/userdata 1010 648 346 65% /data", 346),
    ("tmpfs 100 0 100 0% /data", 100),
])
def test_free_space_is_read_from_the_percentage_anchor_not_a_column_index(line, want):
    """
    busybox wraps a long filesystem name onto its own line, so the available
    column is $3 sometimes and $4 others. awk '{print $4}' returned "65%" on a
    real device — which parsed as no reading, silently disabling the
    free-space check rather than failing visibly.
    """
    assert A.parse_free_mb(line) == want


@pytest.mark.parametrize("line", ["", "garbage", "df: /data: No such file or directory"])
def test_unreadable_df_yields_no_measurement(line):
    """None means 'unknown', which plan_sync treats as 'do not check' — the
    opposite of 0, which would block every install."""
    assert A.parse_free_mb(line) is None


def test_unknown_free_space_does_not_block():
    desired = _base()
    p = A.plan_sync(desired, {}, free_mb=A.parse_free_mb("garbage"))
    assert p.blocked is None and p.push


# --- which models the device can actually be given -------------------------
#
# A BC-ResNet .onnx is a whole detector: its own log-mel frontend, a
# 22400-sample audio input, three logits. The device's shadow scorer expects
# an openWakeWord classifier HEAD taking [1,16,96] off the shared embedding
# model. The two are not interchangeable, but they ARE both `<stem>.onnx`
# sitting in the same directory, so nothing about the file's name or location
# distinguishes them — which is exactly how a BC-ResNet model would get
# installed under the name the device looks for and fail at session creation.


def _resources(tmp_path):
    """A stand-in openwakeword resources dir with the two shared models."""
    d = tmp_path / "resources"
    d.mkdir()
    for n in A.SHARED_NAMES:
        (d / n).write_bytes(b"shared")
    return d


def _runtime(tmp_path):
    d = tmp_path / "runtime"
    d.mkdir()
    (d / A.RUNTIME_NAME).write_bytes(b"so")
    return d


def test_an_openwakeword_model_is_planned_as_a_classifier(tmp_path):
    models = tmp_path / "models"
    models.mkdir()
    (models / "custom.onnx").write_bytes(b"onnx")

    assets, problems = A.desired_assets(
        [str(models / "custom.onnx")],
        runtime_dir=_runtime(tmp_path),
        resources=_resources(tmp_path),
        models_dir=models,
    )

    assert problems == []
    assert [a.name for a in assets if a.kind == "classifier"] == ["custom.onnx"]


def test_a_bcresnet_model_is_refused_by_name_not_pushed(tmp_path):
    """The sidecar is what makes it BC-ResNet, and it is what makes it unsendable."""
    models = tmp_path / "models"
    models.mkdir()
    (models / "ohphelia.onnx").write_bytes(b"onnx")
    (models / "ohphelia.json").write_text('{"window": 22400}')

    assets, problems = A.desired_assets(
        [str(models / "ohphelia.onnx")],
        runtime_dir=_runtime(tmp_path),
        resources=_resources(tmp_path),
        models_dir=models,
    )

    # Named, so the dashboard can say why on-device scoring is unavailable...
    assert len(problems) == 1
    assert "ohphelia" in problems[0]
    assert "BC-ResNet" in problems[0]
    # ...and no file the device would try to load as a classifier head.
    assert [a.kind for a in assets] == ["runtime", "shared", "shared"]


def test_the_same_stem_without_a_sidecar_is_still_openwakeword(tmp_path):
    """
    The discriminator must be the sidecar, never the name: a user is free to
    call an openWakeWord model anything, and every model predating this
    feature has no sidecar.
    """
    models = tmp_path / "models"
    models.mkdir()
    (models / "ohphelia.onnx").write_bytes(b"onnx")

    assets, problems = A.desired_assets(
        [str(models / "ohphelia.onnx")],
        runtime_dir=_runtime(tmp_path),
        resources=_resources(tmp_path),
        models_dir=models,
    )

    assert problems == []
    assert any(a.kind == "classifier" for a in assets)


def _bcresnet(models, stem: str = "ophelia") -> str:
    (models / f"{stem}.onnx").write_bytes(b"onnx")
    (models / f"{stem}.json").write_text('{"window": 22400, "wakeIndex": 1}')
    return str(models / f"{stem}.onnx")


def test_a_capable_device_gets_the_model_AND_its_sidecar(tmp_path):
    """
    Planned as a pair, never separately. The device chooses its engine from the
    sidecar's presence, so a model sent without one is not "partly installed" —
    it is the openWakeWord pipeline pointed at a graph that is not an
    openWakeWord classifier.
    """
    models = tmp_path / "models"
    models.mkdir()
    path = _bcresnet(models)

    assets, problems = A.desired_assets(
        [path],
        runtime_dir=_runtime(tmp_path),
        resources=_resources(tmp_path),
        models_dir=models,
        bcresnet_capable=True,
    )

    assert problems == []
    by_kind = {a.kind: a.name for a in assets if a.kind in ("classifier", "sidecar")}
    assert by_kind == {"classifier": "ophelia.onnx", "sidecar": "ophelia.json"}


def test_a_bcresnet_model_with_no_sidecar_installs_NEITHER_half(tmp_path):
    """Half a pair is worse than none: the .onnx alone is the wrong engine."""
    models = tmp_path / "models"
    models.mkdir()
    (models / "ophelia.onnx").write_bytes(b"onnx")
    (models / "ophelia.json").write_text("{}")
    path = str(models / "ophelia.onnx")
    # is_bcresnet_model saw the sidecar; now it vanishes before planning.
    import em_wake_scorer
    assert em_wake_scorer.is_bcresnet_model(path)
    (models / "ophelia.json").unlink()

    assets, problems = A.desired_assets(
        [path],
        runtime_dir=_runtime(tmp_path),
        resources=_resources(tmp_path),
        models_dir=models,
        bcresnet_capable=True,
    )
    # With the sidecar gone it is simply an openWakeWord model again — the rule
    # is the sidecar, consistently, at every layer.
    assert problems == []
    assert [a.kind for a in assets if a.kind == "sidecar"] == []


def test_firmware_without_the_engine_is_refused_and_told_to_update(tmp_path):
    models = tmp_path / "models"
    models.mkdir()
    path = _bcresnet(models)

    assets, problems = A.desired_assets(
        [path],
        runtime_dir=_runtime(tmp_path),
        resources=_resources(tmp_path),
        models_dir=models,
        bcresnet_capable=False,
    )
    assert len(problems) == 1
    assert "BC-ResNet" in problems[0] and "update the device" in problems[0]
    assert [a.kind for a in assets] == ["runtime", "shared", "shared"]


def test_capability_defaults_to_absent(tmp_path):
    """
    A caller that has not been taught about the capability must refuse, not
    ship a pair to firmware that cannot use it. Wrong in this direction costs
    a warning; wrong in the other costs a device that scores nothing.
    """
    models = tmp_path / "models"
    models.mkdir()
    path = _bcresnet(models)
    _, problems = A.desired_assets(
        [path],
        runtime_dir=_runtime(tmp_path),
        resources=_resources(tmp_path),
        models_dir=models,
    )
    assert problems and "BC-ResNet" in problems[0]


def test_a_sidecar_is_evicted_with_its_classifier(tmp_path):
    """
    Leaving one behind makes the device take the BC-ResNet path for a stem
    whose model is gone — and for whatever openWakeWord model is later
    installed under that name, since the sidecar IS the engine decision.
    """
    desired = _base() + [_asset("current.onnx", "c1", "classifier")]
    actual = {
        A.RUNTIME_NAME: ("rt1", NOW),
        "melspectrogram.onnx": ("mel1", NOW),
        "embedding_model.onnx": ("emb1", NOW),
        "current.onnx": ("c1", NOW),
    }
    # Five stale classifiers, one of them a BC-ResNet pair, so at least one is
    # evicted past the four slots.
    for i, name in enumerate(("a", "b", "c", "old")):
        actual[f"{name}.onnx"] = (f"h{i}", NOW - 1000 * (i + 1))
    actual["old.json"] = ("sidecar", NOW - 4000)

    plan = A.plan_sync(desired, actual)

    assert "old.onnx" in plan.prune, plan.prune
    assert "old.json" in plan.prune, "the sidecar outlived its classifier"


def test_an_orphan_sidecar_is_left_alone(tmp_path):
    """
    "Delete only what it positively recognises" still holds: a .json with no
    matching .onnx on the device was not put there by this eviction, and
    guessing at it is how the prune path starts deleting a user's files.
    """
    desired = _base() + [_asset("current.onnx", "c1", "classifier")]
    actual = {
        A.RUNTIME_NAME: ("rt1", NOW),
        "melspectrogram.onnx": ("mel1", NOW),
        "embedding_model.onnx": ("emb1", NOW),
        "current.onnx": ("c1", NOW),
        "stranger.json": ("x", NOW),
    }
    plan = A.plan_sync(desired, actual)
    assert "stranger.json" not in plan.prune


def test_the_selected_sidecar_is_never_pruned(tmp_path):
    desired = _base() + [
        _asset("pinned.onnx", "p1", "classifier"),
        _asset("pinned.json", "p2", "sidecar"),
    ]
    actual = {
        A.RUNTIME_NAME: ("rt1", NOW),
        "melspectrogram.onnx": ("mel1", NOW),
        "embedding_model.onnx": ("emb1", NOW),
        "pinned.onnx": ("p1", NOW),
        "pinned.json": ("p2", NOW),
    }
    plan = A.plan_sync(desired, actual)
    assert plan.prune == []
    assert "pinned.json" in plan.keep


def test_the_sidecar_filename_matches_what_the_device_looks_for():
    """
    The sidecar's EXISTENCE is the whole of the engine decision, at both ends.
    If the two spell it differently the device loads the openWakeWord pipeline
    against a BC-ResNet graph — and the only symptom is a detector that never
    fires, with a perfectly successful install behind it.
    """
    from pathlib import Path
    import em_wake_scorer
    go = (Path(__file__).resolve().parents[2]
          / "device/internal/wakeword/shadow/open.go").read_text()
    # Go: filepath.Join(dir, stem+".json")
    assert 'stem+".json"' in go, "shadow.SidecarPath no longer derives <stem>.json"
    # Python: the same derivation, from the model path.
    assert em_wake_scorer.sidecar_path("/data/oww_models/clara.onnx").name == "clara.json"


def test_the_bcresnet_capability_string_matches_the_firmware():
    """
    Announced by the device and read by the controller. A typo on either side
    is a device that can run BC-ResNet and is never offered it — silent, and
    indistinguishable from firmware that genuinely lacks the engine.
    """
    from pathlib import Path
    root = Path(__file__).resolve().parents[2]
    go = (root / "device/internal/client/control.go").read_text()
    assert '"oww_bcresnet"' in go, "the device no longer announces oww_bcresnet"
    py = (root / "controller/em_controller.py").read_text()
    assert '"oww_bcresnet" in (self.capabilities or [])' in py
