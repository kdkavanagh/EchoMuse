"""Normative Home Assistant Assist script generation (SPEC §16.7)."""

from __future__ import annotations

import hashlib
import json
from typing import Any

SCRIPT_REVISION = 1
EVENT_REQUEST = "echomuse_alert_request"
EVENT_RESULT = "echomuse_alert_result"


def canonical_json(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def config_sha256(config: dict) -> str:
    return hashlib.sha256(canonical_json(config).encode()).hexdigest()


def _relay(action: str, args: dict) -> list[dict]:
    return [
        {"variables": {
            "args": args,
            "request_id": "{{ context.id }}|" + action + "|{{ args | tojson }}",
        }},
        {"event": EVENT_REQUEST,
         "event_data": {"request_id": "{{ request_id }}", "action": action,
                        "args": "{{ args }}"}},
        {"wait_for_trigger": [{"trigger": "event", "event_type": EVENT_RESULT,
                               "event_data": {"request_id": "{{ request_id }}"}}],
         "timeout": "00:00:10"},
        {"variables": {"result":
            "{{ wait.trigger.event.data.result if wait.trigger\n"
            "   else {'ok': false, 'error': 'EchoMuse did not answer'} }}"}},
        {"stop": "done", "response_variable": "result"},
    ]


def _script(alias: str, description: str, fields: dict, action: str, args: dict) -> dict:
    return {"alias": alias, "description": description, "mode": "parallel",
            "fields": fields, "variables": {"echomuse_revision": SCRIPT_REVISION},
            "sequence": _relay(action, args)}


def render_scripts() -> dict[str, dict]:
    """The five full script configs generated from §16.7's schema."""
    speaker = {"selector": {"text": {}}}
    return {
        "echomuse_set_alarm": _script(
            "Set an alarm on an EchoMuse speaker",
            "The only way to set a clock-time alarm. Alarms are saved in Home Assistant's "
            "calendar and ring on the speaker. Set speaker to the area or speaker the user "
            "is talking through unless they name another. Use the timer tools for countdown timers.",
            {"time": {"required": True, "selector": {"time": {}}},
             "days": {"selector": {"select": {"multiple": True,
                         "options": ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]}}},
             "name": {"selector": {"text": {}}}, "speaker": speaker},
            "set_alarm", {"time": "{{ time }}", "days": "{{ days | default([]) }}",
                          "name": "{{ name | default('') }}",
                          "speaker": "{{ speaker | default('') }}"}),
        "echomuse_list_alarms": _script(
            "List alarms on an EchoMuse speaker",
            "Lists the next alarms on a speaker. Set speaker to the area or speaker the user "
            "is talking through unless they name another.",
            {"speaker": speaker}, "list_alarms",
            {"speaker": "{{ speaker | default('') }}"}),
        "echomuse_cancel_alarm": _script(
            "Cancel alarms on an EchoMuse speaker",
            "Cancels an alarm by name or time, or every alarm with all. Cancelling a repeating "
            "alarm removes the whole series. Set speaker as for the other EchoMuse tools.",
            {"name": {"selector": {"text": {}}}, "time": {"selector": {"time": {}}},
             "all": {"default": False, "selector": {"boolean": {}}}, "speaker": speaker},
            "cancel_alarm", {"name": "{{ name | default('') }}",
                             "time": "{{ time | default('') }}",
                             "all": "{{ all | default(false) }}",
                             "speaker": "{{ speaker | default('') }}"}),
        "echomuse_dismiss_alert": _script(
            "Stop the ringing timer or alarm on an EchoMuse speaker",
            "Stops whatever timer or alarm is ringing on the speaker. A repeating alarm still "
            "rings on its next day. Set speaker as for the other EchoMuse tools.",
            {"speaker": speaker}, "dismiss_alert",
            {"speaker": "{{ speaker | default('') }}"}),
        "echomuse_snooze_alarm": _script(
            "Snooze the ringing alarm on an EchoMuse speaker",
            "Snoozes the alarm ringing on the speaker. Timers cannot be snoozed. Set speaker as "
            "for the other EchoMuse tools.",
            {"speaker": speaker}, "snooze_alarm",
            {"speaker": "{{ speaker | default('') }}"}),
    }
