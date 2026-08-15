"""
Tests for em_samples — wake-word sample collection.

Two halves, both pure: the segmenter (frames in, clips out) and the storage
(paths, retention, ownership). The properties that matter:

  * a clip carries the audio from BEFORE the level crossed — a wake word's
    onset is its quietest part, and a training set of clips missing their
    first phoneme teaches the model to want a word nobody says
  * the thresholds are relative to a tracked noise floor, so the same config
    works in a quiet study and a loud kitchen — and a room of digital
    silence (a muted device) does not read as continuous speech
  * one device's clip name can never reach another device's audio
"""

import re
from pathlib import Path

import em_samples as smp
import pytest

CONTROLLER_DIR = Path(__file__).resolve().parents[1]
CONTROLLER = CONTROLLER_DIR / "em_controller.py"
API        = CONTROLLER_DIR / "em_api.py"
DB         = CONTROLLER_DIR / "em_db.py"
SECTIONS   = CONTROLLER_DIR / "em_config_sections.py"
JSX        = CONTROLLER_DIR / "static" / "dashboard.jsx"


FRAME_MS = 80


def _frame(ms: int = FRAME_MS) -> bytes:
    """A frame of the wire format. The bytes are irrelevant — the segmenter
    is driven by the RMS the caller passes, not by the samples."""
    return b"\x00\x01" * int(smp.SAMPLE_RATE * ms / 1000)


def _rms(db: float) -> float:
    return 10.0 ** (db / 20.0)


QUIET = _rms(-66.0)     # a measured room floor on this fleet
SPEECH = _rms(-46.0)    # ~20dB above it, where speech sits after micGainDb


def _settle(seg: smp.Segmenter, frames: int = 60, level: float = QUIET) -> None:
    """Let the noise-floor tracker converge on a quiet room."""
    for _ in range(frames):
        seg.push(_frame(), level)


def _run(seg: smp.Segmenter, pattern) -> list[smp.Clip]:
    """Feed (level, frame_count) pairs; collect whatever comes out."""
    out = []
    for level, count in pattern:
        for _ in range(count):
            clip = seg.push(_frame(), level)
            if clip is not None:
                out.append(clip)
    return out


# ─── the floor ───────────────────────────────────────────────────────────────

def test_thresholds_follow_the_room_not_an_absolute_level():
    """The whole reason this is relative: a level that is speech in a study
    is below the floor in a kitchen, and neither device should need tuning."""
    quiet = smp.Segmenter()
    _settle(quiet, level=_rms(-70.0))
    loud = smp.Segmenter()
    _settle(loud, level=_rms(-50.0))
    assert loud.open_db > quiet.open_db + 15.0


def test_a_silent_room_cannot_make_everything_look_like_speech():
    """A muted device streams zero-filled frames. An unclamped floor would
    track true digital silence and put the open threshold at -168dB, where
    the first quantisation noise after an unmute is a shout."""
    seg = smp.Segmenter()
    _settle(seg, level=0.0)
    assert seg.floor_db == smp.DB_FLOOR
    assert seg.open_db == pytest.approx(
        smp.ABS_FLOOR_DB + seg.cfg.open_margin_db
    )


def test_the_floor_is_frozen_while_a_clip_is_open():
    """Otherwise a few seconds of speech drags the floor up to the speaker's
    own level and the clip closes on them mid-word."""
    seg = smp.Segmenter()
    _settle(seg)
    _run(seg, [(SPEECH, 1)])           # the frame that opens the clip
    before = seg.floor_db
    _run(seg, [(SPEECH, 20)])          # still open — no silence yet
    assert seg.floor_db == before
    # And the one frame that did move it (the crossing frame is measured
    # before the clip exists) moved it by a rise-alpha step, not a jump.
    assert before < -60.0


# ─── cutting ─────────────────────────────────────────────────────────────────

def test_speech_between_silences_becomes_one_clip():
    seg = smp.Segmenter()
    _settle(seg)
    clips = _run(seg, [(SPEECH, 12), (QUIET, 12)])
    assert len(clips) == 1
    assert not clips[0].truncated
    assert clips[0].speech_ms == pytest.approx(12 * FRAME_MS, abs=FRAME_MS)


def test_two_utterances_are_two_clips():
    seg = smp.Segmenter()
    _settle(seg)
    clips = _run(seg, [(SPEECH, 12), (QUIET, 12), (SPEECH, 12), (QUIET, 12)])
    assert len(clips) == 2


def test_a_clip_carries_the_audio_from_before_the_onset():
    """A wake word opens on its vowel, not its first consonant. Without the
    preroll every clip starts a syllable late."""
    seg = smp.Segmenter()
    _settle(seg)
    clips = _run(seg, [(SPEECH, 12), (QUIET, 12)])
    assert smp.duration_ms(len(clips[0].pcm)) > clips[0].speech_ms
    # ...and specifically about a preroll's worth of it, plus the tail.
    extra = smp.duration_ms(len(clips[0].pcm)) - clips[0].speech_ms
    assert extra >= seg.cfg.preroll_ms


def test_the_trailing_silence_is_trimmed_to_the_tail():
    """The clip must not carry the whole silence window that ended it —
    that is padding on every single sample in the training set."""
    seg = smp.Segmenter()
    _settle(seg)
    clips = _run(seg, [(SPEECH, 12), (QUIET, 60)])
    tail = (smp.duration_ms(len(clips[0].pcm))
            - clips[0].speech_ms - seg.cfg.preroll_ms)
    assert tail <= seg.cfg.tail_ms + FRAME_MS


def test_a_click_is_not_a_clip():
    """One frame over the threshold is a door, a chair or a cough."""
    seg = smp.Segmenter()
    _settle(seg)
    clips = _run(seg, [(SPEECH, 1), (QUIET, 12)])
    assert clips == []
    assert seg.stats.dropped_short == 1
    assert seg.stats.clips == 0


def test_min_clip_is_judged_on_speech_not_on_the_padded_file():
    """preroll + tail is ~560ms of padding — nearly twice min_clip_ms. If the
    length test looked at the file, every click would qualify."""
    seg = smp.Segmenter()
    _settle(seg)
    cfg = seg.cfg
    assert cfg.preroll_ms + cfg.tail_ms > cfg.min_clip_ms
    assert _run(seg, [(SPEECH, 2), (QUIET, 12)]) == []


def test_a_continuous_source_is_cut_at_the_cap_and_then_left_alone():
    """A television never goes quiet. It must not produce a back-to-back
    truncated clip forever — one, then nothing until the room is quiet."""
    seg = smp.Segmenter()
    _settle(seg)
    clips = _run(seg, [(SPEECH, 400)])     # 32s of unbroken level
    assert len(clips) == 1
    assert clips[0].truncated
    assert seg.stats.truncated == 1


def test_the_cap_cooldown_clears_when_the_room_goes_quiet():
    seg = smp.Segmenter()
    _settle(seg)
    _run(seg, [(SPEECH, 400)])
    clips = _run(seg, [(QUIET, 12), (SPEECH, 12), (QUIET, 12)])
    assert len(clips) == 1
    assert not clips[0].truncated


def test_hysteresis_stops_a_boundary_level_chattering():
    """One threshold would open and close a clip every 80ms for a frame
    sitting on it."""
    seg = smp.Segmenter()
    _settle(seg)
    assert seg.close_db < seg.open_db


def test_flush_keeps_a_clip_that_was_still_open():
    """Turning collection off mid-word is exactly when someone has just
    said the thing they turned it on to record."""
    seg = smp.Segmenter()
    _settle(seg)
    assert _run(seg, [(SPEECH, 12)]) == []
    clip = seg.flush()
    assert clip is not None and clip.speech_ms >= seg.cfg.min_clip_ms
    assert seg.flush() is None       # nothing left open


def test_frame_size_does_not_change_the_cut():
    """The controller feeds 80ms frames today; nothing here may depend on
    that, since the wake stream's framing is a wire detail."""
    out = []
    for ms in (20, 80, 160):
        seg = smp.Segmenter()
        for _ in range(int(60 * FRAME_MS / ms)):
            seg.push(_frame(ms), QUIET)
        clips = []
        for level, total_ms in ((SPEECH, 960), (QUIET, 960)):
            for _ in range(int(total_ms / ms)):
                c = seg.push(_frame(ms), level)
                if c:
                    clips.append(c)
        assert len(clips) == 1
        out.append(clips[0].speech_ms)
    assert max(out) - min(out) <= 160


def test_config_rejects_a_combination_that_can_never_emit():
    with pytest.raises(ValueError):
        smp.SegmentConfig(open_margin_db=6.0, close_margin_db=12.0)
    with pytest.raises(ValueError):
        smp.SegmentConfig(min_clip_ms=9000, max_clip_ms=6000)


# ─── storage ─────────────────────────────────────────────────────────────────

def _db(tmp_path):
    return str(tmp_path / "echomuse.db")


def _clip(ms: int = 800) -> smp.Clip:
    return smp.Clip(pcm=_frame(ms), speech_ms=ms, peak_db=-40.0, truncated=False)


def test_samples_dir_sits_beside_the_db(tmp_path):
    assert smp.samples_dir(_db(tmp_path)) == tmp_path / "samples"


def test_samples_dir_is_absolute_from_env(monkeypatch, tmp_path):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("DB_PATH", "echomuse.db")
    assert smp.samples_dir().is_absolute()


def test_save_then_resolve_roundtrips(tmp_path):
    name = smp.save_clip("G090LF11", _clip(), 1_700_000_000_000, _db(tmp_path))
    assert name is not None
    path = smp.resolve("G090LF11", name, _db(tmp_path))
    assert path is not None and path.is_file()


def test_saved_clips_are_playable_wavs(tmp_path):
    import wave
    name = smp.save_clip("G090LF11", _clip(800), 1_700_000_000_000, _db(tmp_path))
    with wave.open(str(smp.resolve("G090LF11", name, _db(tmp_path)))) as w:
        assert w.getframerate() == smp.SAMPLE_RATE
        assert w.getnchannels() == smp.CHANNELS
        assert w.getsampwidth() == smp.SAMPLE_WIDTH


def test_listing_reports_duration_and_is_newest_first(tmp_path):
    db = _db(tmp_path)
    smp.save_clip("dev", _clip(400), 1_700_000_000_000, db)
    smp.save_clip("dev", _clip(800), 1_700_000_002_000, db)
    listed = smp.list_for("dev", db)
    assert [c["ms"] for c in listed] == [800, 400]


def test_a_clip_never_silently_overwrites_another(tmp_path):
    """Same-millisecond collisions only happen when the clock steps back,
    which is exactly when losing the older file would be worst."""
    db = _db(tmp_path)
    a = smp.save_clip("dev", _clip(), 1_700_000_000_000, db)
    b = smp.save_clip("dev", _clip(), 1_700_000_000_000, db)
    assert a != b
    assert len(smp.list_for("dev", db)) == 2


def test_retention_is_a_hard_per_device_count(tmp_path):
    db = _db(tmp_path)
    for i in range(6):
        smp.save_clip("dev", _clip(), 1_700_000_000_000 + i * 1000, db, keep=3)
    listed = smp.list_for("dev", db)
    assert len(listed) == 3
    # The newest survive.
    assert listed[0]["ts"] == pytest.approx(1_700_000_005.0)


def test_one_device_cannot_reach_another_devices_clips(tmp_path):
    db = _db(tmp_path)
    name = smp.save_clip("alice", _clip(), 1_700_000_000_000, db)
    assert smp.resolve("bob", name, db) is None


def test_a_traversing_name_resolves_to_nothing(tmp_path):
    db = _db(tmp_path)
    smp.save_clip("dev", _clip(), 1_700_000_000_000, db)
    for bad in ("../dev/1700000000000.wav", "/etc/passwd", "1700000000000.wav.part"):
        assert smp.resolve("dev", bad, db) is None


def test_an_unnameable_device_writes_nothing(tmp_path):
    assert smp.save_clip("../../etc", _clip(), 1, _db(tmp_path)) is None
    assert smp.device_dir("has spaces") is None


def test_a_part_file_is_never_listed_or_served(tmp_path):
    db = _db(tmp_path)
    smp.save_clip("dev", _clip(), 1_700_000_000_000, db)
    (smp.device_dir("dev", db) / "1700000009999.wav.part").write_bytes(b"x")
    assert len(smp.list_for("dev", db)) == 1


def test_deleting_a_device_takes_its_speech_with_it(tmp_path):
    db = _db(tmp_path)
    for i in range(3):
        smp.save_clip("dev", _clip(), 1_700_000_000_000 + i * 1000, db)
    assert smp.delete_device("dev", db) == 3
    assert smp.list_for("dev", db) == []
    assert not smp.device_dir("dev", db).exists()


def test_usage_reports_what_the_volume_is_carrying(tmp_path):
    db = _db(tmp_path)
    smp.save_clip("dev", _clip(800), 1_700_000_000_000, db)
    smp.save_clip("dev", _clip(400), 1_700_000_001_000, db)
    u = smp.usage("dev", db)
    assert u["count"] == 2
    assert u["ms"] == 1200
    assert u["bytes"] > 0


# ─── wiring ──────────────────────────────────────────────────────────────────
#
# The suite cannot import em_controller (openwakeword, aiohttp, a device), so
# the properties that make collection SAFE are pinned against the source the
# way test_deploy.py and test_capabilities.py pin theirs. Each of these has a
# failure mode that looks like nothing at all: a device that quietly keeps
# talking to Home Assistant, or one that never comes back from the mode.


def test_collection_is_refused_at_the_single_turn_choke_point():
    """
    Wake word, dot button and HA's own start_conversation all meet at
    _run_voice_locked. Guarding each trigger instead would leave whichever
    one nobody remembered still streaming a room to Home Assistant.
    """
    src  = CONTROLLER.read_text()
    body = src.split("async def _run_voice_locked", 1)[1].split("\nasync def ", 1)[0]
    assert "device.collect_mode" in body, \
        "_run_voice_locked must refuse to start a turn while collecting"
    # Before the lock is taken and before anything is drained: refusing after
    # would still pause music and take the speaker.
    assert body.index("device.collect_mode") < body.index("voice_lock")


def test_the_wake_model_is_not_scored_while_collecting():
    """Not scoring IS the mode: a detection acted on would open a turn, and
    a detection merely logged would cost inference on every device left
    collecting for an afternoon."""
    src  = CONTROLLER.read_text()
    body = src.split("async def wake_word_listener", 1)[1].split("\nasync def ", 1)[0]
    assert body.index("_collect_frame") < body.index("model.predict"), \
        "the collect branch must take the frame before the model scores it"


def test_collection_survives_a_controller_restart():
    """Someone walks the house saying the wake word for twenty minutes. A
    restart in the middle must not leave the dashboard claiming to record
    while nothing is written."""
    assert "collect_mode" in DB.read_text(), \
        "the mode must be a persisted device column"
    src = CONTROLLER.read_text()
    assert "db.get_collect_mode" in src, \
        "handle_control must re-arm collection from the DB on connect"


def test_collection_is_not_a_config_key():
    """Config is section-scoped and fleet-inherited by default, so a key
    here would let one toggle in the fleet panel silence every Echo in the
    house at once. It is a per-device mode with its own endpoint."""
    assert "collect" not in SECTIONS.read_text().lower()


def test_an_open_clip_is_kept_when_the_device_goes_away():
    src = CONTROLLER.read_text()
    assert "collect_teardown" in src
    body = src.split("async def collect_teardown", 1)[1].split("\n# ", 1)[0]
    assert "flush()" in body, \
        "a disconnect must flush the open clip, not discard it"


def test_the_mode_is_admin_only_and_the_reads_are_not():
    """Toggling suspends the assistant for everyone in the house; listening
    to what was already collected does not."""
    src = API.read_text()
    for handler in ("_post_device_collect", "_delete_sample", "_delete_samples"):
        block = src.split(f"async def {handler}", 1)[0]
        assert block.rstrip().endswith("@auth.require_admin"), \
            f"{handler} must be admin-only"
    for handler in ("_get_samples", "_get_sample_audio", "_get_samples_zip"):
        block = src.split(f"async def {handler}", 1)[0]
        assert block.rstrip().endswith("@auth.require_auth"), \
            f"{handler} must be reachable by a signed-in viewer"


def test_every_sample_route_has_a_handler():
    src   = API.read_text()
    routes = re.findall(r'add_(?:get|post|delete)\("(/api/devices/\{id\}/(?:samples|collect)[^"]*)",\s*(\w+)', src)
    assert routes, "the sample routes must be registered"
    for path, handler in routes:
        assert f"async def {handler}" in src, f"{path} points at a missing handler"


def test_a_collecting_device_says_so_outside_its_own_tab():
    """A device that answers nothing is indistinguishable from a broken one
    from every other panel."""
    jsx = JSX.read_text()
    assert "collectMode" in jsx
    assert "COLLECTING" in jsx
    # One name for the fact, on both the REST object and the live event —
    # the dashboard merges the event straight onto the object, so two names
    # would disagree depending on which arrived last.
    assert '"collectMode"' in API.read_text()
    assert '"collectMode"' in CONTROLLER.read_text()


def test_deleting_a_device_reaches_the_filesystem():
    body = DB.read_text().split("def delete_device", 1)[1].split("\ndef ", 1)[0]
    assert "em_samples.delete_device" in body, \
        "nothing cascades from SQLite to the volume — the unlink must be explicit"
