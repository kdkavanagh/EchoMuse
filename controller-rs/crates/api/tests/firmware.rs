//! The firmware and provisioning routes over real HTTP, with simulated firmware delivery,
//! checked against what the shipped dashboard (controller/static/dashboard.jsx) sends and
//! reads.

mod common;

use common::{BUNDLED_BYTES, BUNDLED_VERSION, FirmwareCall, Options, PASSWORD, START, TestServer};
use echomuse_api::{FirmwareRefusal, UpdateStatus};
use echomuse_core::types::DeviceId;
use echomuse_protocol::Sha256;
use serde_json::json;

const KITCHEN: &str = "G0SYNTHETIC00001";

async fn signed_in() -> (TestServer, String, String) {
    let server = TestServer::start(Options::default()).await;
    server.add_user("admin", "admin");
    server.add_user("viewer", "readonly");
    let admin = server.login("admin", PASSWORD).await;
    let viewer = server.login("viewer", PASSWORD).await;
    server.add_device(KITCHEN, Some("Kitchen"), true).await;
    (server, admin, viewer)
}

fn kitchen() -> DeviceId {
    DeviceId(KITCHEN.to_owned())
}

#[tokio::test]
async fn the_bundled_firmware_is_reported_to_anyone_signed_in() {
    let (server, _, viewer) = signed_in().await;
    let (status, body) = server.get("/api/firmware", Some(&viewer)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({
            "version": BUNDLED_VERSION,
            "size": BUNDLED_BYTES.len(),
            "sha256": Sha256::of(BUNDLED_BYTES).as_str(),
        })
    );
    assert_eq!(server.get("/api/firmware", None).await.0, 401);
}

#[tokio::test]
async fn device_commands_answer_as_the_dashboard_reads_them() {
    let (server, admin, viewer) = signed_in().await;
    let path = |tail: &str| format!("/api/devices/{KITCHEN}/{tail}");

    let (status, body) = server.post(&path("update"), json!({}), Some(&admin)).await;
    assert_eq!(
        (status, &body["version"]),
        (200, &json!(BUNDLED_VERSION)),
        "{body}"
    );
    let (status, body) = server
        .post(&path("rollback"), json!({}), Some(&admin))
        .await;
    assert_eq!((status, &body["started"]), (200, &json!(true)), "{body}");
    let (status, body) = server
        .post(&path("update/queue"), json!({}), Some(&admin))
        .await;
    assert_eq!(
        (status, &body["update_queued_at"]),
        (200, &json!(START)),
        "{body}"
    );
    let (status, body) = server.delete(&path("update/queue"), Some(&admin)).await;
    assert_eq!((status, &body["cancelled"]), (200, &json!(true)), "{body}");
    let (status, body) = server.post(&path("debloat"), json!({}), Some(&admin)).await;
    assert_eq!((status, &body["started"]), (200, &json!(true)), "{body}");
    let (status, body) = server
        .post(&path("secure_link"), json!({}), Some(&admin))
        .await;
    assert_eq!((status, &body["installed"]), (200, &json!(true)), "{body}");
    assert_eq!(
        server.firmware.calls(),
        [
            FirmwareCall::Update(kitchen()),
            FirmwareCall::Rollback(kitchen()),
            FirmwareCall::Queue(kitchen()),
            FirmwareCall::CancelQueue(kitchen()),
            FirmwareCall::Debloat(kitchen()),
            FirmwareCall::SecureLink(kitchen()),
        ]
    );

    // Read-only users change nothing.
    for tail in [
        "update",
        "rollback",
        "update/queue",
        "debloat",
        "secure_link",
    ] {
        let (status, body) = server.post(&path(tail), json!({}), Some(&viewer)).await;
        assert_eq!(status, 403, "{tail}: {body}");
    }
    assert_eq!(server.firmware.calls().len(), 6);
}

#[tokio::test]
async fn refusals_carry_their_own_codes() {
    let (server, admin, _) = signed_in().await;
    let update = format!("/api/devices/{KITCHEN}/update");
    for (refusal, status, code) in [
        (FirmwareRefusal::NotConnected, 409, "not_connected"),
        (FirmwareRefusal::Busy, 409, "busy"),
        (FirmwareRefusal::AlreadyCurrent, 409, "already_current"),
        (
            FirmwareRefusal::NotApplicable("nothing in the other slot".to_owned()),
            409,
            "not_applicable",
        ),
        (FirmwareRefusal::UnknownDevice, 404, "not_found"),
        (FirmwareRefusal::TlsUnavailable, 503, "unavailable"),
        (
            FirmwareRefusal::Failed("synthetic".to_owned()),
            500,
            "internal",
        ),
    ] {
        server.firmware.refuse_with(Some(refusal.clone()));
        let (got, body) = server.post(&update, json!({}), Some(&admin)).await;
        assert_eq!(
            (got, &body["code"]),
            (status, &json!(code)),
            "{refusal:?}: {body}"
        );
        assert!(body["error"].is_string());
    }
}

#[tokio::test]
async fn deploy_reports_what_started_and_what_was_skipped() {
    let (server, admin, viewer) = signed_in().await;
    let (status, body) = server
        .post("/api/firmware/deploy", json!({}), Some(&admin))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({
            "version": BUNDLED_VERSION,
            "started": ["G0SYNTHETIC00001"],
            "skipped": [{"device_id": "G0SYNTHETIC00002", "reason": "offline"}],
        })
    );
    assert_eq!(
        server
            .post("/api/firmware/deploy", json!({}), Some(&viewer))
            .await
            .0,
        403
    );
}

#[tokio::test]
async fn the_device_object_shows_the_update_state() {
    let (server, admin, _) = signed_in().await;
    let (_, device) = server
        .get(&format!("/api/devices/{KITCHEN}"), Some(&admin))
        .await;
    assert_eq!(device["update_in_progress"], false);
    assert_eq!(device["update_error"], json!(null));
    server.firmware.set_status(
        KITCHEN,
        UpdateStatus {
            in_progress: false,
            error: Some("the transfer failed".to_owned()),
        },
    );
    let (_, device) = server
        .get(&format!("/api/devices/{KITCHEN}"), Some(&admin))
        .await;
    assert_eq!(device["update_error"], "the transfer failed");
}

#[tokio::test]
async fn the_wizard_gets_its_files_and_admins_only() {
    let (server, admin, viewer) = signed_in().await;
    let response = server
        .http
        .get(server.url("/api/provision/start_script"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; charset=utf-8"
    );
    assert!(
        response
            .text()
            .await
            .unwrap()
            .starts_with("#!/system/bin/sh")
    );

    let response = server
        .http
        .get(server.url("/api/provision/firmware"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-firmware-version"], BUNDLED_VERSION);
    assert_eq!(
        response.headers()["x-firmware-sha256"],
        Sha256::of(BUNDLED_BYTES).as_str()
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), BUNDLED_BYTES);

    let (status, lists) = server.get("/api/provision/crown_lists", Some(&admin)).await;
    assert_eq!(status, 200, "{lists}");
    assert_eq!(lists["hide"][0]["tier"], "1");
    assert_eq!(lists["mic_deny"]["packages"][0], "com.example.synthetic");

    let (status, creds) = server
        .post(
            "/api/provision/tls_credentials",
            json!({"device_id": KITCHEN}),
            Some(&admin),
        )
        .await;
    assert_eq!(status, 200, "{creds}");
    assert_eq!(creds["token"], "synthetic-token");
    let (status, body) = server
        .post(
            "/api/provision/tls_credentials",
            json!({"device_id": ""}),
            Some(&admin),
        )
        .await;
    assert_eq!((status, &body["code"]), (400, &json!("bad_request")));

    let (status, diagnostics) = server
        .post(
            "/api/provision/diagnostics",
            json!({"step": "configure_wifi", "error": "timeout", "probes": {"ip": "none"},
                   "transcript": ["a", "b"], "selected_ssid": null}),
            Some(&admin),
        )
        .await;
    assert_eq!(status, 200, "{diagnostics}");
    assert_eq!(diagnostics["step"], "configure_wifi");
    assert_eq!(diagnostics["bundled_firmware"]["version"], BUNDLED_VERSION);

    for path in [
        "/api/provision/start_script",
        "/api/provision/crown_lists",
        "/api/provision/firmware",
    ] {
        assert_eq!(server.get(path, Some(&viewer)).await.0, 403, "{path}");
    }
    assert_eq!(
        server
            .post(
                "/api/provision/tls_credentials",
                json!({"device_id": KITCHEN}),
                Some(&viewer)
            )
            .await
            .0,
        403
    );
}
