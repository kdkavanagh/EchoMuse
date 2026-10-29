//! The five LLM tool scripts (§3.3.4, architecture §16.7) and the decision whether to
//! install each.
//!
//! Each script fires `echomuse_alert_request` and waits up to 10 s for
//! `echomuse_alert_result`. The configurations are rendered from the normative generation
//! schema: `echomuse_set_alarm` as given, the other four with the same relay sequence and
//! their own action and arguments. HA stores a posted script configuration as it is and
//! returns it unchanged, so the SHA-256 of the canonical JSON of what was written tells
//! whether the user edited it since.

use echomuse_protocol::canonical::canonical_sha256;
use serde_json::{Value, json};

/// The `echomuse_revision` of the scripts this controller renders.
pub const SCRIPT_REVISION: i64 = 1;

/// The event the scripts fire.
pub const REQUEST_EVENT: &str = "echomuse_alert_request";

/// The event the controller answers with.
pub const RESULT_EVENT: &str = "echomuse_alert_result";

/// How long a script waits for the answer.
pub const SCRIPT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// One script: its object id and configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptConfig {
    /// The object id (`script.<object_id>`).
    pub object_id: &'static str,
    /// The configuration to post.
    pub config: Value,
}

impl ScriptConfig {
    /// The entity id.
    pub fn entity_id(&self) -> String {
        format!("script.{}", self.object_id)
    }

    /// The digest recorded for it: lowercase hex SHA-256 of its canonical JSON.
    pub fn sha256(&self) -> String {
        digest(&self.config)
    }
}

/// The lowercase hex SHA-256 of a configuration's canonical JSON; `None` for one that
/// has no canonical form.
pub fn config_digest(config: &Value) -> Option<String> {
    canonical_sha256(config)
        .ok()
        .map(|sha| sha.as_str().to_owned())
}

fn digest(config: &Value) -> String {
    config_digest(config).expect("rendered scripts hold no floats")
}

/// The relay sequence `relay(action, args)` of the generation schema.
fn relay(action: &str, args: Value) -> Value {
    json!([
        {"variables": {
            "args": args,
            "request_id": format!("{{{{ context.id }}}}|{action}|{{{{ args | tojson }}}}"),
        }},
        {"event": REQUEST_EVENT,
         "event_data": {"request_id": "{{ request_id }}", "action": action, "args": "{{ args }}"}},
        {"wait_for_trigger": [
            {"trigger": "event", "event_type": RESULT_EVENT,
             "event_data": {"request_id": "{{ request_id }}"}}
         ],
         "timeout": "00:00:10"},
        // The folded block keeps the line break before its more-indented second line.
        {"variables": {"result": "{{ wait.trigger.event.data.result if wait.trigger\n   else {'ok': false, 'error': 'EchoMuse did not answer'} }}"}},
        {"stop": "done", "response_variable": "result"},
    ])
}

fn script(alias: &str, description: &str, fields: Value, action: &str, args: Value) -> Value {
    json!({
        "alias": alias,
        "description": description,
        "mode": "parallel",
        "fields": fields,
        "variables": {"echomuse_revision": SCRIPT_REVISION},
        "sequence": relay(action, args),
    })
}

const SPEAKER: &str = "{{ speaker | default('') }}";

/// The five scripts, in the order they are installed.
pub fn scripts() -> Vec<ScriptConfig> {
    let speaker_field = json!({"selector": {"text": {}}});
    vec![
        ScriptConfig {
            object_id: "echomuse_set_alarm",
            config: script(
                "Set an alarm on an EchoMuse speaker",
                "The only way to set a clock-time alarm. Alarms are saved in Home Assistant's \
                 calendar and ring on the speaker. Set speaker to the area or speaker the user \
                 is talking through unless they name another. Use the timer tools for \
                 countdown timers.",
                json!({
                    "time": {"required": true, "selector": {"time": {}}},
                    "days": {"selector": {"select": {"multiple": true,
                        "options": ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]}}},
                    "name": {"selector": {"text": {}}},
                    "speaker": speaker_field,
                }),
                "set_alarm",
                json!({
                    "time": "{{ time }}",
                    "days": "{{ days | default([]) }}",
                    "name": "{{ name | default('') }}",
                    "speaker": SPEAKER,
                }),
            ),
        },
        ScriptConfig {
            object_id: "echomuse_list_alarms",
            config: script(
                "List alarms on an EchoMuse speaker",
                "Lists the next alarms on a speaker. Set speaker to the area or speaker the \
                 user is talking through unless they name another.",
                json!({"speaker": speaker_field}),
                "list_alarms",
                json!({"speaker": SPEAKER}),
            ),
        },
        ScriptConfig {
            object_id: "echomuse_cancel_alarm",
            config: script(
                "Cancel alarms on an EchoMuse speaker",
                "Cancels an alarm by name or time, or every alarm with all. Cancelling a \
                 repeating alarm removes the whole series. Set speaker as for the other \
                 EchoMuse tools.",
                json!({
                    "name": {"selector": {"text": {}}},
                    "time": {"selector": {"time": {}}},
                    "all": {"default": false, "selector": {"boolean": {}}},
                    "speaker": speaker_field,
                }),
                "cancel_alarm",
                json!({
                    "name": "{{ name | default('') }}",
                    "time": "{{ time | default('') }}",
                    "all": "{{ all | default(false) }}",
                    "speaker": SPEAKER,
                }),
            ),
        },
        ScriptConfig {
            object_id: "echomuse_dismiss_alert",
            config: script(
                "Stop the ringing timer or alarm on an EchoMuse speaker",
                "Stops whatever timer or alarm is ringing on the speaker. A repeating alarm \
                 still rings on its next day. Set speaker as for the other EchoMuse tools.",
                json!({"speaker": speaker_field}),
                "dismiss_alert",
                json!({"speaker": SPEAKER}),
            ),
        },
        ScriptConfig {
            object_id: "echomuse_snooze_alarm",
            config: script(
                "Snooze the ringing alarm on an EchoMuse speaker",
                "Snoozes the alarm ringing on the speaker. Timers cannot be snoozed. Set \
                 speaker as for the other EchoMuse tools.",
                json!({"speaker": speaker_field}),
                "snooze_alarm",
                json!({"speaker": SPEAKER}),
            ),
        },
    ]
}

/// What HA holds for a script, and what this controller recorded writing.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptSituation<'a> {
    /// HA's current configuration; `None` when the script does not exist.
    pub current: Option<&'a Value>,
    /// The digest and revision recorded when this controller last wrote it.
    pub recorded: Option<(&'a str, i64)>,
}

/// What to do about one script.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptDecision {
    /// Write the rendered configuration (absent, or an unedited older revision).
    Install,
    /// HA holds exactly the rendered configuration; record it if the record differs.
    Current,
    /// The user edited it (or it is not one this controller wrote): leave it alone and
    /// warn.
    LeaveEdited,
}

/// Decides for `rendered` given `situation`: install it when absent; keep it when HA has
/// exactly it; upgrade only an unedited script whose recorded revision is older; leave
/// anything else alone.
pub fn decide(rendered: &ScriptConfig, situation: &ScriptSituation<'_>) -> ScriptDecision {
    let Some(current) = situation.current else {
        return ScriptDecision::Install;
    };
    let current_sha = config_digest(current);
    if current_sha.as_deref() == Some(rendered.sha256().as_str()) {
        return ScriptDecision::Current;
    }
    match situation.recorded {
        Some((sha, revision))
            if Some(sha) == current_sha.as_deref() && revision < SCRIPT_REVISION =>
        {
            ScriptDecision::Install
        }
        _ => ScriptDecision::LeaveEdited,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_five_scripts_follow_the_generation_schema() {
        let scripts = scripts();
        let ids: Vec<&str> = scripts.iter().map(|s| s.object_id).collect();
        assert_eq!(
            ids,
            [
                "echomuse_set_alarm",
                "echomuse_list_alarms",
                "echomuse_cancel_alarm",
                "echomuse_dismiss_alert",
                "echomuse_snooze_alarm"
            ]
        );
        let set = &scripts[0].config;
        assert_eq!(set["mode"], "parallel");
        assert_eq!(set["variables"]["echomuse_revision"], 1);
        assert_eq!(
            set["sequence"][0]["variables"]["request_id"],
            "{{ context.id }}|set_alarm|{{ args | tojson }}"
        );
        assert_eq!(
            set["sequence"][1],
            json!({"event": "echomuse_alert_request",
                   "event_data": {"request_id": "{{ request_id }}", "action": "set_alarm",
                                  "args": "{{ args }}"}})
        );
        assert_eq!(set["sequence"][2]["timeout"], "00:00:10");
        // Every script shares the wait, result and stop steps byte for byte.
        for script in &scripts[1..] {
            let tail = |config: &Value| config["sequence"].as_array().unwrap()[2..].to_vec();
            assert_eq!(tail(&script.config), tail(set));
        }
        let cancel = &scripts[2].config;
        assert_eq!(
            cancel["sequence"][0]["variables"]["args"],
            json!({"name": "{{ name | default('') }}", "time": "{{ time | default('') }}",
                   "all": "{{ all | default(false) }}", "speaker": "{{ speaker | default('') }}"})
        );
        assert_eq!(cancel["fields"]["all"]["default"], false);
        assert_eq!(scripts[3].entity_id(), "script.echomuse_dismiss_alert");
    }

    #[test]
    fn a_user_edited_script_is_left_alone() {
        let rendered = &scripts()[0];
        let sha = rendered.sha256();

        assert_eq!(
            decide(
                rendered,
                &ScriptSituation {
                    current: None,
                    recorded: None
                }
            ),
            ScriptDecision::Install
        );
        assert_eq!(
            decide(
                rendered,
                &ScriptSituation {
                    current: Some(&rendered.config),
                    recorded: Some((&sha, SCRIPT_REVISION))
                }
            ),
            ScriptDecision::Current
        );

        let mut edited = rendered.config.clone();
        edited["description"] = json!("My own words");
        assert_eq!(
            decide(
                rendered,
                &ScriptSituation {
                    current: Some(&edited),
                    recorded: Some((&sha, SCRIPT_REVISION))
                }
            ),
            ScriptDecision::LeaveEdited
        );
        // Not written by this controller as far as the record knows.
        assert_eq!(
            decide(
                rendered,
                &ScriptSituation {
                    current: Some(&edited),
                    recorded: None
                }
            ),
            ScriptDecision::LeaveEdited
        );
        // An unedited older revision is upgraded; an edited one is not.
        let mut older = rendered.config.clone();
        older["variables"]["echomuse_revision"] = json!(0);
        let older_sha = config_digest(&older).unwrap();
        assert_eq!(
            decide(
                rendered,
                &ScriptSituation {
                    current: Some(&older),
                    recorded: Some((&older_sha, 0))
                }
            ),
            ScriptDecision::Install
        );
        assert_eq!(
            decide(
                rendered,
                &ScriptSituation {
                    current: Some(&edited),
                    recorded: Some((&older_sha, 0))
                }
            ),
            ScriptDecision::LeaveEdited
        );
    }
}
