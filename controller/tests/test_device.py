"""em_device: registration policy, session.ready, D→C routing, button policy,
session loss, and the LED projection."""

import pytest

pytest.importorskip("websockets")

import em_device  # noqa: E402
from _device_fakes import (  # noqa: E402
    FakeLink, FakeRender, FakeStore, hello, make_hub, run, settle,
)

Rejected = em_device.em_device_link.Rejected
Admitted = em_device.em_device_link.Admitted


def _env(body: dict, msg_type: str = "x") -> dict:
    return {"type": msg_type, "body": body, "generation": 0, "message_id": "m"}


async def _online(store=None, host=None):
    store = store or FakeStore()
    store.add("DEV1", approved=True)
    hub, host, store = make_hub(store, host)
    admitted = await hub.admit(hello(), device_id="DEV1", peer_ip="10.0.0.5", secure=True, token=None)
    link = FakeLink(hello())
    await admitted.sink.on_ready(link)
    device = hub.devices["DEV1"]
    return device, link, host, store, admitted.sink


# ── admission ────────────────────────────────────────────────────────────

def test_unknown_device_in_strict_mode_is_registered_and_held_pending():
    async def scenario():
        hub, host, store = make_hub(FakeStore(approval="strict"))
        result = await hub.admit(hello(), device_id="NEW", peer_ip="1.2.3.4", secure=False, token=None)
        assert result == Rejected("pending_approval")
        assert store.registered == ["NEW"] and ("pending", "NEW") in host.events
        assert "NEW" not in hub.devices
    run(scenario())


def test_unknown_device_in_auto_mode_is_approved_and_admitted():
    async def scenario():
        hub, _, store = make_hub(FakeStore(approval="auto"))
        result = await hub.admit(hello(), device_id="NEW12345", peer_ip="1.2.3.4",
                                 secure=False, token=None)
        assert isinstance(result, Admitted)
        assert store.rows["NEW12345"]["approved"] and hub.devices["NEW12345"].label == "Unknown NEW12345"
        await hub.devices["NEW12345"].close()
    run(scenario())


def test_known_but_unapproved_device_stays_pending():
    async def scenario():
        store = FakeStore()
        store.add("DEV1", approved=False)
        hub, host, _ = make_hub(store)
        assert await hub.admit(hello(), device_id="DEV1", peer_ip="x", secure=False,
                               token=None) == Rejected("pending_approval")
    run(scenario())


@pytest.mark.parametrize("presented, secure, require_tls", [
    ("wrong", True, False),        # mismatching token always rejects
    (None, False, True),           # REQUIRE_DEVICE_TLS demands TLS + token
])
def test_link_auth_policy_rejects_as_unauthorized(presented, secure, require_tls):
    async def scenario():
        store = FakeStore(token="right")
        store.add("DEV1", approved=True)
        hub, _, _ = make_hub(store, require_tls=require_tls)
        assert await hub.admit(hello(), device_id="DEV1", peer_ip="x", secure=secure,
                               token=presented) == Rejected("unauthorized")
    run(scenario())


def test_ready_carries_speech_assets_active_thresholds_and_duck_depth():
    async def scenario():
        store = FakeStore()
        store.add("DEV1", approved=True)
        store.configs["DEV1"] = {"duckDb": -9.0}
        hub, _, _ = make_hub(store)
        result = await hub.admit(hello(), device_id="DEV1", peer_ip="x", secure=True, token=None)
        ready = result.ready
        assert ready["capture_permitted"] is True
        assert ready["assets"] == {"runtime_sha256": "r", "graph_sha256": "g", "sidecar_sha256": "s"}
        detector = ready["detector"]
        assert detector["thresholds"] == {"idle": 0.9, "playback": 0.65, "near_miss": 0.17}
        assert (detector["hop_blocks"], detector["smoothing"], detector["clear_after_unscored"]) == (2, 3, 6)
        assert detector["provisional_duck"] == {"duck_db": -9.0, "max_per_window": 2, "window_ms": 5000}
        await hub.devices["DEV1"].close()
    run(scenario())


def test_ready_pushes_only_device_owned_config_keys():
    async def scenario():
        store = FakeStore()
        store.configs["DEV1"] = {"duckDb": -18.0, "startupVolume": 90, "wakeModel": "abc",
                                 "timerSound": "bell"}
        device, link, host, _, _ = await _online(store)
        assert ("config", {"duckDb": -18.0, "startupVolume": 90}) in link.sent
        assert ("connected", "DEV1") in host.events
        assert host.alerts.calls[0][0] == "hello"
        await device.close()
    run(scenario())


def test_changing_the_selected_wake_model_renegotiates_the_session():
    # The graph is named only in session.ready, so a new selection must end
    # the session; any other config change keeps it.
    async def scenario():
        device, link, *_ = await _online()
        assert device.wake_model_sha256 == "g"
        await device.apply_config({"duckDb": -12.0, "wakeModel": "g"})
        assert not link.closed
        await device.apply_config({"duckDb": -12.0, "wakeModel": "h"})
        assert link.closed
        await device.close()
    run(scenario())


# ── routing ──────────────────────────────────────────────────────────────

def test_render_messages_are_consumed_before_anything_else():
    async def scenario():
        device, _, host, _, sink = await _online()
        sink.on_message("render.finished", _env({"playback_id": "p"}))
        assert device.actor.messages == [] and "render.finished" in host.render.seen
        await device.close()
    run(scenario())


def test_actor_and_alert_messages_reach_their_owners():
    async def scenario():
        device, _, host, _, sink = await _online()
        for typ in ("wake.candidate", "uplink.ended", "stream.open", "privacy.changed"):
            sink.on_message(typ, _env({"muted": True} if typ == "privacy.changed" else {}, typ))
        sink.on_message("alert.state", _env({"active": {"id": "a"}}))
        sink.on_message("alert.ring_ended", _env({"id": "t", "kind": "timer"}))
        sink.on_message("alert.local_operation", _env({"op_id": "o"}))
        sink.on_message("alert.ack", _env({"applied_through": 3}))
        sink.on_message("command.ack", _env({"message_id": "m1", "status": "applied"}))
        await settle()
        assert [m for m, _ in device.actor.messages] == [
            "wake.candidate", "uplink.ended", "stream.open", "privacy.changed",
            "alert.state", "command.ack"]
        kinds = [c[0] for c in host.alerts.calls]
        for kind in ("state", "ring_ended", "local_operation", "ack", "command_ack"):
            assert kind in kinds
        assert device.muted is True and device.alert_state == {"active": {"id": "a"}}
        await device.close()
    run(scenario())


def test_wake_stats_are_kept_with_receipt_time_and_persisted():
    async def scenario():
        device, _, _, store, sink = await _online()
        body = {"hops_scored": 187, "near_misses": [{"peak": 0.3}], "wake_unavailable": None}
        sink.on_message("wake.stats", _env(body))
        await settle()
        assert store.wake == [body]
        assert device.wake_stats["hops_scored"] == 187 and isinstance(device.wake_stats["received_ms"], int)
        await device.close()
    run(scenario())


def test_wake_stats_map_onto_the_wake_counter_columns(monkeypatch):
    calls = []
    monkeypatch.setattr(em_device.em_db, "bump_wake_counters", lambda did, **kw: calls.append(kw))
    em_device.DbStore().wake_stats("DEV1", {
        "hops_scored": 187, "hops_dropped": 2, "candidates_opened": 1, "peak_smoothed": 0.93,
        "infer_max_ms": 131.0, "near_misses": [{"peak": 0.31}, {"peak": 0.44}]})
    em_device.DbStore().wake_stats("DEV1", {"hops_scored": 0, "near_misses": [],
                                            "peak_smoothed": None, "infer_max_ms": None})
    assert calls[0] == {"near_misses": 2, "near_miss_max": 0.44, "dev_hops": 187, "dev_drops": 2,
                        "dev_crossings": 1, "dev_max_score": 0.93, "dev_max_infer_ms": 131}
    assert calls[1]["near_miss_max"] is None and calls[1]["dev_max_score"] is None
    assert calls[1]["dev_max_infer_ms"] is None


# ── button policy ────────────────────────────────────────────────────────

def _press(seq, **kw):
    return _env({"click_type": 138, "down": False, "held_ms": 120, "muted": False,
                 "physical_seq": seq, "occurrence_id": None, "handled": None, **kw})


def test_button_policy_routes_each_gesture():
    async def scenario():
        device, _, host, _, sink = await _online()
        sink.on_message("button.action", _press(1))                     # tap → turn
        sink.on_message("button.action", _press(1))                     # duplicate seq
        sink.on_message("button.action", _press(2, handled="alert_stopped", occurrence_id="o"))
        sink.on_message("button.action", _press(3, held_ms=900))        # hold → HA event
        sink.on_message("button.action", _press(4, muted=True))         # blocked
        await settle()
        device.actor.turn_active = True
        sink.on_message("button.action", _press(5))                     # cancel
        await settle()
        assert [t["physical_seq"] for t in device.actor.turns] == [1]
        assert device.actor.cancels == ["interrupted"]
        assert ("button_event", "long") in host.events
        await device.close()
    run(scenario())


# ── session loss ─────────────────────────────────────────────────────────

def test_session_loss_fails_playbacks_and_detaches_everything():
    async def scenario():
        device, link, host, _, sink = await _online()
        sink.on_lost("session_lost")
        await settle()
        assert not device.online and device.link is None and device.render is None
        assert host.render.failed == ["session_lost"] and device.actor.detached == ["session_lost"]
        assert ("lost", "DEV1") in host.alerts.calls and ("disconnected", "DEV1") in host.events
        await device.close()
    run(scenario())


def test_a_stale_link_loss_does_not_touch_the_new_session():
    async def scenario():
        device, old, host, _, _ = await _online()
        host.render = FakeRender()
        new = FakeLink(hello())
        await device._ready(new)
        device._lost(old, "closed")
        assert device.link is new and device.actor.detached == []
        await device.close()
    run(scenario())


def test_send_while_offline_raises_link_closed():
    async def scenario():
        device, _, _, _, sink = await _online()
        sink.on_lost("closed")
        with pytest.raises(em_device.em_device_link.LinkClosed):
            await device.send("leds", {})
        await device.close()
    run(scenario())


# ── LED projection (§11.2) ───────────────────────────────────────────────

@pytest.mark.parametrize("state, key", [("LISTENING", "listening_anim"), ("EXPECT_REPLY", "listening_anim"),
                                        ("THINKING", "spin_anim")])
def test_led_projection_follows_actor_state(state, key):
    async def scenario():
        device, _, _, _, _ = await _online()
        device.actor.state = state
        assert device._project_led() == device.led_scene[key]
        await device.close()
    run(scenario())


def test_idle_ring_shows_the_first_timer_fraction_and_diagnostic_wins():
    async def scenario():
        device, _, _, _, _ = await _online()
        assert device._project_led() == {"pattern": "off"}
        device.update_timer_projection([{"total_seconds": 120, "remaining_seconds": 30},
                                        {"total_seconds": 10, "remaining_seconds": 10}])
        lit = [led for led in device._project_led()["leds"] if (led["r"], led["g"], led["b"]) != (0, 0, 0)]
        assert len(lit) == 3                              # 30/120 of 12 LEDs
        device.diagnostic = True
        assert device._project_led()["pattern"] == "pulse"
        await device.close()
    run(scenario())
