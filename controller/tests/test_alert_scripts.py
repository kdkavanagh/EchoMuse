import copy
import hashlib
import re
from pathlib import Path

import pytest

from em_alert_scripts import SCRIPT_REVISION, canonical_json, config_sha256, render_scripts


def test_five_normative_scripts_and_hashes():
    scripts = render_scripts()
    assert set(scripts) == {
        "echomuse_set_alarm", "echomuse_list_alarms", "echomuse_cancel_alarm",
        "echomuse_dismiss_alert", "echomuse_snooze_alarm"}
    for object_id, config in scripts.items():
        assert config["mode"] == "parallel"
        assert config["variables"] == {"echomuse_revision": SCRIPT_REVISION}
        assert config_sha256(config) == hashlib.sha256(canonical_json(config).encode()).hexdigest()
        action = object_id.removeprefix("echomuse_")
        sequence = config["sequence"]
        assert sequence[0]["variables"]["request_id"] == (
            "{{ context.id }}|" + action + "|{{ args | tojson }}")
        assert sequence[1] == {
            "event": "echomuse_alert_request",
            "event_data": {"request_id": "{{ request_id }}", "action": action,
                           "args": "{{ args }}"}}
        assert sequence[2] == {
            "wait_for_trigger": [{"trigger": "event", "event_type": "echomuse_alert_result",
                                  "event_data": {"request_id": "{{ request_id }}"}}],
            "timeout": "00:00:10"}
        assert sequence[3]["variables"]["result"] == (
            "{{ wait.trigger.event.data.result if wait.trigger\n"
            "   else {'ok': false, 'error': 'EchoMuse did not answer'} }}")
        assert sequence[4] == {"stop": "done", "response_variable": "result"}


def test_generation_schema_substitutions_are_exact():
    scripts = render_scripts()
    expected = {
        "echomuse_set_alarm": {"time": "{{ time }}", "days": "{{ days | default([]) }}",
                               "name": "{{ name | default('') }}",
                               "speaker": "{{ speaker | default('') }}"},
        "echomuse_list_alarms": {"speaker": "{{ speaker | default('') }}"},
        "echomuse_cancel_alarm": {"name": "{{ name | default('') }}",
                                  "time": "{{ time | default('') }}",
                                  "all": "{{ all | default(false) }}",
                                  "speaker": "{{ speaker | default('') }}"},
        "echomuse_dismiss_alert": {"speaker": "{{ speaker | default('') }}"},
        "echomuse_snooze_alarm": {"speaker": "{{ speaker | default('') }}"},
    }
    for name, args in expected.items():
        assert scripts[name]["sequence"][0]["variables"]["args"] == args


def test_script_field_schema_and_text_match_spec():
    scripts = render_scripts()
    set_alarm = scripts["echomuse_set_alarm"]
    assert set_alarm["alias"] == "Set an alarm on an EchoMuse speaker"
    assert set_alarm["fields"] == {
        "time": {"required": True, "selector": {"time": {}}},
        "days": {"selector": {"select": {"multiple": True,
                    "options": ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]}}},
        "name": {"selector": {"text": {}}},
        "speaker": {"selector": {"text": {}}},
    }
    assert scripts["echomuse_cancel_alarm"]["fields"]["all"] == {
        "default": False, "selector": {"boolean": {}}}


def _spec_scripts():
    yaml = pytest.importorskip("yaml")
    spec = Path(__file__).resolve().parents[2] / "docs" / "post-afe-audio-architecture.md"
    text = spec.read_text(encoding="utf-8")
    block = re.search(r"```yaml\n(echomuse_set_alarm:.*?)```", text, re.S).group(1)
    return yaml.safe_load(block)


def test_rendering_equals_spec_yaml_under_the_generation_schema():
    spec = _spec_scripts()
    scripts = render_scripts()
    assert scripts["echomuse_set_alarm"] == spec["echomuse_set_alarm"]
    relay = spec["echomuse_set_alarm"]["sequence"]
    for object_id, rendered in scripts.items():
        expected = copy.deepcopy(spec[object_id])
        if expected.get("sequence") is None:        # the SPEC elides it with a comment
            action = object_id.removeprefix("echomuse_")
            sequence = copy.deepcopy(relay)
            sequence[0]["variables"]["args"] = rendered["sequence"][0]["variables"]["args"]
            sequence[0]["variables"]["request_id"] = "{{ context.id }}|" + action + "|{{ args | tojson }}"
            sequence[1]["event_data"]["action"] = action
            expected["sequence"] = sequence
        assert rendered == expected, object_id
