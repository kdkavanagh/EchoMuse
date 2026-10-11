"""
Guard against the config-clobber trap.

Config POSTs replace the stored dict rather than merging: safe for the
dashboard, which submits the complete config, and destructive for a caller
that submits one key. `_dropped_keys` makes such a write refuse instead of
silently resetting every omitted setting.

The pure key-set logic is exercised directly; em_api pulls in the whole
controller stack, which this suite keeps out.
"""

import ast
import re
from pathlib import Path

import pytest

CONTROLLER = Path(__file__).resolve().parents[1]


def _load_dropped_keys():
    """Exec `_dropped_keys` from em_api's source in isolation (stdlib only).
    Decorators are stripped here, which is why the placement tests below
    parse the real file."""
    tree = ast.parse((CONTROLLER / "em_api.py").read_text())
    for node in ast.walk(tree):
        if isinstance(node, ast.FunctionDef) and node.name == "_dropped_keys":
            node.decorator_list = []
            mod = ast.Module(body=[node], type_ignores=[])
            ns: dict = {}
            exec(compile(ast.fix_missing_locations(mod), "<em_api>", "exec"), ns)
            return ns["_dropped_keys"]
    raise AssertionError("could not locate _dropped_keys in em_api.py")


dropped_keys = _load_dropped_keys()

# A stored fleet config after the schema-22 cutover.
LIVE_CONFIG = {
    "wakeModel": "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f",
    "wakeArbitrationMs": 700, "wakeSound": False, "saveWakeClips": False,
    "nsAsr": True, "saveUtterances": False, "extendedUtterances": False,
    "timerSound": "chime", "alarmSound": "chime", "timerRingSeconds": 900,
    "timerRingGapSeconds": 2.0, "startupVolume": 85, "duckDb": -18.0,
    "bleProxyEnabled": True, "eqBands": [0, 3, 2, 0, -2, 3, 7, 0],
    "eqLoudness": True, "ledScene": "standard",
}


def test_a_one_key_write_is_caught_and_names_what_it_would_reset():
    dropped = dropped_keys({"wakeArbitrationMs": 500}, LIVE_CONFIG)
    assert len(dropped) == len(LIVE_CONFIG) - 1
    assert "wakeModel" in dropped and "alarmSound" in dropped


def test_full_config_write_drops_nothing():
    body = {**LIVE_CONFIG, "wakeArbitrationMs": 500}
    assert dropped_keys(body, LIVE_CONFIG) == []


def test_dropping_a_single_key_is_still_caught():
    body = {k: v for k, v in LIVE_CONFIG.items() if k != "alarmSound"}
    assert dropped_keys(body, LIVE_CONFIG) == ["alarmSound"]


def test_empty_stored_config_permits_anything():
    """A fresh install has nothing to destroy."""
    assert dropped_keys({"wakeArbitrationMs": 700}, {}) == []


def test_result_is_sorted_for_a_stable_error_message():
    out = dropped_keys({"nsAsr": False}, LIVE_CONFIG)
    assert out == sorted(out)


def test_guard_reads_raw_stored_config_not_the_underlaid_view():
    """The guard must call the raw accessor, or the false positive returns."""
    src = (CONTROLLER / "em_api.py").read_text()
    m = re.search(r"async def _post_global_config\(.*?(?=\nasync def )", src, re.S)
    assert m
    assert "get_global_device_config_raw" in m.group(0)


# ── decorator placement ───────────────────────────────────────────────────
#
# A helper inserted directly above a decorated handler steals its decorator,
# leaving the handler unauthenticated. The tests above strip decorators, so
# these parse the real file.

def _ast_tree():
    return ast.parse((CONTROLLER / "em_api.py").read_text())


def _decorators_of(name):
    for n in ast.walk(_ast_tree()):
        if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)) and n.name == name:
            return [getattr(d, "id", getattr(d, "attr", "?")) for d in n.decorator_list]
    raise AssertionError(f"{name} not found in em_api.py")


def test_dropped_keys_is_a_plain_helper_not_a_route_handler():
    assert _decorators_of("_dropped_keys") == [], (
        "_dropped_keys has picked up a decorator — it is a pure helper. This "
        "means it was inserted directly beneath a decorated handler and stole "
        "that decorator."
    )


@pytest.mark.parametrize("handler", [
    "_post_global_config",
    "_post_device_config",
    "_post_device_update",
    "_post_device_update_queue",
    "_delete_device_update_queue",
    "_post_deploy_firmware",
    "_get_provision_firmware",
])
def test_mutating_handlers_still_require_admin(handler):
    """Config writes, firmware installs and the firmware download keep their
    admin decorator."""
    assert "require_admin" in _decorators_of(handler), (
        f"{handler} lost its @auth.require_admin — anyone could call it"
    )
