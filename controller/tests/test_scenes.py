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
        # led_anim specs must carry the fields firmware keys on
        assert scene["listening_anim"]["pattern"] == "solid"
        assert scene["listening_anim"]["ttlSec"] > 0
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
    assert scene["spin_anim"]["colors"][0] == [0x40, 0x50, 0x60]


def test_custom_scene_bad_hex_falls_back_to_defaults():
    scene = em_scenes.resolve({"ledScene": "custom", "ledListenColor": "#zzz"})
    led = scene["listening"][0]
    assert (led["r"], led["g"], led["b"]) == (0, 180, 0)


def test_pride_rotates_whole_palette():
    scene = em_scenes.resolve({"ledScene": "pride"})
    assert scene["spin_anim"]["pattern"] == "rotate"
    assert len(scene["spin_anim"]["colors"]) == em_scenes.NUM_LEDS


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


def test_dialog_rings_are_dead_man_bounded():
    """Remote dialog indicators must clear on controller loss (§11.2): every
    turn-state layer is short enough for em_device to renew (TTL <= 30 s,
    re-sent every 10 s) and longer than one renewal period."""
    scene = em_scenes.resolve({})
    for key in ("listening_anim", "spin_anim", "meter_anim"):
        assert 10 < scene[key]["ttlSec"] <= 30


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

