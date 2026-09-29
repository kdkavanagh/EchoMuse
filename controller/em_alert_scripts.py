"""Normative Home Assistant Assist script generation (SPEC §16.7)."""

from __future__ import annotations

import hashlib
from enum import StrEnum
from typing import Mapping, TypedDict

from em_alert_wire import Weekday, canonical_json

SCRIPT_REVISION = 1


class RelayEvent(StrEnum):
    """The HA events of the §16.7 LLM relay."""
    REQUEST = "echomuse_alert_request"
    RESULT = "echomuse_alert_result"


class RelayAction(StrEnum):
    """`action` of an `echomuse_alert_request` (§16.7)."""
    SET_ALARM = "set_alarm"
    LIST_ALARMS = "list_alarms"
    CANCEL_ALARM = "cancel_alarm"
    DISMISS_ALERT = "dismiss_alert"
    SNOOZE_ALARM = "snooze_alarm"


class ScriptId(StrEnum):
    """Object ids of the five §16.7 Assist scripts."""
    SET_ALARM = "echomuse_set_alarm"
    LIST_ALARMS = "echomuse_list_alarms"
    CANCEL_ALARM = "echomuse_cancel_alarm"
    DISMISS_ALERT = "echomuse_dismiss_alert"
    SNOOZE_ALARM = "echomuse_snooze_alarm"


class ScriptVariables(TypedDict):
    echomuse_revision: int


class ScriptConfig(TypedDict):
    """One full script config as `POST /api/config/script/config/<id>` takes it."""
    alias: str
    description: str
    mode: str
    fields: dict[str, object]
    variables: ScriptVariables
    sequence: list[dict[str, object]]


def config_sha256(config: Mapping[str, object]) -> str:
    return hashlib.sha256(canonical_json(config).encode()).hexdigest()


def _relay(action: RelayAction, args: dict[str, str]) -> list[dict[str, object]]:
    return [
        {"variables": {
            "args": args,
            "request_id": "{{ context.id }}|" + action + "|{{ args | tojson }}",
        }},
        {"event": RelayEvent.REQUEST,
         "event_data": {"request_id": "{{ request_id }}", "action": action,
                        "args": "{{ args }}"}},
        {"wait_for_trigger": [{"trigger": "event", "event_type": RelayEvent.RESULT,
                               "event_data": {"request_id": "{{ request_id }}"}}],
         "timeout": "00:00:10"},
        {"variables": {"result":
            "{{ wait.trigger.event.data.result if wait.trigger\n"
            "   else {'ok': false, 'error': 'EchoMuse did not answer'} }}"}},
        {"stop": "done", "response_variable": "result"},
    ]


def _script(alias: str, description: str, fields: dict[str, object], action: RelayAction,
            args: dict[str, str]) -> ScriptConfig:
    return {"alias": alias, "description": description, "mode": "parallel",
            "fields": fields, "variables": {"echomuse_revision": SCRIPT_REVISION},
            "sequence": _relay(action, args)}


def render_scripts() -> dict[ScriptId, ScriptConfig]:
    """The five full script configs generated from §16.7's schema."""
    speaker: dict[str, object] = {"selector": {"text": {}}}
    return {
        ScriptId.SET_ALARM: _script(
            "Set an alarm on an EchoMuse speaker",
            "The only way to set a clock-time alarm. Alarms are saved in Home Assistant's "
            "calendar and ring on the speaker. Set speaker to the area or speaker the user "
            "is talking through unless they name another. Use the timer tools for countdown timers.",
            {"time": {"required": True, "selector": {"time": {}}},
             "days": {"selector": {"select": {"multiple": True,
                         "options": list(Weekday)}}},
             "name": {"selector": {"text": {}}}, "speaker": speaker},
            RelayAction.SET_ALARM, {"time": "{{ time }}", "days": "{{ days | default([]) }}",
                                    "name": "{{ name | default('') }}",
                                    "speaker": "{{ speaker | default('') }}"}),
        ScriptId.LIST_ALARMS: _script(
            "List alarms on an EchoMuse speaker",
            "Lists the next alarms on a speaker. Set speaker to the area or speaker the user "
            "is talking through unless they name another.",
            {"speaker": speaker}, RelayAction.LIST_ALARMS,
            {"speaker": "{{ speaker | default('') }}"}),
        ScriptId.CANCEL_ALARM: _script(
            "Cancel alarms on an EchoMuse speaker",
            "Cancels an alarm by name or time, or every alarm with all. Cancelling a repeating "
            "alarm removes the whole series. Set speaker as for the other EchoMuse tools.",
            {"name": {"selector": {"text": {}}}, "time": {"selector": {"time": {}}},
             "all": {"default": False, "selector": {"boolean": {}}}, "speaker": speaker},
            RelayAction.CANCEL_ALARM, {"name": "{{ name | default('') }}",
                                       "time": "{{ time | default('') }}",
                                       "all": "{{ all | default(false) }}",
                                       "speaker": "{{ speaker | default('') }}"}),
        ScriptId.DISMISS_ALERT: _script(
            "Stop the ringing timer or alarm on an EchoMuse speaker",
            "Stops whatever timer or alarm is ringing on the speaker. A repeating alarm still "
            "rings on its next day. Set speaker as for the other EchoMuse tools.",
            {"speaker": speaker}, RelayAction.DISMISS_ALERT,
            {"speaker": "{{ speaker | default('') }}"}),
        ScriptId.SNOOZE_ALARM: _script(
            "Snooze the ringing alarm on an EchoMuse speaker",
            "Snoozes the alarm ringing on the speaker. Timers cannot be snoozed. Set speaker as "
            "for the other EchoMuse tools.",
            {"speaker": speaker}, RelayAction.SNOOZE_ALARM,
            {"speaker": "{{ speaker | default('') }}"}),
    }
