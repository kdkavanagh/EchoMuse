//! Credential redaction for HA bodies (§6.2 "Privacy").
//!
//! Every HA request and reply passes through [`redact()`] inside the recorder before it is
//! queued, so no credential reaches the write queue, let alone the disk. A value is replaced by
//! `[redacted]` when:
//!
//! - its key names a credential: any key that, lowercased with `_`, `-`, `.` and spaces
//!   removed, contains `token`, `password`, `passwd`, `secret`, `cookie`, `authorization`,
//!   `credential`, `apikey`, `privatekey`, `passphrase`, `authsig` or `signature`, or is
//!   exactly `auth`, `psk` or `sig` (the whole value is replaced, whatever its type);
//! - it is a string that looks like a credential wherever it appears: an HTTP
//!   `Authorization` value (`Bearer …`, `Basic …`), or a JSON Web Token (three base64url
//!   segments, the first starting `eyJ`, as HA's access tokens are);
//! - it is a query parameter inside a URL string whose name names a credential (HA's signed
//!   media URLs carry `authSig=`); only the parameter's value is replaced.

use serde_json::Value;

/// What a redacted value is replaced with.
pub(crate) const REDACTED: &str = "[redacted]";

/// Whether a key names a credential.
fn is_credential_key(key: &str) -> bool {
    let k: String = key
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | '.' | ' '))
        .flat_map(char::to_lowercase)
        .collect();
    const CONTAINS: [&str; 12] = [
        "token",
        "password",
        "passwd",
        "secret",
        "cookie",
        "authorization",
        "credential",
        "apikey",
        "privatekey",
        "passphrase",
        "authsig",
        "signature",
    ];
    const EXACT: [&str; 3] = ["auth", "psk", "sig"];
    CONTAINS.iter().any(|p| k.contains(p)) || EXACT.contains(&k.as_str())
}

fn is_jwt(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && s.starts_with("eyJ")
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'=')
        })
}

fn is_authorization_value(s: &str) -> bool {
    let lower = s.trim_start().to_ascii_lowercase();
    lower.starts_with("bearer ") || lower.starts_with("basic ")
}

/// A string with its credentials removed, and how many were.
fn redact_str(s: &str) -> (Option<String>, u32) {
    let trimmed = s.trim();
    if is_authorization_value(trimmed) || is_jwt(trimmed) {
        return (Some(REDACTED.to_owned()), 1);
    }
    let Some((base, query)) = s.split_once('?') else {
        return (None, 0);
    };
    let (query, fragment) = match query.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (query, None),
    };
    let mut count = 0;
    let params: Vec<String> = query
        .split('&')
        .map(|p| match p.split_once('=') {
            Some((k, v)) if is_credential_key(k) || is_jwt(v) => {
                count += 1;
                format!("{k}={REDACTED}")
            }
            _ => p.to_owned(),
        })
        .collect();
    if count == 0 {
        return (None, 0);
    }
    let mut out = format!("{base}?{}", params.join("&"));
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    (Some(out), count)
}

/// Removes every credential from `value`, returning the cleaned copy and how many values were
/// replaced.
pub(crate) fn redact(value: &Value) -> (Value, u32) {
    let mut out = value.clone();
    let n = redact_in_place(&mut out);
    (out, n)
}

fn redact_in_place(value: &mut Value) -> u32 {
    match value {
        Value::Object(map) => {
            let mut n = 0;
            for (k, v) in map.iter_mut() {
                if is_credential_key(k) {
                    if *v != Value::Null {
                        *v = Value::String(REDACTED.to_owned());
                        n += 1;
                    }
                } else {
                    n += redact_in_place(v);
                }
            }
            n
        }
        Value::Array(items) => items.iter_mut().map(redact_in_place).sum(),
        Value::String(s) => match redact_str(s) {
            (Some(clean), n) => {
                *s = clean;
                n
            }
            (None, _) => 0,
        },
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

/// Removes credentials from free text, such as an error message.
pub(crate) fn redact_text(s: &str) -> String {
    match redact_str(s) {
        (Some(clean), _) => clean,
        (None, _) => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn credential_keys_at_any_depth_are_replaced_whole() {
        let body = json!({
            "type": "auth",
            "access_token": "abc",
            "data": {
                "Set-Cookie": ["a=b"],
                "headers": { "Authorization": "x" },
                "client_secret": { "nested": 1 },
                "refreshToken": "r",
                "wifi_password": "p",
                "kiosk-token": "k",
                "keep": "light.kitchen",
                "token_type": null,
            },
            "items": [ { "api_key": 5 } ],
        });
        let (clean, n) = redact(&body);
        assert_eq!(n, 8);
        assert_eq!(clean["access_token"], REDACTED);
        assert_eq!(clean["data"]["Set-Cookie"], REDACTED);
        assert_eq!(clean["data"]["headers"]["Authorization"], REDACTED);
        assert_eq!(clean["data"]["client_secret"], REDACTED);
        assert_eq!(clean["data"]["refreshToken"], REDACTED);
        assert_eq!(clean["data"]["wifi_password"], REDACTED);
        assert_eq!(clean["data"]["kiosk-token"], REDACTED);
        assert_eq!(clean["items"][0]["api_key"], REDACTED);
        // Not credentials: kept as they were.
        assert_eq!(clean["type"], "auth");
        assert_eq!(clean["data"]["keep"], "light.kitchen");
        assert_eq!(clean["data"]["token_type"], Value::Null);
    }

    #[test]
    fn credential_shaped_strings_are_replaced_wherever_they_are() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJpc3MiOiJ4In0.c2lnbmF0dXJl";
        let body = json!({
            "text": "turn off the lights",
            "note": format!("Bearer {jwt}"),
            "list": [jwt, "plain"],
            "url": "http://ha:8123/api/tts_proxy/a.mp3?authSig=abc.def&lang=en#t",
        });
        let (clean, n) = redact(&body);
        assert_eq!(n, 3);
        assert_eq!(clean["text"], "turn off the lights");
        assert_eq!(clean["note"], REDACTED);
        assert_eq!(clean["list"][0], REDACTED);
        assert_eq!(clean["list"][1], "plain");
        assert_eq!(
            clean["url"],
            "http://ha:8123/api/tts_proxy/a.mp3?authSig=[redacted]&lang=en#t"
        );
    }

    #[test]
    fn text_without_credentials_is_untouched() {
        let (clean, n) = redact(&json!("what time is it? it's late"));
        assert_eq!(n, 0);
        assert_eq!(clean, json!("what time is it? it's late"));
    }
}
