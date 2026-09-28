import hashlib
import io
import wave

import numpy as np
import pytest

import em_sounds as snd


def _wav(seconds: float, *, hz: float = 440.0) -> bytes:
    n = round(seconds * snd.SAMPLE_RATE)
    t = np.arange(n, dtype=np.float64) / snd.SAMPLE_RATE
    pcm = (np.sin(2 * np.pi * hz * t) * 12_000).astype("<i2")
    out = io.BytesIO()
    with wave.open(out, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(snd.SAMPLE_RATE)
        w.writeframes(pcm.tobytes())
    return out.getvalue()


def _wav_samples(data: bytes) -> np.ndarray:
    with wave.open(io.BytesIO(data), "rb") as w:
        assert (w.getframerate(), w.getnchannels(), w.getsampwidth()) == (48_000, 1, 2)
        return np.frombuffer(w.readframes(w.getnframes()), dtype="<i2")


@pytest.mark.parametrize("value", ["../../etc/passwd", "sub/dir", ".hidden", "sp ace", "", None])
def test_id_rejects_traversal_and_junk(value):
    assert snd.safe_sound_id(value) is None


def test_id_accepts_catalog_names_and_trims_edges():
    assert snd.safe_sound_id("Alarm-2_final.v3") == "Alarm-2_final.v3"
    assert snd.safe_sound_id("  timer  ") == "timer"


def test_suffix_accepts_only_supported_audio_containers():
    assert snd.safe_upload_suffix("ring.WAV") == ".wav"
    assert snd.safe_upload_suffix("/tmp/upload/ring.flac") == ".flac"
    assert snd.safe_upload_suffix("ring.exe") is None
    assert snd.safe_upload_suffix("ring") is None


def test_sounds_dir_sits_beside_db(tmp_path):
    assert snd.sounds_dir(str(tmp_path / "echomuse.db")) == tmp_path / "sounds"
    assert snd.sounds_dir("echomuse.db").is_absolute()


def test_store_replaces_other_container_and_invalidates_export(tmp_path):
    snd.store("ring", ".mp3", b"old", tmp_path)
    (tmp_path / "ring.alert.json").write_text('{"stale":true}')
    snd.store("ring", ".wav", _wav(0.1), tmp_path)
    assert not (tmp_path / "ring.mp3").exists()
    assert snd.source_path("ring", tmp_path) == tmp_path / "ring.wav"
    assert not (tmp_path / "ring.alert.json").exists()


def test_delete_reports_miss_and_removes_catalog_entry(tmp_path):
    assert snd.delete("never-existed", tmp_path) is False
    snd.store("ring", ".wav", _wav(0.1), tmp_path)
    assert snd.delete("ring", tmp_path) is True
    assert snd.source_path("ring", tmp_path) is None


def test_render_alert_wav_cuts_at_ten_seconds_and_fades_to_zero():
    pcm = np.full(snd.ALERT_MAX_SAMPLES + 321, 10_000, dtype=np.int16)
    wav, shortened = snd.render_alert_wav(pcm)
    samples = _wav_samples(wav)
    assert shortened is True
    assert samples.size == snd.ALERT_MAX_SAMPLES
    # Before the 50 ms tail the level is unchanged; the tail ramps down.
    assert samples[-snd.FADE_OUT_SAMPLES - 1] == 10_000
    assert 9_990 <= samples[-snd.FADE_OUT_SAMPLES] <= 10_000
    assert 4_900 <= samples[-snd.FADE_OUT_SAMPLES // 2] <= 5_100
    assert samples[-1] == 0


def test_short_sound_is_not_flagged_but_still_has_click_free_tail():
    wav, shortened = snd.render_alert_wav(np.full(4_800, 1_000, dtype=np.int16))
    samples = _wav_samples(wav)
    assert shortened is False
    assert samples.size == 4_800
    assert samples[-1] == 0


def test_export_is_sha_named_48k_mono_pcm16_and_scan_warns_when_shortened(tmp_path):
    snd.store("long", ".wav", _wav(10.2), tmp_path)
    exported = snd.export("long", tmp_path)
    path = snd.asset_path(exported.sha256, tmp_path)
    assert path is not None
    assert path.name == f"{hashlib.sha256(path.read_bytes()).hexdigest()}.wav"
    samples = _wav_samples(path.read_bytes())
    assert samples.size == snd.ALERT_MAX_SAMPLES
    assert exported.seconds == 10.0
    assert exported.shortened is True
    assert samples[-1] == 0
    assert snd.scan(tmp_path) == [{
        "id": "long", "file": "long.wav", "size": (tmp_path / "long.wav").stat().st_size,
        "mtime": int((tmp_path / "long.wav").stat().st_mtime), "seconds": 10.0,
        "sha256": exported.sha256, "shortened": True,
    }]


def test_export_cache_is_reused_until_source_changes(tmp_path, monkeypatch):
    snd.store("ring", ".wav", _wav(0.1), tmp_path)
    first = snd.export("ring", tmp_path)
    monkeypatch.setattr(snd, "_decode", lambda _p: (_ for _ in ()).throw(AssertionError("decoded twice")))
    assert snd.export("ring", tmp_path) == first


def test_unresolved_named_sound_is_flagged_and_uses_device_fallback(tmp_path):
    assert snd.alert_asset("missing", tmp_path) == (snd.FALLBACK, True)
    assert snd.preview_asset("missing", tmp_path) == snd.FALLBACK


def test_empty_choice_uses_default_or_unflagged_fallback(tmp_path):
    assert snd.alert_asset("", tmp_path) == (snd.FALLBACK, False)
    snd.store(snd.DEFAULT_ID, ".wav", _wav(0.1), tmp_path)
    exported = snd.export(snd.DEFAULT_ID, tmp_path)
    assert snd.alert_asset(None, tmp_path) == (exported.sha256, False)


def test_deleting_catalog_sound_keeps_content_addressed_asset(tmp_path):
    snd.store("ring", ".wav", _wav(0.1), tmp_path)
    exported = snd.export("ring", tmp_path)
    path = snd.asset_path(exported.sha256, tmp_path)
    assert snd.delete("ring", tmp_path) is True
    assert path is not None and path.is_file(), "calendar events may still name this hash"


def test_scan_missing_dir_and_unexported_sound(tmp_path):
    assert snd.scan(tmp_path / "nope") == []
    snd.store("a", ".mp3", b"not decoded yet", tmp_path)
    row = snd.scan(tmp_path)[0]
    assert (row["sha256"], row["seconds"], row["shortened"]) == (None, None, None)


def test_in_use_by_checks_timer_and_alarm_settings():
    configs = {
        "global": {"timerSound": "chime"},
        "dev1": {"alarmSound": "chime"},
        "dev2": {"timerSound": "other", "alarmSound": "other"},
        "dev3": {},
    }
    assert sorted(snd.in_use_by("chime", configs)) == ["dev1", "global"]
