import em_scenes


def _assert_frame(frame):
    assert len(frame) == em_scenes.NUM_LEDS
    for i, led in enumerate(frame):
        assert led["id"] == i
        for ch in ("r", "g", "b"):
            assert 0 <= led[ch] <= 255


def test_every_preset_resolves_render_ready():
    for name in ("standard", "airy", "malevolent", "pride"):
        scene = em_scenes.resolve({"ledScene": name})
        assert scene["name"] == name
        _assert_frame(scene["listening"])
        _assert_frame(scene["spin_frame"](0))
        _assert_frame(scene["spin_frame"](7))
        # led_anim specs must carry the fields firmware keys on
        assert scene["listening_anim"]["pattern"] == "solid"
        assert scene["listening_anim"]["listening"] is True
        assert scene["spin_anim"]["pattern"] in ("spin", "rotate")
        assert scene["spin_anim"]["ttlSec"] > 0
        assert scene["meter_anim"]["pattern"] == "meter"


def test_unknown_scene_falls_back_to_standard():
    scene = em_scenes.resolve({"ledScene": "does-not-exist"})
    assert scene["listening"] == em_scenes.resolve({"ledScene": "standard"})["listening"]


def test_empty_config_is_standard():
    assert em_scenes.resolve({})["listening"] == \
        em_scenes.resolve({"ledScene": "standard"})["listening"]
    assert em_scenes.resolve(None)["listening"] == \
        em_scenes.resolve({"ledScene": "standard"})["listening"]


def test_custom_scene_uses_configured_colours():
    scene = em_scenes.resolve({
        "ledScene": "custom",
        "ledListenColor": "#102030",
        "ledThinkColor": "#405060",
    })
    led = scene["listening"][0]
    assert (led["r"], led["g"], led["b"]) == (0x10, 0x20, 0x30)
    spin = scene["spin_frame"](3)
    assert (spin[3]["r"], spin[3]["g"], spin[3]["b"]) == (0x40, 0x50, 0x60)


def test_custom_scene_bad_hex_falls_back_to_defaults():
    scene = em_scenes.resolve({"ledScene": "custom", "ledListenColor": "#zzz"})
    led = scene["listening"][0]
    assert (led["r"], led["g"], led["b"]) == (0, 180, 0)


def test_spinner_position_wraps():
    scene = em_scenes.resolve({"ledScene": "standard"})
    assert scene["spin_frame"](0) == scene["spin_frame"](em_scenes.NUM_LEDS)


def test_pride_rotates_whole_palette():
    scene = em_scenes.resolve({"ledScene": "pride"})
    f0, f1 = scene["spin_frame"](0), scene["spin_frame"](1)
    # rotation: LED i at pos 1 shows what LED i-1 showed at pos 0
    for i in range(em_scenes.NUM_LEDS):
        a, b = f1[i], f0[(i - 1) % em_scenes.NUM_LEDS]
        assert (a["r"], a["g"], a["b"]) == (b["r"], b["g"], b["b"])


# ─── meter response curve (dashboard-tunable) ────────────────────────────────

def test_meter_curve_omits_absent_keys():
    """
    Absent config keys must NOT be sent: an empty override means "use the
    firmware default", which lets the device move its defaults without every
    stored config pinning the old ones.
    """
    anim = em_scenes.resolve({})["meter_anim"]
    for k in ("attack", "decay", "floor", "gamma", "ref", "curve"):
        assert k not in anim


def test_meter_curve_passes_through_and_clamps():
    anim = em_scenes.resolve({
        "meterDecay": 0.5, "meterFloor": 0.0,
        "meterGamma": 99, "meterRef": -1, "meterCurve": "bogus",
    })["meter_anim"]
    assert anim["decay"] == 0.5
    assert anim["floor"] == 0.0          # 0 is a real value, not "absent"
    assert anim["gamma"] == 3.5          # clamped to the top of the range
    assert anim["ref"] == 0.02           # clamped to the bottom
    assert "curve" not in anim           # unparseable is dropped, not crashed


def test_turn_state_ttls_are_bounded():
    """
    A controller that dies mid-turn used to leave the ring animating for
    three minutes. Each phase's TTL must now be bounded by what that phase
    can legitimately take.
    """
    scene = em_scenes.resolve({})
    assert scene["listening_anim"]["ttlSec"] <= 30
    # The spinner spans HA think time AND the TTS fetch, so its TTL has to
    # outlast _fetch_tts_audio's timeout (60s, two attempts) or it clears
    # the ring during exactly the long responses that need it. Guard the
    # coupling so moving one without the other fails here.
    assert scene["spin_anim"]["ttlSec"] >= 120
    assert scene["spin_anim"]["ttlSec"] <= 180


def test_meter_ttl_scales_with_response_length():
    """
    The meter TTL must always exceed the response it is showing — a fixed
    value would clear the ring part-way through a long answer, which is the
    exact bug the playback_stats rendezvous fixes at the other end.
    """
    assert em_scenes.meter_ttl(0.5) >= 30           # short clips get a floor
    for seconds in (5, 30, 120, 600):
        assert em_scenes.meter_ttl(seconds) > seconds * 1.5


def test_outcome_cues_are_self_clearing_and_distinct():
    """
    Cues must retire on the device's own ticker (no follow-up message to
    lose) and must be told apart by rhythm, since colour is already spoken
    for by mute/link/volume.
    """
    scene = em_scenes.resolve({})
    ns, err = scene["nospeech_anim"], scene["error_anim"]
    for a in (ns, err):
        assert a["pattern"] == "pulse"
        assert 0 < a["ttlSec"] <= 2
    assert ns["periodMs"] != err["periodMs"]
    assert err["periodMs"] < ns["periodMs"]   # error reads as more agitated


def test_timer_ring_cue_is_not_self_clearing():
    """
    The odd one out among the pulse anims: the outcome cues are flourishes
    that retire themselves in ~1s, but a firing timer has to stay lit until
    something stops it. A 1s TTL here would leave the ring dark while the
    alarm was still sounding, which reads as a fault.
    """
    scene = em_scenes.resolve({})
    ring = scene["ring_anim"]
    assert ring["pattern"] == "pulse"
    assert ring["ttlSec"] > scene["nospeech_anim"]["ttlSec"]


def test_timer_ring_rhythm_sits_between_the_outcome_cues():
    """
    Rhythm carries the meaning (colour is spoken for by mute/link/volume),
    so an alarm must not be mistakable for either cue: slower than the
    error blink, faster than the "heard nothing" throb.
    """
    scene = em_scenes.resolve({})
    ring = scene["ring_anim"]["periodMs"]
    assert scene["error_anim"]["periodMs"] < ring < scene["nospeech_anim"]["periodMs"]


def test_ring_ttl_outlasts_the_stop_cap():
    """
    The ring's dead-man must never expire while the alarm is still allowed
    to sound, at any configurable cap (timerRingSeconds tops out at 300).
    """
    for cap in (5, 60, 300):
        assert em_scenes.ring_ttl(cap) > cap
    assert em_scenes.ring_ttl(1) >= 15   # very short caps still get a floor
