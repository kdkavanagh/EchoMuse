//! `GET /api/support/bundle` (admin): a sanitised diagnostics file to attach to a report
//! (spec §4.11), which the dashboard saves as `echomuse-support-<time>.json`.
//!
//! It holds what the dashboard itself shows, gathered in one JSON object:
//!
//! | Key | What |
//! |---|---|
//! | `generated_at` | Unix seconds |
//! | `controller_version`, `bundled_firmware` | what runs, and what devices are given |
//! | `system` | `{device_approval, session_expiry_days}` |
//! | `home_assistant` | the provisioning status (`/api/ha/status`) |
//! | `fleet_config` | the fleet's value of every setting |
//! | `devices` | every device object, as a read-only user sees it |
//! | `wake_models` | the wake-model registry (`/api/wake_models`) |
//! | `device_logs` | each device's newest [`LOG_LINES`] log lines, newest first |
//! | `redacted` | what was left out or blanked |
//!
//! Secrets are never read into it: device and Kiosk Satellite tokens, user password
//! hashes and sessions, the Home Assistant token, TLS keys and the ESPHome keys stay in
//! the database and on disk. What is read is then sanitised twice over. Every object key
//! that names a secret (`token`, `password`, `secret`, `psk`, `cookie`, `authorization`,
//! `key` alone or as a word of the key, such as `private_key`) has its text value replaced
//! with `"[redacted]"`; `kioskTokenSet`, which only says whether one is set, keeps its
//! true or false. In every text, a URL's user name and password are removed, and a value
//! written after one of those words (`token=…`, `"password": "…"`) is replaced too, so a
//! device log line that printed one does not carry it out.

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
    routing::get,
};
use echomuse_db::ReadQuery;
use rusqlite::Connection;
use serde_json::{Map, Value, json};

use crate::{
    auth::{AdminUser, Role},
    devices::{all_views, hide_secrets},
    error::ApiError,
    state::ApiState,
    system::read_system,
    wake_models::model_json,
};

/// Log lines kept per device.
pub const LOG_LINES: usize = 200;
/// What a blanked value becomes.
pub const REDACTED: &str = "[redacted]";

pub(crate) fn routes() -> Router<ApiState> {
    Router::new().route("/api/support/bundle", get(bundle))
}

/// Each device's newest log lines, newest first.
struct RecentLogs {
    per_device: usize,
}

impl ReadQuery for RecentLogs {
    type Output = Map<String, Value>;
    type Error = rusqlite::Error;

    fn read(self, conn: &Connection) -> rusqlite::Result<Map<String, Value>> {
        let devices: Vec<String> = conn
            .prepare_cached("SELECT device_id FROM devices ORDER BY device_id")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut statement = conn.prepare_cached(
            "SELECT ts, level, source, message FROM device_logs WHERE device_id = ?1 \
             ORDER BY id DESC LIMIT ?2",
        )?;
        let mut out = Map::new();
        for device in devices {
            let lines: Vec<Value> = statement
                .query_map(rusqlite::params![device, self.per_device as i64], |row| {
                    Ok(json!({
                        "ts": row.get::<_, Option<i64>>(0)?,
                        "level": row.get::<_, Option<String>>(1)?,
                        "source": row.get::<_, Option<String>>(2)?,
                        "message": row.get::<_, Option<String>>(3)?,
                    }))
                })?
                .collect::<rusqlite::Result<_>>()?;
            out.insert(device, Value::Array(lines));
        }
        Ok(out)
    }
}

/// The words that make an object key a secret's.
const SECRET_WORDS: &[&str] = &[
    "token",
    "password",
    "passwd",
    "secret",
    "psk",
    "cookie",
    "authorization",
];

/// Splits a key into lowercase words at `_`, `-` and lower-to-upper case changes.
fn key_words(key: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut previous_lower = false;
    for c in key.chars() {
        if c == '_' || c == '-' || c == ' ' {
            words.push(std::mem::take(&mut word));
            previous_lower = false;
            continue;
        }
        if c.is_uppercase() && previous_lower {
            words.push(std::mem::take(&mut word));
        }
        previous_lower = c.is_lowercase() || c.is_ascii_digit();
        word.extend(c.to_lowercase());
    }
    words.push(word);
    words.retain(|w| !w.is_empty());
    words
}

/// Whether `key` names a secret.
fn secret_key(key: &str) -> bool {
    let words = key_words(key);
    words
        .iter()
        .any(|w| w == "key" || SECRET_WORDS.iter().any(|s| w.contains(s)))
}

/// `text` without the user name and password of any URL in it.
fn without_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '"' | '\''))
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str(REDACTED);
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// `text` with the value after each secret word (`token=abc`, `password: abc`,
/// `"psk":"abc"`) replaced. Matching is on ASCII bytes, so offsets never fall inside a
/// character.
fn without_inline_secrets(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let lower = lower.as_bytes();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let Some(word) = SECRET_WORDS
            .iter()
            .find(|w| lower[i..].starts_with(w.as_bytes()))
        else {
            i += 1;
            continue;
        };
        let mut j = i + word.len();
        // The rest of the word (`token_id`, `passwords`) and closing quotes.
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        while j < bytes.len() && matches!(bytes[j], b'"' | b'\'') {
            j += 1;
        }
        let separator = j;
        while j < bytes.len() && matches!(bytes[j], b' ' | b'\t') {
            j += 1;
        }
        if j >= bytes.len() || !matches!(bytes[j], b'=' | b':') {
            i = separator.max(i + 1);
            continue;
        }
        j += 1;
        while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'"' | b'\'') {
            j += 1;
        }
        let start = j;
        while j < bytes.len()
            && !matches!(
                bytes[j],
                b' ' | b'\t' | b'\n' | b'"' | b'\'' | b',' | b';' | b'&' | b'}'
            )
        {
            j += 1;
        }
        if j > start {
            out.push_str(&text[copied..start]);
            out.push_str(REDACTED);
            copied = j;
        }
        i = j.max(i + 1);
    }
    out.push_str(&text[copied..]);
    out
}

fn sanitise_text(text: &str) -> String {
    without_inline_secrets(&without_userinfo(text))
}

/// Blanks every secret in `value`, in place.
pub(crate) fn sanitise(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                if secret_key(key)
                    && matches!(
                        value,
                        Value::String(_) | Value::Object(_) | Value::Array(_) | Value::Number(_)
                    )
                {
                    *value = Value::String(REDACTED.to_owned());
                } else {
                    sanitise(value);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(sanitise),
        Value::String(text) => *text = sanitise_text(text),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

async fn bundle(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
) -> Result<Response, ApiError> {
    let generated_at = crate::clock::unix_now(state.clock())?.0;
    let system = read_system(&state).await?;
    let devices = all_views(&state, Role::Readonly).await?;
    let logs = state
        .db()
        .read(RecentLogs {
            per_device: LOG_LINES,
        })
        .await?;
    let snapshot = state.settings().snapshot();
    let mut fleet = match serde_json::to_value(&**snapshot.fleet()) {
        Ok(Value::Object(fleet)) => fleet,
        _ => {
            return Err(ApiError::internal(
                "The fleet configuration could not be read",
            ));
        }
    };
    hide_secrets(&mut fleet, Role::Readonly);
    let models: Vec<Value> = snapshot
        .registry()
        .models()
        .map(|model| model_json(model, &snapshot))
        .collect();
    let mut body = json!({
        "generated_at": generated_at,
        "controller_version": state.controller_version(),
        "bundled_firmware": state.firmware().bundled(),
        "system": system,
        "home_assistant": state.ha_status(),
        "fleet_config": fleet,
        "devices": devices.iter().map(|view| Value::Object(view.fields().clone())).collect::<Vec<_>>(),
        "wake_models": {
            "active": snapshot.fleet().wakeword.wake_model.0.as_str(),
            "models": models,
        },
        "device_logs": logs,
    });
    sanitise(&mut body);
    if let Value::Object(object) = &mut body {
        object.insert(
            "redacted".to_owned(),
            json!(
                "Never included: device and Kiosk Satellite tokens, user password hashes and \
                 sessions, the Home Assistant token, TLS keys, ESPHome keys. Blanked: every \
                 field or text value named as a secret, and the user name and password of \
                 every URL."
            ),
        );
    }
    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"echomuse-support.json\""),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_keys_are_recognised_by_their_words() {
        for key in [
            "token",
            "kiosk_token",
            "kioskToken",
            "screenWakePassword",
            "esphome_noise_psk",
            "private_key",
            "Authorization",
            "session_cookie",
            "clientSecret",
        ] {
            assert!(secret_key(key), "{key}");
        }
        for key in ["kioskTokenSet", "wakeModel", "keyboard", "monkey", "label"] {
            // kioskTokenSet names a secret's word but only holds a bool, which stays.
            assert_eq!(secret_key(key), key == "kioskTokenSet", "{key}");
        }
    }

    #[test]
    fn values_are_blanked_but_flags_kept() {
        let mut value = json!({
            "token": "abc",
            "kioskTokenSet": true,
            "config": {"screenWakePassword": "hunter22", "screenWakeUrl": "http://admin:pw@kiosk.local:2323/x"},
            "nested": [{"psk": "wifi-pass"}],
            "message": "connected with token=deadbeef and \"password\": \"hunter22\", ok",
            "count": 3,
        });
        sanitise(&mut value);
        assert_eq!(value["token"], REDACTED);
        assert_eq!(value["kioskTokenSet"], true);
        assert_eq!(value["config"]["screenWakePassword"], REDACTED);
        assert_eq!(
            value["config"]["screenWakeUrl"],
            "http://[redacted]@kiosk.local:2323/x"
        );
        assert_eq!(value["nested"][0]["psk"], REDACTED);
        assert_eq!(
            value["message"],
            "connected with token=[redacted] and \"password\": \"[redacted]\", ok"
        );
        assert_eq!(value["count"], 3);
        let text = value.to_string();
        for secret in ["abc\"", "hunter22", "admin:pw", "wifi-pass", "deadbeef"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    #[test]
    fn plain_text_is_left_alone() {
        assert_eq!(
            sanitise_text("tokens are fine here"),
            "tokens are fine here"
        );
        assert_eq!(
            sanitise_text("http://kiosk.local/x"),
            "http://kiosk.local/x"
        );
        assert_eq!(sanitise_text("ünïcode token=x"), "ünïcode token=[redacted]");
    }
}
