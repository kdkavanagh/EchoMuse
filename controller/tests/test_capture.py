"""
Tests for em_capture — script-driven recording windows.

The properties that matter, and why:

  * ONE window in, ONE recording out. The whole reason this exists beside
    em_samples is that a segmenter cannot promise that, so a caller pairing
    each recording with the file it played would be pairing them wrongly.
  * A window that is never stopped still ends. Two independent ways: the cap
    on audio time, and the wall clock — because a device that stops sending
    frames stops advancing audio time altogether.
  * The mode cannot strand a device. The dead-man expires an undriven mode,
    and never expires underneath an open window.
  * A caller's input cannot reach anything sharp: window lengths clamp
    rather than raise, tags are bounded and header-safe, and a webhook that
    is not an http(s) URL with a host is refused before the mode is armed.

Plus a pair of guards on the controller side, in the shape test_samples.py
uses: the voice-turn refusal has to cover capture mode, and the frame tap has
to sit where the wake stream is scored — those are the two places where
getting it wrong is silent.
"""

from pathlib import Path

import pytest

import em_capture as cap

CONTROLLER_DIR = Path(__file__).resolve().parents[1]
CONTROLLER = CONTROLLER_DIR / "em_controller.py"
API        = CONTROLLER_DIR / "em_api.py"
JSX        = CONTROLLER_DIR / "static" / "dashboard.jsx"

FRAME_MS = 80


def _frame(ms: int = FRAME_MS) -> bytes:
    """A frame of the wire format. The bytes are irrelevant — the window is
    driven by the RMS the caller passes, exactly as em_samples is."""
    return b"\x00\x01" * int(16000 * ms / 1000)


def _rms(db: float) -> float:
    return 10.0 ** (db / 20.0)


QUIET  = _rms(-66.0)
SPEECH = _rms(-30.0)


# ─── the window ───────────────────────────────────────────────────────────────

def test_one_window_yields_one_recording_whatever_the_levels_do():
    """
    The property em_samples cannot offer.

    This pattern — speech, a pause longer than the segmenter's silence_ms,
    more speech — is an ordinary sentence, and it is exactly what would come
    back as two clips from a segmenter. Here it is one recording.
    """
    w = cap.Window(tag="t", max_ms=10_000)
    for level, count in ((SPEECH, 10), (QUIET, 20), (SPEECH, 10)):
        for _ in range(count):
            assert w.push(_frame(), level) is False
    result = w.close()
    assert result.ms == 40 * FRAME_MS
    assert result.frames == 40
    assert not result.truncated


def test_push_reports_full_at_the_cap_and_the_result_says_truncated():
    w = cap.Window(max_ms=400)
    full = [w.push(_frame(), SPEECH) for _ in range(5)]
    # 80ms frames, 400ms cap: the fifth frame fills it, not the fourth.
    assert full == [False, False, False, False, True]
    assert w.close().truncated is True


def test_a_window_the_caller_stops_early_is_not_truncated():
    """`truncated` distinguishes a recording that hit the cap from one the
    caller ended — the caller uses it to know its playback overran."""
    w = cap.Window(max_ms=10_000)
    for _ in range(3):
        w.push(_frame(), SPEECH)
    assert w.close().truncated is False


def test_close_defaults_truncated_from_the_cap_so_both_closers_agree():
    """
    The frame path notices a full window; the watchdog notices an overdue
    one. Neither should have to work out the flag — a window that reached its
    cap is truncated however it was closed.
    """
    w = cap.Window(max_ms=160)
    while not w.push(_frame(), SPEECH):
        pass
    assert w.close().truncated is True          # closed with no argument


def test_peak_is_the_loudest_frame_and_floor_comes_from_the_head():
    """
    The pair that lets a caller notice it has recorded silence.

    The floor is measured from the START of the window — the room before the
    playback — because a tracker would follow the playback itself, which is
    the thing being measured.
    """
    w = cap.Window(max_ms=10_000)
    for _ in range(cap.Window.FLOOR_FRAMES):
        w.push(_frame(), QUIET)
    for _ in range(10):
        w.push(_frame(), SPEECH)
    r = w.close()
    assert r.peak_db == pytest.approx(-30.0, abs=0.5)
    assert r.floor_db == pytest.approx(-66.0, abs=0.5)


def test_an_empty_window_reports_a_floor_rather_than_minus_infinity():
    """Silence must read as a number. Same reason em_samples.DB_FLOOR exists."""
    r = cap.Window().close()
    assert r.pcm == b""
    assert r.floor_db == pytest.approx(-100.0)
    assert r.peak_db == pytest.approx(-100.0)


def test_the_result_encodes_a_playable_wav():
    w = cap.Window(max_ms=10_000)
    for _ in range(5):
        w.push(_frame(), SPEECH)
    import io, wave
    with wave.open(io.BytesIO(w.close().wav()), "rb") as f:
        assert f.getnchannels() == 1
        assert f.getsampwidth() == 2
        assert f.getframerate() == 16000
        assert f.getnframes() == 5 * 1280


def test_empty_frames_are_ignored_rather_than_counted():
    w = cap.Window(max_ms=10_000)
    assert w.push(b"", SPEECH) is False
    assert w.frames == 0


# ─── the wall clock ───────────────────────────────────────────────────────────

def test_a_window_whose_frames_stop_arriving_still_goes_overdue():
    """
    The failure this exists for: a link stall means no frames, which means
    audio time stops advancing, which means the cap can never be reached. The
    caller would wait for a delivery that never comes.
    """
    w = cap.Window(max_ms=1000, opened_mono=100.0)
    w.push(_frame(), SPEECH)                    # one frame, then nothing
    assert w.overdue(now_mono=100.5) is False
    assert w.overdue(now_mono=101.0) is False   # inside the grace
    assert w.overdue(now_mono=100.0 + 1.0 + cap.WINDOW_GRACE_MS / 1000.0) is True


def test_window_overdue_tolerates_no_window():
    """The watchdog's shape: it runs whether or not one is open."""
    assert cap.window_overdue(None, 1.0) is False


# ─── the dead-man ─────────────────────────────────────────────────────────────

def test_an_undriven_mode_expires():
    assert cap.decide_idle_expiry(400.0, 100.0, 300.0, window_open=False) is True
    assert cap.decide_idle_expiry(399.0, 100.0, 300.0, window_open=False) is False


def test_an_open_window_is_never_idle():
    """
    A caller may legitimately open a 30s window and say nothing for its
    duration. Expiring underneath it would discard the recording it is
    waiting for — and clear the mode a script is actively using.
    """
    assert cap.decide_idle_expiry(1e6, 0.0, 300.0, window_open=True) is False


# ─── caller input cannot reach anything sharp ─────────────────────────────────

@pytest.mark.parametrize("given,expected", [
    (None,                    cap.DEFAULT_WINDOW_MS),
    ("nonsense",              cap.DEFAULT_WINDOW_MS),
    (0,                       cap.DEFAULT_WINDOW_MS),
    (-5,                      cap.DEFAULT_WINDOW_MS),
    (5_000,                   5_000),
    (cap.MAX_WINDOW_MS * 10,  cap.MAX_WINDOW_MS),
])
def test_max_ms_clamps_rather_than_raising(given, expected):
    """
    Clamp, never raise — the same rule em_endpoint.config_for follows. A
    refusal here costs the caller the recording, which is the broken
    behaviour it was trying to avoid.
    """
    assert cap.clamp_max_ms(given) == expected


@pytest.mark.parametrize("given,expected", [
    (None,      cap.DEFAULT_IDLE_S),
    ("",        cap.DEFAULT_IDLE_S),
    (0,         cap.DEFAULT_IDLE_S),   # never disarm the dead-man
    (-1,        cap.DEFAULT_IDLE_S),
    (60,        60.0),
    (1e9,       cap.MAX_IDLE_S),
])
def test_idle_clamps_and_can_never_be_switched_off(given, expected):
    assert cap.clamp_idle_s(given) == expected


def test_a_tag_is_bounded_and_can_ride_a_header():
    """
    Tags are opaque and echoed into X-EM-Tag. Anything that would break the
    header — newlines above all — is dropped rather than escaped.
    """
    assert cap.clean_tag("clip|media_player.lounge|35") == "clip|media_player.lounge|35"
    assert "\n" not in cap.clean_tag("a\nb: injected")
    assert cap.clean_tag("é" * 10) == ""
    assert len(cap.clean_tag("x" * 1000)) == cap.MAX_TAG_LEN
    assert cap.clean_tag(None) == ""


@pytest.mark.parametrize("url,ok", [
    ("http://192.168.2.10:8791/clip",  True),
    ("https://host.local/clip",        True),
    ("file:///etc/passwd",             False),
    ("ftp://host/x",                   False),
    ("http:///no-host",                False),
    ("",                               False),
    (None,                             False),
    (12345,                            False),
])
def test_webhook_scheme_and_host_are_checked_before_the_mode_is_armed(url, ok):
    """
    Not an authorisation check — the caller is an admin. It is the cheap half
    of not building an SSRF gadget out of a controller sitting on a home LAN;
    the other half is refusing redirects at the request, in em_controller.
    """
    assert cap.valid_webhook(url) is ok


# ─── delivery metadata ────────────────────────────────────────────────────────

def test_headers_carry_the_tag_verbatim_and_the_numbers_a_caller_needs():
    w = cap.Window(tag="clip|lounge|35", max_ms=10_000)
    for _ in range(cap.Window.FLOOR_FRAMES):
        w.push(_frame(), QUIET)
    for _ in range(5):
        w.push(_frame(), SPEECH)
    h = cap.headers(w.close(), "DEVICE1", dropped=2)
    assert h["Content-Type"] == "audio/wav"
    assert h["X-EM-Tag"]     == "clip|lounge|35"
    assert h["X-EM-Device"]  == "DEVICE1"
    assert h["X-EM-Ms"]      == str(10 * FRAME_MS)
    assert h["X-EM-Dropped"] == "2"
    assert h["X-EM-Truncated"] == "0"
    assert float(h["X-EM-Peak-Db"]) == pytest.approx(-30.0, abs=0.5)
    # Every value must be a str: aiohttp refuses a non-string header value,
    # and the failure would land on the delivery, not here.
    assert all(isinstance(v, str) for v in h.values())


def test_sessions_are_distinct_so_a_stop_cannot_land_on_the_wrong_window():
    """
    On a matrix run, a stop that closed the NEXT window instead of the one it
    named would pair every recording after it with the wrong source file —
    silently, and for the rest of the run.
    """
    assert cap.Window().session != cap.Window().session


# ─── the pull store ───────────────────────────────────────────────────────────

def _result(session: str) -> cap.CaptureResult:
    return cap.CaptureResult(pcm=b"\x00\x01", tag=session, session=session,
                             peak_db=-40.0, floor_db=-70.0, frames=1,
                             truncated=False, opened_ms=0)


def test_a_result_is_consumed_on_read():
    """
    Same rule as the shadow tracker's crossings. Leaving it behind would let
    one recording be collected twice and paired with two different source
    files — and on a matrix run every file after that is mislabelled.
    """
    store = cap.ResultStore()
    store.put(_result("a"))
    assert store.take("a").session == "a"
    assert store.take("a") is None


def test_taking_without_a_session_gets_the_oldest():
    store = cap.ResultStore()
    for name in ("a", "b", "c"):
        store.put(_result(name))
    assert [store.take().session for _ in range(3)] == ["a", "b", "c"]


def test_the_store_is_bounded_and_drops_the_oldest():
    """
    A driver that stops collecting must not grow this without limit, and the
    NEWEST is the one somebody is currently waiting for — so the oldest goes.
    """
    store = cap.ResultStore(limit=2)
    for name in ("a", "b", "c"):
        store.put(_result(name))
    assert store.sessions == ["b", "c"]
    assert store.evicted == 1
    assert store.take("a") is None


def test_an_unknown_session_is_not_an_error():
    """The driver asks by name; a window that was evicted or never existed
    reads as 'nothing available', which is an ordinary answer."""
    assert cap.ResultStore().take("nope") is None


def test_pull_is_the_default_transport_because_push_needs_a_route_back():
    """
    The controller runs on a macvlan network here, and a macvlan container
    cannot reach its own Docker host — measured, a webhook on the driving
    machine times out every time with the audio captured and nowhere to go.
    So an arm call with no webhook must be accepted, not rejected.
    """
    api_src = API.read_text()
    handler = api_src[api_src.index("async def _post_device_capture"):]
    handler = handler[:handler.index("\n@auth")]
    assert "webhook is not None and not em_capture.valid_webhook" in handler
    assert "_get_capture_recording" in api_src


def test_a_delivery_failure_names_the_exception_type():
    """
    A bare asyncio.TimeoutError stringifies to the EMPTY STRING, so the log
    read 'delivery failed:' with nothing after it — on precisely the failure
    an unreachable webhook produces, which is the one that happened.
    """
    src = CONTROLLER.read_text()
    assert "type(last).__name__" in src


# ─── controller wiring (shape guards, as in test_samples.py) ──────────────────

def test_voice_turns_are_refused_while_capturing():
    """
    The refusal has to sit at _run_voice_locked, where wake word, dot button
    and HA's own start_conversation all meet. Guarding each trigger leaves
    whichever one nobody remembered still streaming a room to Home Assistant.

    Matched on the operands rather than the whole line: ambient recording
    (em_ambient) joined the same condition, so pinning the exact spelling
    would fail for a mode being ADDED to the guard this test exists to
    protect.
    """
    src  = CONTROLLER.read_text()
    body = src[src.index("async def _run_voice_locked"):][:4000]
    assert "device.capture_mode" in body
    assert "device.collect_mode" in body


def test_the_frame_tap_sits_in_the_wake_listener_before_the_model():
    """
    Same tap point as collect mode: the audio is byte-for-byte what the wake
    model scores, which is the whole reason these recordings are worth
    training on. Before `model.push`, so a capturing device is not paying for
    inference nothing may act on.
    """
    src  = CONTROLLER.read_text()
    body = src[src.index("async def wake_word_listener"):]
    tap   = body.index("_capture_frame(device, frame, rms)")
    score = body.index("model.push")
    assert tap < score


def test_capture_state_reaches_every_panel_that_shows_a_device():
    """
    A capturing device answers nothing, which from any other panel is
    indistinguishable from a broken one — and the person who armed it is not
    necessarily the person who next asks it for the weather.
    """
    assert '"captureMode":      device.capture_mode' in CONTROLLER.read_text()
    assert '"captureMode"' in API.read_text()
    assert "CAPTURING" in JSX.read_text()


def test_the_disarm_never_cancels_the_task_it_is_running_on():
    """
    The watchdog disarms the mode itself when the dead-man fires, so it is
    running INSIDE set_capture_mode when the tasks are cancelled. A blind
    cancel raises CancelledError at the next await and abandons the rest of
    the disarm — webhook and queue left set, no log line, no state push — so
    the device reads as still capturing everywhere except the frame tap.

    Shape guard rather than a live test: em_controller pulls in aiohttp,
    websockets and zeroconf, which this suite deliberately does not.
    """
    src = CONTROLLER.read_text()
    helper = src[src.index("def _capture_cancel_tasks"):]
    helper = helper[:helper.index("\ndef ", 1)]
    assert "asyncio.current_task()" in helper
    assert "task is not current" in helper
    # And nothing else may cancel these tasks behind its back.
    for attr in ("capture_sender_task", "capture_watchdog_task",
                 "capture_led_task"):
        assert src.count(f"device.{attr}.cancel()") == 0


def test_the_mode_is_not_persisted():
    """
    The counterpart to collect_mode's persistence, and the reason there is no
    schema change. A webhook is a running process's address: a controller
    that came back up still suspended, POSTing at a socket nobody holds,
    would be a device answering nothing for a reason nothing on screen
    explains.
    """
    db_src = (CONTROLLER_DIR / "em_db.py").read_text()
    assert "capture_mode" not in db_src
    assert "capture_webhook" not in db_src
