//! The fleet configuration, system settings and status routes over real HTTP (§3.4),
//! checked against what controller/static/dashboard.jsx sends and reads.

mod common;

use common::{Options, PASSWORD, RuntimeCall, TestServer};
use echomuse_api::{LiveDevice, SettingsApplied};
use echomuse_core::types::DeviceId;
use serde_json::{Value, json};

const KITCHEN: &str = "G0SYNTHETIC00001";
const HALLWAY: &str = "G0SYNTHETIC00002";

async fn server() -> (TestServer, String, String) {
    let server = TestServer::start(Options::default()).await;
    server.add_user("admin", "admin");
    server.add_user("viewer", "readonly");
    let admin = server.login("admin", PASSWORD).await;
    let viewer = server.login("viewer", PASSWORD).await;
    (server, admin, viewer)
}

/// dashboard.jsx loads `GET /api/global/config` into `globalConfig`, reads keys such as
/// `nearMissThreshold` and `wakeModel` from it (`effectiveConfig`), and its Settings
/// panel saves the whole object with `POST /api/global/config`, reading
/// `res.pushed_to.length` ("pushed live to n devices").
#[tokio::test]
async fn the_fleet_configuration_round_trips() {
    let (server, admin, viewer) = server().await;
    server.add_device(KITCHEN, Some("Kitchen"), true).await;
    server.add_device(HALLWAY, Some("Hallway"), true).await;
    server.runtimes.set_live(HALLWAY, LiveDevice::default());
    // Kitchen is connected: any session will do for the simulated runtimes.
    server.runtimes.set_live(
        KITCHEN,
        LiveDevice {
            esphome_port: None,
            session: Some(echomuse_api::LiveSession {
                ip: "192.0.2.30".parse().unwrap(),
                platform: echomuse_protocol::Platform::FireOs6,
                capabilities: echomuse_protocol::Capabilities::default(),
                tls: false,
                firmware_version: None,
                os_version: None,
                muted: None,
                volume: None,
                stats: None,
                installed_assets: None,
            }),
            health: Some(echomuse_api::LiveHealth::default()),
        },
    );

    let (status, global) = server.get("/api/global/config", Some(&viewer)).await;
    assert_eq!(status, 200);
    let config = global.as_object().unwrap();
    assert_eq!(config.len(), 40, "every setting key");
    assert_eq!(config["nearMissThreshold"], 0.17);
    assert_eq!(config["startupVolume"], 85);
    assert_eq!(
        config["screenWakePassword"],
        Value::Null,
        "hidden from read-only users"
    );
    let (_, global) = server.get("/api/global/config", Some(&admin)).await;
    assert_eq!(global["screenWakePassword"], "");

    let mut edit = global.as_object().unwrap().clone();
    edit.insert("duckDb".into(), json!(-24));
    let (status, body) = server
        .post(
            "/api/global/config",
            Value::Object(edit.clone()),
            Some(&viewer),
        )
        .await;
    assert_eq!((status, &body["code"]), (403, &json!("forbidden")));
    let (status, body) = server
        .post("/api/global/config", Value::Object(edit), Some(&admin))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"pushed_to": [KITCHEN]}));
    assert_eq!(
        server.runtimes.calls(),
        [RuntimeCall::SettingsApplied(SettingsApplied {
            changed: vec![DeviceId(KITCHEN.to_owned()), DeviceId(HALLWAY.to_owned())],
            new_session: vec![],
        })]
    );
    let (_, global) = server.get("/api/global/config", Some(&viewer)).await;
    assert_eq!(global["duckDb"], -24);

    let (status, body) = server
        .post(
            "/api/global/config",
            json!({"vadThreshold": 0.5}),
            Some(&admin),
        )
        .await;
    assert_eq!((status, &body["code"]), (400, &json!("removed_setting")));
    assert!(body["error"].as_str().unwrap().contains("vadThreshold"));
    let (status, body) = server
        .post(
            "/api/global/config",
            json!({"notASetting": 1}),
            Some(&admin),
        )
        .await;
    assert_eq!((status, &body["code"]), (400, &json!("unknown_setting")));
}

/// docs/configuration.md "System settings": both routes are administrator-only.
#[tokio::test]
async fn system_settings_are_for_administrators() {
    let (server, admin, viewer) = server().await;
    let (status, body) = server.get("/api/system/config", Some(&viewer)).await;
    assert_eq!((status, &body["code"]), (403, &json!("forbidden")));
    let (status, body) = server
        .patch(
            "/api/system/config",
            json!({"device_approval": "auto"}),
            Some(&viewer),
        )
        .await;
    assert_eq!((status, &body["code"]), (403, &json!("forbidden")));

    let (status, body) = server.get("/api/system/config", Some(&admin)).await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({"device_approval": "strict", "session_expiry_days": 30})
    );

    let (status, body) = server
        .patch(
            "/api/system/config",
            json!({"device_approval": "auto", "session_expiry_days": 7}),
            Some(&admin),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({"device_approval": "auto", "session_expiry_days": 7})
    );
    let stored: String = server
        .sqlite()
        .query_row(
            "SELECT value FROM system_config WHERE key = 'device_approval'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "auto");

    for (body, code) in [
        (json!({"github_repo": "someone/else"}), "unknown_setting"),
        (json!({"device_approval": "open"}), "invalid_setting"),
        (json!({"session_expiry_days": 0}), "invalid_setting"),
    ] {
        let (status, response) = server.patch("/api/system/config", body, Some(&admin)).await;
        assert_eq!((status, &response["code"]), (400, &json!(code)));
    }
    let (_, body) = server.get("/api/system/config", Some(&admin)).await;
    assert_eq!(
        body,
        json!({"device_approval": "auto", "session_expiry_days": 7})
    );
}

/// dashboard.jsx shows `status.controller_version` beside the title.
#[tokio::test]
async fn system_status_names_the_controller_version() {
    let (server, _, viewer) = server().await;
    let (status, body) = server.get("/api/system/status", Some(&viewer)).await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"controller_version": "v0.0.0-synthetic"}));
    let (status, body) = server.get("/api/ha/status", Some(&viewer)).await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"features": {}, "calendar_url": null}));
}
