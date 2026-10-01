"""em_wake_rules: rule config validation, the shadow report, its persistence and
API rollup, and the Poisson bound on would-be false wakes."""

import math

import pytest

import em_db as db
import em_wake_rules as wr
from em_session import WakeCandidate

IDLE2 = {"profile": "idle", "windows": 2, "combine": "mean", "threshold": 0.95}
BASE = wr.baseline(0.90, 0.65)


@pytest.fixture()
def fresh_db(tmp_path):
    db.init(str(tmp_path / "test.db"))
    yield db
    if db._conn is not None:
        db._conn.close()
        db._conn = None


# ── config ───────────────────────────────────────────────────────────────

def test_defaults_are_valid_and_change_no_live_behaviour():
    defaults = db.DEFAULT_DEVICE_CONFIG
    assert wr.validate_config(defaults, BASE) is None
    rules = wr.RuleSet.from_config(defaults)
    assert rules.extra == () and rules.open_rules(0.90, 0.65) == BASE
    assert [r.key for r in rules.shadow] == [
        "idle:2:mean:0.90", "idle:2:mean:0.95", "idle:2:all:0.90", "idle:2:all:0.95", "idle:1:mean:0.97"]


@pytest.mark.parametrize("values", [
    {"wakeOpenRules": []},
    {"wakeOpenRules": [IDLE2, {**IDLE2, "profile": "playback", "threshold": 0.3}]},
    {"wakeOpenRules": [{**IDLE2, "threshold": 0.9}]},                    # 0.9 is 0.90
    {"wakeShadowRules": [{**IDLE2, "windows": w, "combine": c} for w in (1, 2, 3) for c in ("mean", "all")][:6]},
    {"wakeShadowRules": [{**IDLE2, "threshold": t / 100} for t in range(90, 98)]},   # eight
])
def test_config_accepts_valid_rule_lists(values):
    assert wr.validate_config(values, BASE) is None


@pytest.mark.parametrize("values, fragment", [
    ({"wakeOpenRules": "idle:2:mean:0.95"}, "list"),
    ({"wakeOpenRules": [{**IDLE2, "threshold": t / 100} for t in range(90, 95)]}, "at most 4"),
    ({"wakeShadowRules": [{**IDLE2, "threshold": t / 100} for t in range(90, 99)]}, "at most 8"),
    ({"wakeShadowRules": [IDLE2, dict(IDLE2)]}, "twice"),
    ({"wakeOpenRules": [{"profile": "idle", "windows": 3, "combine": "mean", "threshold": 0.9}]}, "baseline"),
    ({"wakeOpenRules": [{**IDLE2, "threshold": 0.29}]}, "0.30"),
    ({"wakeOpenRules": [{**IDLE2, "threshold": 1.0}]}, "0.99"),
    ({"wakeOpenRules": [{**IDLE2, "threshold": 0.955}]}, "two decimals"),
    ({"wakeShadowRules": [{**IDLE2, "windows": 4}]}, "windows"),
    ({"wakeShadowRules": [{**IDLE2, "windows": True}]}, "windows"),
    ({"wakeShadowRules": [{**IDLE2, "combine": "max"}]}, "combine"),
    ({"wakeShadowRules": [{**IDLE2, "profile": "speech"}]}, "profile"),
])
def test_config_rejects_invalid_rule_lists(values, fragment):
    message = wr.validate_config(values, BASE)
    assert message is not None and fragment in message


# ── wire ─────────────────────────────────────────────────────────────────

def _candidate(**extra):
    return {"lease_id": "l", "candidate_id": "c", "capture_epoch": "1", "support_start": "640",
            "first_crossing_end": "23040", "graph_sha256": "g", "threshold": 0.9,
            "hops": [{"end_sample": "23040", "raw": 0.95, "smoothed": 0.95}], **extra}


def test_wake_candidate_rule_is_parsed_and_absent_is_none():
    assert WakeCandidate.parse(_candidate()).rule is None
    rule = WakeCandidate.parse(_candidate(rule={**IDLE2, "threshold": 0.9})).rule
    assert rule == wr.OpenRule(wr.RuleProfile.IDLE, 2, wr.RuleCombine.MEAN, 0.9)
    with pytest.raises(ValueError):
        WakeCandidate.parse(_candidate(rule={**IDLE2, "windows": 7}))


def test_absent_shadow_is_no_data_and_an_empty_list_is_none_configured():
    assert wr.parse_shadow({"hops_scored": 3}) is None
    assert wr.parse_shadow({"shadow": []}) == ()


# ── persistence and rollup ───────────────────────────────────────────────

def _report(rule=IDLE2, *, hops=22_500, matched=0, lead=None, unmatched=0, retried=0, live_only=0, events=()):
    hist = [0] * 7
    for value in lead or ():
        hist[value + 3] += 1
    return wr.ShadowReport.parse({
        "rule": rule, "hops": hops, "opens": matched + unmatched + retried, "matched": matched,
        "lead_hist": hist, "unmatched": unmatched, "retried": retried, "live_only": live_only,
        "events_dropped": 0,
        "events": [{"kind": k, "open_sample": "2560", "mono_ns": str(n), "peak_raw": 0.97, "raws": [0.5, 0.97]}
                   for n, k in events]})


def test_shadow_reports_upsert_per_hour_and_aggregate_across_hours(fresh_db, monkeypatch):
    clock = [3600 * 1000 + 10.0]
    monkeypatch.setattr(db.time, "time", lambda: clock[0])
    db.record_wake_shadow("D", [_report(matched=2, lead=[1, 2], unmatched=1)], float)
    db.record_wake_shadow("D", [_report(matched=1, lead=[-1], live_only=1)], float)
    clock[0] += 3600
    db.record_wake_shadow("D", [_report(matched=1, lead=[0]),
                                _report({**IDLE2, "threshold": 0.9}, unmatched=2)], float)
    rows = db._conn.execute("SELECT hour_ts, rule_key, matched, lead_hist FROM wake_shadow "
                            "WHERE rule_key = 'idle:2:mean:0.95' ORDER BY hour_ts").fetchall()
    assert [tuple(r) for r in rows] == [(3600 * 1000, "idle:2:mean:0.95", 3, "[0, 0, 1, 0, 1, 1, 0]"),
                                        (3600 * 1001, "idle:2:mean:0.95", 1, "[0, 0, 0, 1, 0, 0, 0]")]

    totals = db.get_wake_shadow("D", 0)
    configured = (wr.OpenRule.from_config(IDLE2), wr.OpenRule.from_config({**IDLE2, "windows": 1}))
    summary = {s["key"]: s for s in wr.summarize(totals, configured)}
    assert list(summary) == ["idle:2:mean:0.95", "idle:1:mean:0.95", "idle:2:mean:0.90"]

    active = summary["idle:2:mean:0.95"]
    assert active["active"] and active["hops"] == 67_500 and active["hours"] == pytest.approx(3.0)
    assert (active["matched"], active["matched_earlier"], active["unmatched"], active["live_only"]) == (4, 2, 1, 1)
    assert active["mean_lead_ms"] == pytest.approx((1 + 2 - 1 + 0) * 160 / 4)
    assert active["unmatched_per_hour"] == pytest.approx(1 / 3)
    assert active["unmatched_per_hour_upper95"] == pytest.approx(wr.poisson_upper(1) / 3)

    idle_one = summary["idle:1:mean:0.95"]          # configured, nothing reported yet
    assert idle_one["active"] and idle_one["hours"] == 0 and idle_one["mean_lead_ms"] is None
    assert idle_one["unmatched_per_hour"] is None and idle_one["unmatched_per_hour_upper95"] is None

    retired = summary["idle:2:mean:0.90"]           # reported, no longer configured
    assert not retired["active"] and retired["unmatched"] == 2 and retired["rule"]["threshold"] == 0.9


def test_shadow_events_carry_wall_times_and_keep_the_newest_per_device(fresh_db, monkeypatch):
    monkeypatch.setattr(db, "WAKE_SHADOW_EVENT_RETENTION", 3)
    for n in range(5):
        db.record_wake_shadow("D", [_report(unmatched=1, events=[(n, "unmatched")])], lambda mono: 100.0 + mono)
    db.record_wake_shadow("E", [_report(retried=1, events=[(9, "retried")])], lambda mono: 50.0)
    events = db.get_wake_shadow_events("D", 0, 50)
    assert [e.ts for e in events] == [104.0, 103.0, 102.0]
    assert events[0].kind == "unmatched" and events[0].rule_key == "idle:2:mean:0.95" and events[0].raws == (0.5, 0.97)
    assert [e.kind for e in db.get_wake_shadow_events("E", 0, 50)] == ["retried"]


# ── Poisson bound ────────────────────────────────────────────────────────

def test_poisson_upper_bound_matches_the_closed_form_at_zero_and_grows_with_count():
    hours = 7.0
    assert wr.poisson_upper(0) / hours == pytest.approx(-math.log(0.05) / hours, abs=1e-3)
    assert wr.poisson_upper(0) == pytest.approx(2.996, abs=1e-3)
    bounds = [wr.poisson_upper(x) for x in range(0, 40)]
    assert all(b > a for a, b in zip(bounds, bounds[1:]))
    assert wr.poisson_upper(1) == pytest.approx(4.744, abs=1e-3)       # tabulated one-sided 95 %
    assert wr.poisson_upper(1000) > 1000
