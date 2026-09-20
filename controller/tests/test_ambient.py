"""
Tests for em_ambient — ambient recording.

The mode's whole promise is one sentence: hold the mic open, and when it is
switched off hand back a single file covering the time it was open. Almost
everything below is a way of failing that promise:

  * the file is written incrementally, so its header is a lie until it is
    closed — and a header that is still a lie when the file is served is a
    recording nothing will play
  * a controller killed mid-session leaves a `.part`. An hour of audio that
    needs a hex editor is the same as an hour of audio lost
  * an in-progress recording must never be listed, served or archived — half
    a WAV is not an artefact
  * one device's recording name can never reach another device's audio
"""

import wave
from pathlib import Path

import em_ambient as amb
import pytest

CONTROLLER_DIR = Path(__file__).resolve().parents[1]
CONTROLLER = CONTROLLER_DIR / "em_controller.py"
API        = CONTROLLER_DIR / "em_api.py"
DB         = CONTROLLER_DIR / "em_db.py"
SECTIONS   = CONTROLLER_DIR / "em_config_sections.py"
JSX        = CONTROLLER_DIR / "static" / "dashboard.jsx"

DEV = "abc123"

FRAME_MS = 80


def _frame(value: int = 1, ms: int = FRAME_MS) -> bytes:
    """One frame of the wire format. Non-zero, so a test that loses audio
    cannot pass by comparing silence with silence."""
    return (value % 256).to_bytes(2, "little") * int(amb.SAMPLE_RATE * ms / 1000)


def _db(tmp_path) -> str:
    return str(tmp_path / "echomuse.db")


def _record(tmp_path, frames: int, **kw) -> tuple[str, bytes]:
    """Open a recording, push `frames` frames, close it. Returns the final
    name and the audio that went in."""
    rec = amb.start(DEV, db_path=_db(tmp_path), **kw)
    pushed = b""
    for i in range(frames):
        f = _frame(i + 1)
        pushed += f
        if rec.push(f):
            rec.flush()
    return rec.close(), pushed


# ─── one session, one file ────────────────────────────────────────────────────

def test_a_session_becomes_one_playable_wav_of_everything_it_heard(tmp_path):
    """The contract. Not 'roughly the right length' — the exact bytes, in
    order: this file is training material, and audio that lost a 2s flush
    somewhere in the middle teaches a model about a room that never existed."""
    name, pushed = _record(tmp_path, 40)      # 3.2s, several flushes
    path = amb.resolve(DEV, name, db_path=_db(tmp_path))
    assert path is not None

    with wave.open(str(path), "rb") as w:
        assert w.getnchannels()   == amb.CHANNELS
        assert w.getsampwidth()   == amb.SAMPLE_WIDTH
        assert w.getframerate()   == amb.SAMPLE_RATE
        assert w.readframes(w.getnframes()) == pushed


def test_the_header_is_patched_to_the_real_length_on_close(tmp_path):
    """A streamed WAV cannot know its length when it is opened, so both size
    fields are written as zero and fixed at the end. Unfixed, every player
    reads a 44-byte file and the audio is invisible."""
    name, pushed = _record(tmp_path, 10)
    path = amb.resolve(DEV, name, db_path=_db(tmp_path))
    assert path.read_bytes()[:amb.HEADER_BYTES] == amb.wav_header(len(pushed))


def test_nothing_is_listed_or_served_while_the_recording_is_open(tmp_path):
    """Half a WAV has a zeroed header and no ending. The mode's artefact
    exists when it STOPS, and the API can only reach files that exist."""
    rec = amb.start(DEV, db_path=_db(tmp_path))
    rec.push(_frame())
    rec.flush()

    assert amb.list_for(DEV, db_path=_db(tmp_path)) == []
    assert amb.resolve(DEV, rec.part.name, db_path=_db(tmp_path)) is None
    assert amb.usage(DEV, db_path=_db(tmp_path))["count"] == 0

    name = rec.close()
    assert [i["name"] for i in amb.list_for(DEV, db_path=_db(tmp_path))] == [name]


def test_arming_and_disarming_with_no_audio_leaves_no_file(tmp_path):
    """A 44-byte WAV in the list is a recording that failed pretending to be
    one that worked."""
    rec = amb.start(DEV, db_path=_db(tmp_path))
    assert rec.close() is None
    assert amb.list_for(DEV, db_path=_db(tmp_path)) == []
    assert not rec.part.exists()


def test_a_flush_is_due_only_once_the_batch_is_full(tmp_path):
    """One write per 64kB rather than one per 80ms frame — the tap is on the
    busiest path in the controller, and `push` is the half that must not
    touch the disk at all."""
    rec = amb.start(DEV, db_path=_db(tmp_path), flush_bytes=4 * len(_frame()))
    assert [rec.push(_frame()) for _ in range(4)] == [False, False, False, True]
    # Still nothing but the placeholder header on disk: push buffers.
    assert rec.part.stat().st_size == amb.HEADER_BYTES
    rec.flush()
    assert rec.part.stat().st_size == amb.HEADER_BYTES + 4 * len(_frame())
    rec.close()


def test_duration_and_bytes_count_audio_that_is_still_in_hand(tmp_path):
    """The dashboard's elapsed clock reads these while the file is open. A
    measurement that only counted flushed audio would sit still for 2s at a
    time and read as a stalled recording."""
    rec = amb.start(DEV, db_path=_db(tmp_path))
    for _ in range(5):
        rec.push(_frame())
    assert rec.data_bytes == 5 * len(_frame())
    assert rec.duration_ms == pytest.approx(5 * FRAME_MS, abs=1)
    rec.close()


# ─── crash recovery ───────────────────────────────────────────────────────────

def test_a_part_left_by_a_killed_controller_is_recovered(tmp_path):
    """
    The reason the header is patched by arithmetic rather than by `wave`.
    Someone leaves a room recording overnight and the container is OOM-killed
    at 04:00: the audio is all there, and only the two size fields are wrong.
    Discarding it would be discarding the whole session.
    """
    directory = amb.device_dir(DEV, db_path=_db(tmp_path))
    directory.mkdir(parents=True)
    audio = _frame(7) * 25
    # Exactly what a killed writer leaves: placeholder header, real payload.
    (directory / "1700000000000.wav.part").write_bytes(amb.wav_header(0) + audio)

    assert amb.recover(DEV, db_path=_db(tmp_path)) == ["1700000000000.wav"]
    path = amb.resolve(DEV, "1700000000000.wav", db_path=_db(tmp_path))
    with wave.open(str(path), "rb") as w:
        assert w.readframes(w.getnframes()) == audio
    assert not list(directory.glob("*.part"))


def test_recovery_drops_a_part_that_never_got_any_audio(tmp_path):
    """A header and nothing else is not a recording, and promoting it would
    put an unplayable file in the list and evict a real one at the cap."""
    directory = amb.device_dir(DEV, db_path=_db(tmp_path))
    directory.mkdir(parents=True)
    (directory / "1700000000000.wav.part").write_bytes(amb.wav_header(0))

    assert amb.recover(DEV, db_path=_db(tmp_path)) == []
    assert list(directory.iterdir()) == []


def test_recovery_never_overwrites_an_existing_recording(tmp_path):
    """The recovered name is a timestamp, and a stepped clock can collide
    with a file already on disk. Losing half an hour of audio to make room
    for the file that was being recovered is the worst possible trade."""
    name, kept = _record(tmp_path, 5)
    directory = amb.device_dir(DEV, db_path=_db(tmp_path))
    (directory / f"{amb.parse_filename(name)}.wav.part").write_bytes(
        amb.wav_header(0) + _frame(9) * 5
    )

    recovered = amb.recover(DEV, db_path=_db(tmp_path))
    assert recovered and recovered[0] != name
    with wave.open(str(directory / name), "rb") as w:
        assert w.readframes(w.getnframes()) == kept


# ─── the length cap ───────────────────────────────────────────────────────────

def test_the_length_cap_is_reported_rather_than_enforced_silently(tmp_path):
    """
    `full` is how the controller knows to roll into a new file. The cap
    exists for disk, but a mode that stopped at it would end an unattended
    overnight capture at 00:30 and say so nowhere — so the recorder reports
    and the caller rolls.
    """
    rec = amb.start(DEV, db_path=_db(tmp_path), max_ms=3 * FRAME_MS)
    rec.push(_frame())
    rec.push(_frame())
    assert not rec.full
    rec.push(_frame())
    assert rec.full
    # Reporting only — the recorder is still writable, so the caller decides
    # when to roll and no frame is dropped between the two files.
    name, _ = rec.close(), None
    assert amb.resolve(DEV, name, db_path=_db(tmp_path)) is not None


def test_rolling_at_the_cap_keeps_both_files(tmp_path):
    """A rolled session is more than one file, and each one has to be a whole
    recording — the roll is visible in the list, not a gap in the audio."""
    first, _  = _record(tmp_path, 3, max_ms=2 * FRAME_MS)
    second, _ = _record(tmp_path, 3, max_ms=2 * FRAME_MS)
    names = [i["name"] for i in amb.list_for(DEV, db_path=_db(tmp_path))]
    assert first != second
    assert sorted(names) == sorted([first, second])


def test_a_name_collision_takes_the_next_millisecond(tmp_path):
    """Recordings are named by wall clock. Two in the same millisecond means
    the clock was stepped back onto an existing file, which is exactly when
    overwriting it would be worst."""
    when = 1700000000000
    rec = amb.start(DEV, when_ms=when, db_path=_db(tmp_path))
    rec.push(_frame())
    name = rec.close()
    rec2 = amb.start(DEV, when_ms=when, db_path=_db(tmp_path))
    rec2.push(_frame())
    name2 = rec2.close()
    assert name != name2
    assert len(amb.list_for(DEV, db_path=_db(tmp_path))) == 2


# ─── the store ────────────────────────────────────────────────────────────────

def test_retention_keeps_the_newest_and_drops_the_oldest(tmp_path):
    """These are the largest files the controller writes — 57.6MB each at the
    cap — so the count is small and the oldest go first."""
    names = [_record(tmp_path, 2)[0] for _ in range(5)]
    amb.prune(DEV, db_path=_db(tmp_path), keep=2)
    kept = [i["name"] for i in amb.list_for(DEV, db_path=_db(tmp_path))]
    assert kept == sorted(names[-2:], reverse=True)


def test_listing_reports_duration_from_the_file_size(tmp_path):
    """Every file here was written by this module, so the header it would
    read is derived from the same size — opening each one to read it back
    would make a directory listing a syscall storm for no new information."""
    name, pushed = _record(tmp_path, 12)
    item = amb.list_for(DEV, db_path=_db(tmp_path))[0]
    assert item["name"]  == name
    assert item["bytes"] == len(pushed) + amb.HEADER_BYTES
    assert item["ms"]    == pytest.approx(12 * FRAME_MS, abs=1)


@pytest.mark.parametrize("name", [
    "../../etc/passwd",
    "..%2Fother.wav",
    "1700000000000.wav.part",
    "1700000000000.WAV",
    "notatimestamp.wav",
])
def test_a_crafted_name_cannot_reach_anything(tmp_path, name):
    """Device id and filename both come from the URL, so the name is checked
    against the pattern AND resolved inside that device's own directory."""
    _record(tmp_path, 2)
    assert amb.resolve(DEV, name, db_path=_db(tmp_path)) is None


def test_one_device_cannot_read_another_devices_recordings(tmp_path):
    name, _ = _record(tmp_path, 2)
    assert amb.resolve("someone_else", name, db_path=_db(tmp_path)) is None


def test_a_device_id_that_is_not_a_path_component_is_refused(tmp_path):
    assert amb.device_dir("../escape", db_path=_db(tmp_path)) is None
    assert amb.start("../escape", db_path=_db(tmp_path)) is None


def test_deleting_a_device_takes_its_recordings_and_its_part_file(tmp_path):
    """Nothing cascades from SQLite to the filesystem, and a deleted device's
    room audio is exactly the leftover that matters."""
    _record(tmp_path, 2)
    rec = amb.start(DEV, db_path=_db(tmp_path))     # left open, a .part
    rec.push(_frame())
    rec.flush()
    directory = amb.device_dir(DEV, db_path=_db(tmp_path))

    assert amb.delete_device(DEV, db_path=_db(tmp_path)) == 1
    assert not directory.exists()


# ─── controller and API wiring (shape guards, as in test_samples.py) ──────────

def test_voice_turns_are_refused_while_recording_ambient():
    """A device holding its mic open for a recording must not also be
    answering with it. Guarded at _run_voice_locked, where wake word, dot
    button and HA's start_conversation all meet, and BEFORE the lock is taken
    — refusing after would still pause music and take the speaker."""
    src  = CONTROLLER.read_text()
    body = src.split("async def _run_voice_locked", 1)[1].split("\nasync def ", 1)[0]
    assert "device.ambient_mode" in body
    assert body.index("device.ambient_mode") < body.index("voice_lock")




def test_the_mode_survives_a_controller_restart():
    """Someone leaves a room recording for an hour. A restart in the middle
    must not leave the dashboard claiming to record while nothing is
    written — and the interrupted file must not be lost either."""
    assert "ambient_mode" in DB.read_text(), \
        "the mode must be a persisted device column"
    src = CONTROLLER.read_text()
    assert "db.get_ambient_mode" in src, \
        "handle_control must re-arm ambient recording from the DB on connect"
    assert "em_ambient.recover" in src, \
        "a .part from a killed controller must be recovered on connect"


def test_the_open_file_is_closed_when_the_device_goes_away():
    """The audio this connection delivered is a complete recording. Left as a
    `.part` for a device that never comes back, the session's only artefact
    is unplayable — so the connection teardown finishes the file, next to
    the one that flushes a half-cut sample clip."""
    src  = CONTROLLER.read_text()
    body = src[src.index("async def handle_control"):]
    assert "await ambient_teardown(device)" in body
    assert body.index("await ambient_teardown(device)") > \
           body.index("await collect_teardown(device)")


def test_ambient_is_not_a_config_key():
    """Config is section-scoped and fleet-inherited by default, so a key here
    would let one toggle in the fleet panel silence every Echo in the house.
    It is a per-device mode with its own endpoint."""
    assert "ambient" not in SECTIONS.read_text().lower()


def test_the_mutating_endpoints_are_admin_only():
    """The mode suspends the assistant on a device everyone else in the house
    uses, and the recordings are room audio."""
    src = API.read_text()
    for handler in ("_post_device_ambient", "_delete_ambient",
                    "_delete_ambient_all"):
        block = src.split(f"async def {handler}", 1)[0]
        assert block.rstrip().endswith("@auth.require_admin"), \
            f"{handler} must be admin-only"


def test_the_two_recording_modes_refuse_each_other():
    """Both want the same frames for opposite purposes: a segmenter running
    under an ambient session cuts the wake word out of the room noise, and
    the ambient file fills with the word being said for the segmenter."""
    src = API.read_text()
    ambient = src.split("async def _post_device_ambient", 1)[1].split("\nasync def ", 1)[0]
    collect = src.split("async def _post_device_collect", 1)[1].split("\nasync def ", 1)[0]
    assert 'row["collect_mode"]' in ambient
    assert 'row["ambient_mode"]' in collect


def test_there_is_no_archive_endpoint():
    """Deliberate, and the opposite call to samples.zip: there the archive IS
    the feature, here one recording is one artefact and zipping ~350MB in
    memory from a dashboard click is a way to take the controller down."""
    assert "ambient.zip" not in API.read_text()


def test_a_recording_device_says_so_outside_its_own_tab():
    """A device that answers nothing is indistinguishable from a broken one
    from every other panel."""
    jsx = JSX.read_text()
    assert "ambientMode" in jsx
    assert "RECORDING" in jsx


def test_deleting_a_device_purges_its_ambient_audio():
    assert "em_ambient.delete_device" in DB.read_text()
