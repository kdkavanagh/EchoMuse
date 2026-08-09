import em_sounds as snd


# ─── safe_sound_id ───────────────────────────────────────────────────────────
# Ids become filename stems AND go in URLs, so anything that escapes either
# has to be refused rather than rewritten: a silently renamed id would no
# longer match the one stored in timerSound, and the ring would fall back to
# the built-in chime with nothing to explain why.

def test_id_accepts_simple_names():
    assert snd.safe_sound_id("timer") == "timer"
    assert snd.safe_sound_id("Alarm-2_final.v3") == "Alarm-2_final.v3"


def test_id_strips_surrounding_whitespace():
    assert snd.safe_sound_id("  timer  ") == "timer"


def test_id_rejects_traversal_and_junk():
    assert snd.safe_sound_id("../../etc/passwd") is None
    assert snd.safe_sound_id("sub/dir") is None
    assert snd.safe_sound_id(".hidden") is None
    assert snd.safe_sound_id("sp ace") is None
    assert snd.safe_sound_id("") is None
    assert snd.safe_sound_id(None) is None


# ─── safe_upload_suffix ──────────────────────────────────────────────────────

def test_suffix_accepts_known_audio_containers():
    assert snd.safe_upload_suffix("ring.mp3") == ".mp3"
    assert snd.safe_upload_suffix("ring.WAV") == ".wav"
    assert snd.safe_upload_suffix("/tmp/upload/ring.flac") == ".flac"


def test_suffix_rejects_everything_else():
    assert snd.safe_upload_suffix("ring.exe") is None
    assert snd.safe_upload_suffix("ring") is None
    assert snd.safe_upload_suffix("") is None


# ─── sounds_dir ──────────────────────────────────────────────────────────────

def test_sounds_dir_sits_beside_db(tmp_path):
    assert snd.sounds_dir(str(tmp_path / "echomuse.db")) == tmp_path / "sounds"


def test_sounds_dir_is_absolute_regardless_of_cwd():
    # The stored timerSound is only an id, but the resolved path has to be
    # stable: the controller's cwd is not guaranteed to be /app.
    assert snd.sounds_dir("echomuse.db").is_absolute()


# ─── source_path / store / delete ────────────────────────────────────────────

def test_store_then_resolve_roundtrip(tmp_path):
    snd.store("timer", ".mp3", b"ID3fake", tmp_path)
    assert snd.source_path("timer", tmp_path) == tmp_path / "timer.mp3"


def test_store_replaces_a_different_container_for_the_same_id(tmp_path):
    """
    Uploading ring.wav over an existing ring.mp3 must leave exactly one
    file. Leaving both would make source_path resolve by ALLOWED_SUFFIXES
    order — i.e. keep returning the old upload — and the user's new sound
    would silently never play.
    """
    snd.store("ring", ".mp3", b"old", tmp_path)
    snd.store("ring", ".wav", b"new", tmp_path)
    assert not (tmp_path / "ring.mp3").exists()
    assert snd.source_path("ring", tmp_path).read_bytes() == b"new"


def test_store_clears_the_stale_pcm_cache(tmp_path):
    """
    A cache left behind by the previous upload would be served for the new
    one — the id is the same, so nothing else would notice.
    """
    snd.store("ring", ".mp3", b"old", tmp_path)
    cache = snd.cache_path(tmp_path / "ring.mp3")
    cache.write_bytes(b"\x00\x00" * 100)
    snd.store("ring", ".wav", b"new", tmp_path)
    assert not cache.exists()


def test_source_path_is_none_for_unknown_and_unsafe_ids(tmp_path):
    assert snd.source_path("nope", tmp_path) is None
    assert snd.source_path("../escape", tmp_path) is None


def test_delete_removes_source_and_cache(tmp_path):
    snd.store("ring", ".mp3", b"data", tmp_path)
    cache = snd.cache_path(tmp_path / "ring.mp3")
    cache.write_bytes(b"pcm")
    assert snd.delete("ring", tmp_path) is True
    assert snd.source_path("ring", tmp_path) is None
    assert not cache.exists()


def test_delete_reports_a_miss(tmp_path):
    assert snd.delete("never-existed", tmp_path) is False


# ─── cache freshness ─────────────────────────────────────────────────────────

def test_cache_is_stale_without_a_stamp(tmp_path):
    src = tmp_path / "ring.mp3"
    src.write_bytes(b"data")
    snd.cache_path(src).write_bytes(b"pcm")
    assert snd._cache_is_fresh(src, snd.cache_path(src)) is False


def test_cache_is_fresh_once_stamped(tmp_path):
    src = tmp_path / "ring.mp3"
    src.write_bytes(b"data")
    cache = snd.cache_path(src)
    cache.write_bytes(b"pcm")
    snd._write_cache_stamp(src, cache)
    assert snd._cache_is_fresh(src, cache) is True


def test_cache_goes_stale_when_the_source_changes(tmp_path):
    src = tmp_path / "ring.mp3"
    src.write_bytes(b"data")
    cache = snd.cache_path(src)
    cache.write_bytes(b"pcm")
    snd._write_cache_stamp(src, cache)
    src.write_bytes(b"different length data")
    assert snd._cache_is_fresh(src, cache) is False


# ─── scan ────────────────────────────────────────────────────────────────────

def test_scan_missing_dir_is_empty_not_an_error(tmp_path):
    assert snd.scan(tmp_path / "nope") == []


def test_scan_lists_only_audio_and_ignores_the_pcm_cache(tmp_path):
    snd.store("a", ".mp3", b"x", tmp_path)
    snd.store("b", ".wav", b"y", tmp_path)
    snd.cache_path(tmp_path / "a.mp3").write_bytes(b"pcm")
    (tmp_path / "notes.txt").write_bytes(b"z")
    assert [s["id"] for s in snd.scan(tmp_path)] == ["a", "b"]


# ─── duration / built-in fallback ────────────────────────────────────────────

def test_duration_matches_the_device_wire_format():
    one_second = snd.SAMPLE_RATE * snd.CHANNELS * snd.SAMPLE_BYTES
    assert snd.duration_seconds(one_second) == 1.0


def test_builtin_chime_is_real_audio():
    """
    The end of every fallback chain. A timer that fires silently is the one
    outcome this feature must never produce, so this path has to work with
    no files, no config and no uploads.
    """
    pcm = snd.builtin_pcm()
    assert len(pcm) > 0
    assert len(pcm) % (snd.CHANNELS * snd.SAMPLE_BYTES) == 0
    assert 0.1 < snd.duration_seconds(len(pcm)) < 5.0


def test_builtin_chime_does_not_start_or_end_on_a_click(tmp_path):
    """
    A bare sine switched on and off pops, and on this speaker the pop is
    louder than the tone — hence the raised-cosine edges.
    """
    import numpy as np
    samples = np.frombuffer(snd.builtin_pcm(), dtype=np.int16)
    assert abs(int(samples[0])) < 100
    assert abs(int(samples[-1])) < 100


# ─── in_use_by ───────────────────────────────────────────────────────────────

def test_in_use_by_finds_every_referencing_scope():
    configs = {
        "global": {"timerSound": "chime"},
        "dev1":   {"timerSound": "chime"},
        "dev2":   {"timerSound": "other"},
        "dev3":   {"timerSound": ""},
        "dev4":   {},
    }
    assert sorted(snd.in_use_by("chime", configs)) == ["dev1", "global"]


def test_in_use_by_is_empty_when_nothing_selects_it():
    assert snd.in_use_by("chime", {"global": {"timerSound": ""}}) == []
