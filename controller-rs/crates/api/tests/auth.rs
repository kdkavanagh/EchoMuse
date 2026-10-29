//! Setup, sign-in, sessions and passwords over real HTTP (§3.4).

mod common;

use std::time::Duration;

use common::{DAY, Options, PASSWORD, START, TestServer};
use echomuse_api::ThrottleConfig;
use serde_json::json;

fn is_session_token(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[tokio::test]
async fn setup_works_once_with_the_printed_token() {
    let server = TestServer::start(Options::default()).await;
    let setup_token = server.setup_token.clone().unwrap();

    let (status, body) = server.get("/api/system/setup-state", None).await;
    assert_eq!((status, body), (200, json!({"needs_setup": true})));

    let (status, body) = server
        .post(
            "/api/setup",
            json!({"token": "not-the-token", "username": "admin", "password": "long enough"}),
            None,
        )
        .await;
    assert_eq!(status, 403);
    assert_eq!(body["code"], "invalid_setup_token");
    assert!(body["error"].is_string());

    let (status, body) = server
        .post(
            "/api/setup",
            json!({"token": setup_token, "username": "admin", "password": "short"}),
            None,
        )
        .await;
    assert_eq!((status, &body["code"]), (400, &json!("invalid_password")));

    let (status, body) = server
        .post(
            "/api/setup",
            json!({"token": format!(" {setup_token} "), "username": " admin ", "password": "long enough"}),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["role"], "admin");
    let token = body["token"].as_str().unwrap().to_owned();
    assert!(is_session_token(&token));

    // Stored the way existing installations store them.
    let conn = server.sqlite();
    let (username, hash, role, created_at): (String, String, String, i64) = conn
        .query_row(
            "SELECT username, password_hash, role, created_at FROM users",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (username.as_str(), role.as_str(), created_at),
        ("admin", "admin", START)
    );
    assert!(hash.starts_with("$2b$04$") && hash.len() == 60, "{hash}");
    let (stored, created, expires): (String, i64, i64) = conn
        .query_row(
            "SELECT token, created_at, expires_at FROM sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (stored, created, expires),
        (token.clone(), START, START + 30 * DAY)
    );

    let (status, body) = server.get("/api/system/setup-state", None).await;
    assert_eq!((status, body), (200, json!({"needs_setup": false})));
    let (status, body) = server.get("/api/auth/me", Some(&token)).await;
    assert_eq!(
        (status, body),
        (200, json!({"username": "admin", "role": "admin"}))
    );

    let (status, body) = server
        .post(
            "/api/setup",
            json!({"token": setup_token, "username": "second", "password": "long enough"}),
            None,
        )
        .await;
    assert_eq!((status, &body["code"]), (409, &json!("setup_complete")));
    let users: i64 = conn
        .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
        .unwrap();
    assert_eq!(users, 1);
}

#[tokio::test]
async fn setup_is_refused_when_a_user_exists_or_no_token_is_held() {
    let server = TestServer::start(Options::default()).await;
    server.add_user("existing", "admin");
    let token = server.setup_token.clone().unwrap();
    let (status, body) = server
        .post(
            "/api/setup",
            json!({"token": token, "username": "admin", "password": "long enough"}),
            None,
        )
        .await;
    assert_eq!((status, &body["code"]), (409, &json!("setup_complete")));

    let server = TestServer::start(Options {
        with_setup_token: false,
        ..Options::default()
    })
    .await;
    let (status, body) = server
        .post(
            "/api/setup",
            json!({"token": "", "username": "admin", "password": "long enough"}),
            None,
        )
        .await;
    assert_eq!((status, &body["code"]), (503, &json!("setup_unavailable")));
}

#[tokio::test]
async fn login_succeeds_and_fails_in_the_shape_the_landing_page_reads() {
    let server = TestServer::start(Options::default()).await;
    server.add_user("admin", "admin");
    server.add_user("viewer", "readonly");

    for (username, password) in [("admin", "wrong password"), ("nobody", PASSWORD)] {
        let (status, body) = server
            .post(
                "/api/auth/login",
                json!({"username": username, "password": password}),
                None,
            )
            .await;
        assert_eq!(status, 401);
        assert_eq!(body["code"], "invalid_credentials");
        assert_eq!(body["error"], "Invalid username or password");
    }

    let (status, body) = server
        .post(
            "/api/auth/login",
            json!({"username": "viewer", "password": PASSWORD}),
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["role"], "readonly");
    let token = body["token"].as_str().unwrap();
    assert!(is_session_token(token));
    let (status, body) = server.get("/api/auth/me", Some(token)).await;
    assert_eq!(
        (status, body),
        (200, json!({"username": "viewer", "role": "readonly"}))
    );
}

/// Hashes made outside this crate verify: one from Python's pyca `bcrypt` 5.0.0
/// (`hashpw(..., gensalt(12, prefix=b"2b"))`, the cost real installations use) and one
/// from the `bcrypt` crate at cost 5.
#[tokio::test]
async fn existing_2b_hashes_verify() {
    const EXTERNAL: &str = "$2b$12$a7uXSindGfl6ECu2k2Neeewa7gf5yqUySIeyL7uLViBFSIZGQzwy2";
    const EXTERNAL_PASSWORD: &str = "synthetic fixture passphrase";
    let crate_hash = bcrypt::hash_with_salt("crate fixture passphrase", 5, [42; 16])
        .unwrap()
        .format_for_version(bcrypt::Version::TwoB);
    assert!(crate_hash.starts_with("$2b$05$"));

    let server = TestServer::start(Options::default()).await;
    server.add_user_with_hash("python", "admin", EXTERNAL);
    server.add_user_with_hash("rust", "readonly", &crate_hash);
    server.login("python", EXTERNAL_PASSWORD).await;
    server.login("rust", "crate fixture passphrase").await;
    let (status, _) = server
        .post(
            "/api/auth/login",
            json!({"username": "python", "password": "synthetic fixture passphrasf"}),
            None,
        )
        .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn sessions_last_session_expiry_days() {
    let server = TestServer::start(Options::default()).await;
    server.add_user("admin", "admin");
    server.set_system("session_expiry_days", "2");
    let token = server.login("admin", PASSWORD).await;
    let expires: i64 = server
        .sqlite()
        .query_row(
            "SELECT expires_at FROM sessions WHERE token = ?1",
            [&token],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(expires, START + 2 * DAY);

    server.clock.set(START + 2 * DAY - 1);
    assert_eq!(server.get("/api/auth/me", Some(&token)).await.0, 200);
    server.clock.set(START + 2 * DAY);
    let (status, body) = server.get("/api/auth/me", Some(&token)).await;
    assert_eq!((status, &body["code"]), (401, &json!("not_authenticated")));

    // An unusable setting falls back to the documented 30 days, and a new sign-in
    // clears expired sessions.
    server.set_system("session_expiry_days", "soon");
    let now = server.clock.now();
    let token = server.login("admin", PASSWORD).await;
    let rows: Vec<(String, i64)> = server
        .sqlite()
        .prepare("SELECT token, expires_at FROM sessions")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, vec![(token, now + 30 * DAY)]);
}

#[tokio::test]
async fn logout_ends_only_that_session() {
    let server = TestServer::start(Options::default()).await;
    server.add_user("admin", "admin");
    let first = server.login("admin", PASSWORD).await;
    let second = server.login("admin", PASSWORD).await;
    let (status, body) = server
        .post("/api/auth/logout", json!({}), Some(&first))
        .await;
    assert_eq!((status, body), (200, json!({"ok": true})));
    assert_eq!(server.get("/api/auth/me", Some(&first)).await.0, 401);
    assert_eq!(server.get("/api/auth/me", Some(&second)).await.0, 200);
    assert_eq!(
        server
            .post("/api/auth/logout", json!({}), Some(&first))
            .await
            .0,
        401
    );
}

#[tokio::test]
async fn change_password_keeps_the_asking_session_and_ends_the_others() {
    let server = TestServer::start(Options::default()).await;
    server.add_user("admin", "admin");
    let other_user = server.add_user("viewer", "readonly");
    let asking = server.login("admin", PASSWORD).await;
    let elsewhere = server.login("admin", PASSWORD).await;
    let viewer = server.login("viewer", PASSWORD).await;

    let (status, body) = server
        .post(
            "/api/auth/change-password",
            json!({"current_password": "not it", "new_password": "a new password"}),
            Some(&asking),
        )
        .await;
    // Not 401: the dashboard would treat that as signed out.
    assert_eq!(
        (status, &body["code"]),
        (403, &json!("invalid_credentials"))
    );
    assert_eq!(body["error"], "Current password is incorrect");

    let (status, body) = server
        .post(
            "/api/auth/change-password",
            json!({"current_password": PASSWORD, "new_password": "short"}),
            Some(&asking),
        )
        .await;
    assert_eq!((status, &body["code"]), (400, &json!("invalid_password")));

    let (status, body) = server
        .post(
            "/api/auth/change-password",
            json!({"current_password": PASSWORD, "new_password": "a new password"}),
            Some(&asking),
        )
        .await;
    assert_eq!((status, body), (200, json!({"ok": true})));

    assert_eq!(server.get("/api/auth/me", Some(&asking)).await.0, 200);
    assert_eq!(server.get("/api/auth/me", Some(&elsewhere)).await.0, 401);
    assert_eq!(
        server.get("/api/auth/me", Some(&viewer)).await.0,
        200,
        "other users keep theirs"
    );
    let (status, _) = server
        .post(
            "/api/auth/login",
            json!({"username": "admin", "password": PASSWORD}),
            None,
        )
        .await;
    assert_eq!(status, 401);
    server.login("admin", "a new password").await;
    let hash: String = server
        .sqlite()
        .query_row(
            "SELECT password_hash FROM users WHERE username = 'admin'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(hash.starts_with("$2b$04$"));

    // A readonly user may change their own password.
    let (status, _) = server
        .post(
            "/api/auth/change-password",
            json!({"current_password": PASSWORD, "new_password": "viewer's new one"}),
            Some(&viewer),
        )
        .await;
    assert_eq!(status, 200);
    let viewer_sessions: i64 = server
        .sqlite()
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE user_id = ?1",
            [other_user],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(viewer_sessions, 1);
}

#[tokio::test]
async fn repeated_failures_are_throttled_per_address() {
    let server = TestServer::start(Options {
        throttle: ThrottleConfig {
            free_failures: 2,
            first_delay: Duration::from_secs(60),
            ..ThrottleConfig::default()
        },
        ..Options::default()
    })
    .await;
    server.add_user("admin", "admin");
    let wrong = json!({"username": "admin", "password": "wrong password"});
    for _ in 0..3 {
        assert_eq!(
            server.post("/api/auth/login", wrong.clone(), None).await.0,
            401
        );
    }
    let response = server
        .http
        .post(server.url("/api/auth/login"))
        .header("content-type", "application/json")
        .body(json!({"username": "admin", "password": PASSWORD}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 429);
    let retry_after: u64 = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry_after), "{retry_after}");
    let (_, body) = common::status_json(response).await;
    assert_eq!(body["code"], "too_many_attempts");

    // Setup-token guesses count against the same address.
    let (status, _) = server
        .post(
            "/api/setup",
            json!({"token": "guess", "username": "x", "password": "long enough"}),
            None,
        )
        .await;
    assert_eq!(status, 429);
}

#[tokio::test]
async fn malformed_requests_get_the_error_shape() {
    let server = TestServer::start(Options::default()).await;
    let response = server
        .http
        .post(server.url("/api/auth/login"))
        .body(r#"{"username":"a","password":"b"}"#)
        .send()
        .await
        .unwrap();
    let (status, body) = common::status_json(response).await;
    assert_eq!(
        (status, &body["code"]),
        (415, &json!("unsupported_media_type"))
    );

    let response = server
        .http
        .post(server.url("/api/auth/login"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    let (status, body) = common::status_json(response).await;
    assert_eq!((status, &body["code"]), (400, &json!("bad_request")));

    let (status, body) = server
        .post("/api/auth/login", json!({"username": "only"}), None)
        .await;
    assert_eq!((status, &body["code"]), (400, &json!("bad_request")));

    let (status, body) = server.get("/api/no/such/route", None).await;
    assert_eq!((status, &body["code"]), (404, &json!("not_found")));
    let (status, body) = server.get("/api/auth/login", None).await;
    assert_eq!((status, &body["code"]), (405, &json!("method_not_allowed")));

    let (status, body) = server.get("/api/auth/me", None).await;
    assert_eq!((status, &body["code"]), (401, &json!("not_authenticated")));
    let response = server
        .http
        .get(server.url("/api/auth/me"))
        .header("authorization", "Basic YWRtaW46eA==")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    // A token in the query string of an ordinary request is not a credential.
    server.add_user("admin", "admin");
    let token = server.login("admin", PASSWORD).await;
    let (status, _) = server
        .get(&format!("/api/auth/me?token={token}"), None)
        .await;
    assert_eq!(status, 401);
}
