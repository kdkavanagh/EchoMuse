"""
Tests for em_wakeclips — the audio that crossed the wake threshold.

This store exists for one workflow: a device wakes when nobody spoke to it,
somebody listens to or downloads the clip that caused it, and that clip goes
into a BCResNet training corpus (~/git/bcresnet) as a negative. Everything
below defends a property of that workflow whose failure looks like nothing
at all until the corpus is already wrong:

  * the file is exactly what the model scored — 16kHz mono 16-bit, the same
    frame count that went in. A clip that decodes at the wrong rate trains
    the model on a pitch nobody says.
  * ordering and retention are by TURN ID, never by mtime, so pruning drops
    the oldest turn rather than the oldest timestamp
  * a name out of a URL cannot reach another device's audio, and a half
    written file is never listed, served or counted

Pure paths and bytes, so `tmp_path` is the whole fixture. The wiring that
makes collection safe is pinned against the source as in test_samples.py and
test_ambient.py.
"""

import re
import wave
from pathlib import Path

import em_wakeclips as wc
import pytest

CONTROLLER_DIR = Path(__file__).resolve().parents[1]
CONTROLLER = CONTROLLER_DIR / "em_controller.py"
API        = CONTROLLER_DIR / "em_api.py"
DB         = CONTROLLER_DIR / "em_db.py"

DEV = "G090LF11"

# 44 bytes of RIFF, as encode_wav writes it and as list_for subtracts it.
HEADER_BYTES = 44


def _db(tmp_path) -> str:
    return str(tmp_path / "echomuse.db")


def _pcm(frames: int = wc.CLIP_FRAMES) -> bytes:
    """`frames` chunks of the wire format. Non-zero, so a test that loses or
    reorders audio cannot pass by comparing silence with silence."""
    samples = int(wc.SAMPLE_RATE * wc.FRAME_MS / 1000)
    return bytes(bytearray((i % 251) + 1 for i in range(frames * samples * 2)))


def _body(src: str, header: str) -> str:
    """One top-level def's source, bounded by whatever follows it. The shape
    guards below are about what happens INSIDE a function, so a match that
    leaked in from the next one would pin nothing."""
    body = src.split(header, 1)[1]
    ends = [body.index(d) for d in ("\nasync def ", "\ndef ", "\nclass ") if d in body]
    return body[:min(ends)] if ends else body


# ─── the file ─────────────────────────────────────────────────────────────────

def test_a_saved_clip_is_found_again_by_name(tmp_path):
    name = wc.save(DEV, 41, _pcm(4), _db(tmp_path))
    assert name == "41.wav"
    path = wc.resolve(DEV, name, _db(tmp_path))
    assert path is not None and path.is_file()


def test_a_clip_decodes_as_the_audio_the_model_scored(tmp_path):
    """Training reads these directly, so the container has to agree with
    the wire format down to the frame count: a negative resampled or padded
    by a wrong header is a negative for a sound the device never heard."""
    pcm  = _pcm(6)
    name = wc.save(DEV, 7, pcm, _db(tmp_path))
    with wave.open(str(wc.resolve(DEV, name, _db(tmp_path))), "rb") as w:
        assert w.getframerate()  == wc.SAMPLE_RATE
        assert w.getnchannels()  == wc.CHANNELS
        assert w.getsampwidth()  == wc.SAMPLE_WIDTH
        assert w.getnframes()    == len(pcm) // wc.SAMPLE_WIDTH
        assert w.readframes(w.getnframes()) == pcm


def test_a_full_clip_reads_back_as_the_window_that_was_scored(tmp_path):
    """CLIP_FRAMES frames of 80ms is CLIP_MS. If the ring, the frame size and
    the advertised duration ever disagree, the listing lies about how much
    context each negative carries — and the clip length is the one thing a
    retraining run cannot infer from anything else."""
    wc.save(DEV, 3, _pcm(wc.CLIP_FRAMES), _db(tmp_path))
    assert wc.list_for(DEV, _db(tmp_path))[0]["ms"] == wc.CLIP_MS


# ─── what is refused ──────────────────────────────────────────────────────────

def test_an_empty_ring_writes_no_file(tmp_path):
    """The deque is empty until the feature has been on for a full window, so
    the first detection after enabling it arrives with nothing. A 44-byte WAV
    in the list is a clip that failed pretending to be one."""
    assert wc.save(DEV, 12, b"", _db(tmp_path)) is None
    assert wc.list_for(DEV, _db(tmp_path)) == []


def test_an_unnameable_device_writes_nothing(tmp_path):
    """The device id is ro.serialno as the firmware reported it — never a
    path component until it has been checked."""
    assert wc.save("../../etc", 12, _pcm(1), _db(tmp_path)) is None
    assert wc.device_dir("has spaces", _db(tmp_path)) is None
    assert not (tmp_path / wc.WAKES_SUBDIR).exists()


@pytest.mark.parametrize("turn_id", [0, -1])
def test_a_turn_that_was_never_inserted_writes_nothing(tmp_path, turn_id):
    """The filename IS the rowid, so a clip written before the insert
    succeeded would be an orphan no Activity row can ever point at."""
    assert wc.save(DEV, turn_id, _pcm(1), _db(tmp_path)) is None
    assert wc.list_for(DEV, _db(tmp_path)) == []


# ─── ordering and retention ───────────────────────────────────────────────────

def test_clips_are_ordered_by_turn_id_and_not_by_mtime(tmp_path):
    """Rowids are monotonic; file timestamps are not trustworthy. A restore
    from a backup flattens every mtime to the moment of the copy, and a clip
    rewritten out of order (as here, turn 9 saved after turn 10) would then
    sort as the newest — so mtime ordering prunes the wrong clip and the
    dashboard's newest-first list disagrees with the Activity log."""
    db = _db(tmp_path)
    wc.save(DEV, 10, _pcm(2), db)
    wc.save(DEV, 9,  _pcm(2), db)
    assert [c["turn_id"] for c in wc.list_for(DEV, db)] == [10, 9]


def test_retention_keeps_the_newest_turns_and_drops_the_rest(tmp_path):
    """These accumulate unattended — a device with a badly tuned threshold
    writes one per false wake, all day, with nobody watching. The cap has to
    hold on the write path, not on a sweep somebody remembers to run."""
    db = _db(tmp_path)
    for turn in range(1, 6):
        wc.save(DEV, turn, _pcm(1), db, keep=3)
    assert [c["turn_id"] for c in wc.list_for(DEV, db)] == [5, 4, 3]


def test_deleting_every_clip_leaves_the_device_listable(tmp_path):
    """The purge is offered per device so a corpus can be thrown away and
    started again; it must empty the directory without breaking the next
    write into it."""
    db = _db(tmp_path)
    for turn in (1, 2, 3):
        wc.save(DEV, turn, _pcm(1), db)
    assert wc.delete_all(DEV, db) == 3
    assert wc.list_for(DEV, db) == []
    assert wc.save(DEV, 4, _pcm(1), db) == "4.wav"


def test_deleting_a_device_takes_its_directory_too(tmp_path):
    """Nothing cascades from SQLite to the volume, so a removed device that
    left its directory behind leaves the fleet's audio footprint growing by
    one empty folder per re-provision — and its speech on disk until then."""
    db = _db(tmp_path)
    wc.save(DEV, 1, _pcm(1), db)
    assert wc.delete_device(DEV, db) == 1
    assert not wc.device_dir(DEV, db).exists()


# ─── ownership ────────────────────────────────────────────────────────────────

def test_one_device_cannot_reach_another_devices_clips(tmp_path):
    """Device id and clip name both arrive in the URL. Turn ids are global,
    so `10.wav` exists under whichever device actually woke — asking for it
    under another must not serve it."""
    db = _db(tmp_path)
    name = wc.save("alice", 10, _pcm(1), db)
    assert wc.resolve("bob", name, db) is None


@pytest.mark.parametrize("name", [
    "../../etc/passwd",
    "../alice/10.wav",
    "foo.wav",
    "12.wav.part",
    "10.WAV",
    "",
])
def test_a_name_that_is_not_ours_resolves_to_nothing(tmp_path, name):
    db = _db(tmp_path)
    wc.save(DEV, 10, _pcm(1), db)
    assert wc.resolve(DEV, name, db) is None


def test_a_turn_with_no_clip_resolves_to_nothing(tmp_path):
    """Retention is shorter than the turn history, so a non-NULL wake_file on
    an old row is a claim to check rather than a promise — the reader has to
    get None here instead of a path to a missing file."""
    db = _db(tmp_path)
    wc.save(DEV, 10, _pcm(1), db)
    assert wc.resolve(DEV, "999.wav", db) is None


# ─── duration from the file size ──────────────────────────────────────────────

def test_duration_comes_from_the_file_size(tmp_path):
    """Listing hundreds of clips must not mean opening hundreds of WAVs to
    read a header this module wrote. The header's 44 bytes come off first, or
    every clip reads ~1ms long."""
    db   = _db(tmp_path)
    pcm  = _pcm(12)
    wc.save(DEV, 10, pcm, db)
    item = wc.list_for(DEV, db)[0]
    assert item["bytes"] == len(pcm) + HEADER_BYTES
    assert item["ms"]    == pytest.approx(12 * wc.FRAME_MS, abs=1)


def test_a_truncated_file_reads_as_zero_rather_than_as_nonsense(tmp_path):
    """A clip cut short by a full volume is still listed, so the dashboard
    can show that it is there. A negative duration would render as garbage
    and a negative total would make the usage figure meaningless."""
    db = _db(tmp_path)
    directory = wc.device_dir(DEV, db)
    directory.mkdir(parents=True)
    (directory / "10.wav").write_bytes(b"RIFF")
    assert wc.list_for(DEV, db)[0]["ms"] == 0
    assert wc.usage(DEV, db)["ms"] == 0


# ─── the write is atomic ──────────────────────────────────────────────────────

def test_a_successful_save_leaves_no_part_file(tmp_path):
    """Write-then-rename, so the API can never serve half a WAV. A `.part`
    surviving the save means the rename did not happen and the clip the
    Activity row points at is the temporary."""
    db = _db(tmp_path)
    wc.save(DEV, 10, _pcm(2), db)
    assert list(wc.device_dir(DEV, db).glob("*.part")) == []


def test_a_stray_part_is_never_listed_served_or_counted(tmp_path):
    """A killed controller leaves one behind. It is not a playable clip, so
    it must not appear in the list, must not resolve, and must not inflate
    the usage figures the retention decision is read against."""
    db = _db(tmp_path)
    wc.save(DEV, 10, _pcm(2), db)
    (wc.device_dir(DEV, db) / "11.wav.part").write_bytes(b"\x00" * 4096)
    listed = wc.list_for(DEV, db)
    assert [c["name"] for c in listed] == ["10.wav"]
    assert wc.resolve(DEV, "11.wav.part", db) is None
    assert wc.usage(DEV, db) == {
        "count": 1,
        "bytes": listed[0]["bytes"],
        "ms":    listed[0]["ms"],
    }


# ─── wiring (shape guards, as in test_samples.py) ─────────────────────────────
#
# The properties that make this feature safe rather than merely present are
# pinned against the source. Each one fails silently: a clip written for a
# device nobody opted in, or a clip filed against the wrong turn.












def test_deleting_a_device_reaches_the_volume():
    body = _body(DB.read_text(), "def delete_device")
    assert "em_wakeclips.delete_device" in body, \
        "nothing cascades from SQLite to the volume — the unlink must be explicit"


def test_every_wake_clip_route_has_a_handler():
    src    = API.read_text()
    routes = dict(re.findall(
        r'add_(?:get|delete)\("(/api/devices/\{id\}/(?:wakeclips[^"]*|turns/\{turn\}/wake))",\s*(\w+)',
        src))
    assert "/api/devices/{id}/turns/{turn}/wake" in routes, \
        "the per-turn clip must be servable — it is the whole download path"
    assert "/api/devices/{id}/wakeclips.zip" in routes, \
        "a corpus is downloaded in bulk, not one clip at a time"
    for path, handler in routes.items():
        assert f"async def {handler}" in src, f"{path} points at a missing handler"
