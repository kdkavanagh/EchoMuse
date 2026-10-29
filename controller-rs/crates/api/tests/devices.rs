//! The device routes over real HTTP (§3.4, §4.10), checked against what the shipped
//! dashboard (controller/static/dashboard.jsx) sends and reads.

mod common;

use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
    time::Duration,
};

use common::{Options, PASSWORD, RuntimeCall, START, TestServer};
use echomuse_api::{LiveDevice, LiveSession, LiveVolume, RuntimeError, SettingsApplied};
use echomuse_core::types::DeviceId;
use echomuse_protocol::{Capabilities, Platform};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const KITCHEN: &str = "G0SYNTHETIC00001";
const HALLWAY: &str = "G0SYNTHETIC00002";
const NEW: &str = "G0SYNTHETIC00003";
const WAIT: Duration = Duration::from_secs(10);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Fleet {
    server: TestServer,
    admin: String,
    viewer: String,
}

/// Two approved devices (the kitchen one connected as a current Dot), one pending, an
/// administrator and a read-only user.
async fn fleet() -> Fleet {
    let server = TestServer::start(Options::default()).await;
    server.add_user("admin", "admin");
    server.add_user("viewer", "readonly");
    let admin = server.login("admin", PASSWORD).await;
    let viewer = server.login("viewer", PASSWORD).await;
    server.add_device(KITCHEN, Some("Kitchen"), true).await;
    server.add_device(HALLWAY, Some("Hallway"), true).await;
    server.add_device(NEW, None, false).await;
    server.runtimes.set_live(KITCHEN, connected_dot());
    server.runtimes.set_live(HALLWAY, LiveDevice::default());
    Fleet {
        server,
        admin,
        viewer,
    }
}

fn connected_dot() -> LiveDevice {
    let names = [
        "audio_timeline_v1",
        "uplink_leases_v1",
        "device_wake_v1",
        "render_reference_v1",
        "render_progress_v1",
        "focus_leases_v1",
        "alert_cache_v1",
        "turn_protocol_v1",
        "leds",
        "led_anim",
        "buttons",
        "button_hold",
        "wifi_control",
        "ble_proxy",
        "privacy_mute",
        "open_rules_v1",
        "near_miss_v1",
        "a_future_capability",
    ];
    let mut stats = Map::new();
    stats.insert("cpuPct".into(), json!(12.5));
    LiveDevice {
        esphome_port: Some(16001),
        session: Some(LiveSession {
            ip: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 30)),
            platform: Platform::FireOs6,
            capabilities: Capabilities::from_names(names),
            tls: true,
            firmware_version: Some("fw-live".to_owned()),
            os_version: Some("NS6301".to_owned()),
            muted: Some(false),
            volume: Some(LiveVolume::Level(127)),
            stats: Some(Arc::new(stats)),
            installed_assets: None,
        }),
        health: Some(echomuse_api::LiveHealth::default()),
    }
}

fn device<'a>(list: &'a Value, id: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|d| d["device_id"] == id)
        .unwrap_or_else(|| panic!("{id} is listed"))
}

async fn events(server: &TestServer, token: &str) -> Socket {
    let (mut socket, _) = connect_async(server.ws_url(&format!("/api/events?token={token}")))
        .await
        .unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "snapshot");
    socket
}

async fn next_json(socket: &mut Socket) -> Value {
    let message = tokio::time::timeout(WAIT, socket.next())
        .await
        .expect("an event arrives")
        .unwrap()
        .unwrap();
    match message {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected text, got {other:?}"),
    }
}

/// dashboard.jsx reads the fleet from `GET /api/devices` (App: `setDevices(devs)`,
/// `refreshDevices`) and each device's fields across the page: `connected`,
/// `approved` (pending = `!d.approved`), `label`, `firmware_ver`, `platform`
/// (`PLATFORM_LABEL`), `capabilities` and `capability_gaps` (`liveCapabilities`),
/// `linkTls`, `volume` (×100 %), `volume_gap`, `muted`, `stats`, `esphome_port`,
/// `config` and `config_sections` (`effectiveConfig`), `last_seen` (`relTime`, seconds),
/// `collectMode`, `ambientMode`, `kioskTokenSet`.
#[tokio::test]
async fn the_device_list_has_the_fields_the_dashboard_reads() {
    let Fleet { server, viewer, .. } = fleet().await;
    let (status, list) = server.get("/api/devices", Some(&viewer)).await;
    assert_eq!(status, 200);
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["device_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [KITCHEN, HALLWAY, NEW]);

    let kitchen = device(&list, KITCHEN);
    assert_eq!(kitchen["label"], "Kitchen");
    assert_eq!(kitchen["approved"], true);
    assert_eq!(kitchen["connected"], true);
    assert_eq!(kitchen["platform"], "fireos6");
    assert_eq!(kitchen["linkTls"], true);
    assert_eq!(kitchen["volume"], 1.0);
    assert_eq!(kitchen["volume_gap"], Value::Null);
    assert_eq!(kitchen["muted"], false);
    assert_eq!(kitchen["stats"], json!({"cpuPct": 12.5}));
    // The live session's address and versions, over the stored ones.
    assert_eq!(kitchen["ip"], "192.0.2.30");
    assert_eq!(kitchen["firmware_ver"], "fw-live");
    assert_eq!(kitchen["os_version"], "NS6301");
    assert_eq!(kitchen["esphome_port"], 16001);
    assert_eq!(kitchen["last_seen"], START);
    assert_eq!(kitchen["first_seen"], START - common::DAY);
    assert_eq!(kitchen["firmware_previous"], Value::Null);
    assert_eq!(kitchen["update_queued_at"], Value::Null);
    assert_eq!(kitchen["collectMode"], false);
    assert_eq!(kitchen["ambientMode"], false);
    assert_eq!(kitchen["kioskTokenSet"], true);
    // Unknown names are listed as announced; dashboard.jsx `capabilities.names.includes`.
    let names = kitchen["capabilities"].as_array().unwrap();
    assert!(names.contains(&json!("wifi_control")));
    assert!(names.contains(&json!("a_future_capability")));
    // A Dot has no screen: dashboard.jsx `capabilityGap` shows these first.
    assert_eq!(
        kitchen["capability_gaps"],
        json!({"screen_wake": "no screen on this device",
               "turn_view_v1": "no screen on this device",
               "kiosk_plugin": "no screen on this device"})
    );
    // Following the fleet: `config_sections` [] and only the device-state key.
    assert_eq!(kitchen["config_sections"], json!([]));
    assert_eq!(kitchen["use_global_config"], true);
    assert_eq!(kitchen["config"], json!({"startupVolume": 85}));

    let hallway = device(&list, HALLWAY);
    assert_eq!(hallway["connected"], false);
    for key in [
        "platform",
        "capabilities",
        "capability_gaps",
        "volume",
        "muted",
        "stats",
        "linkTls",
    ] {
        assert_eq!(hallway[key], Value::Null, "{key} is unknown while offline");
    }
    assert_eq!(hallway["ip"], "192.0.2.20");
    assert_eq!(hallway["firmware_ver"], "fw-synthetic");

    let pending = device(&list, NEW);
    assert_eq!(pending["approved"], false);
    assert_eq!(pending["label"], Value::Null);
    assert_eq!(pending["connected"], false);

    // No credential leaves the controller.
    let text = list.to_string();
    assert!(!text.contains("synthetic-link-token"));
    assert!(!text.contains("synthetic-kiosk-token"));
}

#[tokio::test]
async fn one_device_pending_devices_and_unknown_ids() {
    let Fleet { server, viewer, .. } = fleet().await;
    let (status, list) = server.get("/api/devices", Some(&viewer)).await;
    assert_eq!(status, 200);
    let (status, one) = server
        .get(&format!("/api/devices/{KITCHEN}"), Some(&viewer))
        .await;
    assert_eq!(status, 200);
    assert_eq!(&one, device(&list, KITCHEN));

    let (status, pending) = server.get("/api/devices/pending", Some(&viewer)).await;
    assert_eq!(status, 200);
    assert_eq!(pending, json!([device(&list, NEW)]));

    let (status, body) = server
        .get("/api/devices/G0NOTREGISTERED0", Some(&viewer))
        .await;
    assert_eq!((status, &body["code"]), (404, &json!("not_found")));
    let (status, body) = server.get("/api/devices", None).await;
    assert_eq!((status, &body["code"]), (401, &json!("not_authenticated")));
}

/// Changes need an administrator (§3.4); dashboard.jsx shows `e.error` from the
/// `{error, code}` body (`alert(e.error || …)`).
#[tokio::test]
async fn readonly_users_cannot_change_devices() {
    let Fleet { server, viewer, .. } = fleet().await;
    let kitchen = format!("/api/devices/{KITCHEN}");
    let refusals = [
        server
            .patch(&kitchen, json!({"label": "Den"}), Some(&viewer))
            .await,
        server.delete(&kitchen, Some(&viewer)).await,
        server
            .post(
                &format!("/api/devices/{NEW}/approve"),
                json!({"label": "Den"}),
                Some(&viewer),
            )
            .await,
        server
            .post(
                &format!("{kitchen}/config"),
                json!({"config_sections": []}),
                Some(&viewer),
            )
            .await,
    ];
    for (status, body) in refusals {
        assert_eq!((status, &body["code"]), (403, &json!("forbidden")));
        assert!(body["error"].is_string());
    }
    assert!(server.runtimes.calls().is_empty());
    let (_, list) = server.get("/api/devices", Some(&viewer)).await;
    assert_eq!(device(&list, KITCHEN)["label"], "Kitchen");
    assert_eq!(device(&list, NEW)["approved"], false);

    let (status, body) = server.patch(&kitchen, json!({"label": "Den"}), None).await;
    assert_eq!((status, &body["code"]), (401, &json!("not_authenticated")));
}

/// dashboard.jsx `doRename`: `PATCH /api/devices/{id}` `{label}`, then relies on the
/// `device_update` event to show it.
#[tokio::test]
async fn renaming_stores_the_label_and_tells_the_dashboards() {
    let Fleet {
        server,
        admin,
        viewer,
    } = fleet().await;
    let mut socket = events(&server, &viewer).await;
    let (status, body) = server
        .patch(
            &format!("/api/devices/{KITCHEN}"),
            json!({"label": "  Kitchen counter "}),
            Some(&admin),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["label"], "Kitchen counter");
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type": "device_update", "device_id": KITCHEN, "state": {"label": "Kitchen counter"}})
    );
    let stored: String = server
        .sqlite()
        .query_row(
            "SELECT label FROM devices WHERE device_id = ?1",
            [KITCHEN],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "Kitchen counter");

    for label in [json!("   "), json!("x".repeat(65))] {
        let (status, body) = server
            .patch(
                &format!("/api/devices/{KITCHEN}"),
                json!({ "label": label }),
                Some(&admin),
            )
            .await;
        assert_eq!((status, &body["code"]), (400, &json!("bad_request")));
    }
    let (status, body) = server
        .patch(&format!("/api/devices/{KITCHEN}"), json!({}), Some(&admin))
        .await;
    assert_eq!((status, &body["code"]), (400, &json!("bad_request")));
    let (status, body) = server
        .patch(
            "/api/devices/G0NOTREGISTERED0",
            json!({"label": "Den"}),
            Some(&admin),
        )
        .await;
    assert_eq!((status, &body["code"]), (404, &json!("not_found")));
}

/// dashboard.jsx `doApprove`: `POST /api/devices/{id}/approve` `{label}`; every
/// dashboard re-reads the fleet on `device_approved`.
#[tokio::test]
async fn approval_goes_through_the_runtimes_and_is_announced() {
    let Fleet {
        server,
        admin,
        viewer,
    } = fleet().await;
    let mut socket = events(&server, &viewer).await;
    let (status, body) = server
        .post(
            &format!("/api/devices/{NEW}/approve"),
            json!({"label": "Bedroom"}),
            Some(&admin),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["approved"], true);
    assert_eq!(body["label"], "Bedroom");
    assert_eq!(
        server.runtimes.calls(),
        [RuntimeCall::Approve(
            DeviceId(NEW.to_owned()),
            "Bedroom".to_owned()
        )]
    );
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type": "device_approved", "device_id": NEW})
    );
    let (_, pending) = server.get("/api/devices/pending", Some(&viewer)).await;
    assert_eq!(pending, json!([]));

    let (status, body) = server
        .post(
            "/api/devices/G0NOTREGISTERED0/approve",
            json!({"label": "Den"}),
            Some(&admin),
        )
        .await;
    assert_eq!((status, &body["code"]), (404, &json!("not_found")));

    server.add_device("G0SYNTHETIC00004", None, false).await;
    server.runtimes.fail_with(Some(RuntimeError::Failed(
        "runtime did not start".to_owned(),
    )));
    let (status, body) = server
        .post(
            "/api/devices/G0SYNTHETIC00004/approve",
            json!({"label": "Den"}),
            Some(&admin),
        )
        .await;
    assert_eq!((status, &body["code"]), (500, &json!("internal")));
    assert!(!body.to_string().contains("runtime did not start"));
}

/// dashboard.jsx `doDelete`: `DELETE /api/devices/{id}`; every dashboard drops the
/// device on `device_deleted`.
#[tokio::test]
async fn deleting_a_device_removes_it_everywhere() {
    let Fleet {
        server,
        admin,
        viewer,
    } = fleet().await;
    server.add_log(HALLWAY, 1_700_000_000_000, "info", "device", "booted");
    let mut socket = events(&server, &viewer).await;
    let (status, body) = server
        .delete(&format!("/api/devices/{HALLWAY}"), Some(&admin))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"deleted": HALLWAY}));
    assert_eq!(
        server.runtimes.calls(),
        [RuntimeCall::Delete(DeviceId(HALLWAY.to_owned()))]
    );
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type": "device_deleted", "device_id": HALLWAY})
    );
    let (status, _) = server
        .get(&format!("/api/devices/{HALLWAY}"), Some(&viewer))
        .await;
    assert_eq!(status, 404);
    assert!(
        server
            .settings
            .snapshot()
            .device(&DeviceId(HALLWAY.to_owned()))
            .is_none()
    );
    let (status, body) = server
        .delete(&format!("/api/devices/{HALLWAY}"), Some(&admin))
        .await;
    assert_eq!((status, &body["code"]), (404, &json!("not_found")));
}

/// dashboard.jsx `pushConfig` sends `{config_sections, ...config}` (the whole effective
/// config) and reads `res.config`, `res.config_sections` and `res.use_global_config`.
#[tokio::test]
async fn a_device_config_edit_stores_its_sections() {
    let Fleet {
        server,
        admin,
        viewer,
    } = fleet().await;
    let path = format!("/api/devices/{KITCHEN}/config");
    let (status, global) = server.get("/api/global/config", Some(&admin)).await;
    assert_eq!(status, 200);
    let mut body = global.as_object().unwrap().clone();
    body.insert("config_sections".into(), json!(["wakeword", "screen"]));
    body.insert("nearMissThreshold".into(), json!(0.2));
    body.insert("wakeSound".into(), json!(true));
    body.insert("screenWakePassword".into(), json!("kiosk-secret"));
    body.insert("startupVolume".into(), json!(60));
    // A playback value; the section follows the fleet, so it is not stored.
    body.insert("duckDb".into(), json!(-6));

    let (status, response) = server.post(&path, Value::Object(body), Some(&admin)).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["config_sections"], json!(["wakeword", "screen"]));
    assert_eq!(response["use_global_config"], false);
    let config = response["config"].as_object().unwrap();
    assert_eq!(config["nearMissThreshold"], 0.2);
    assert_eq!(config["wakeSound"], true);
    assert_eq!(config["screenWakePassword"], "kiosk-secret");
    assert_eq!(config["startupVolume"], 60);
    assert!(!config.contains_key("duckDb"));
    assert_eq!(
        server.runtimes.calls(),
        [RuntimeCall::SettingsApplied(SettingsApplied {
            changed: vec![DeviceId(KITCHEN.to_owned())],
            new_session: vec![DeviceId(KITCHEN.to_owned())],
        })]
    );

    // Read back, as an administrator and as a read-only user, who never sees the
    // kiosk password.
    let (status, admin_view) = server.get(&path, Some(&admin)).await;
    assert_eq!(status, 200);
    assert_eq!(admin_view, response);
    let (status, viewer_view) = server.get(&path, Some(&viewer)).await;
    assert_eq!(status, 200);
    assert_eq!(viewer_view["config"]["screenWakePassword"], Value::Null);
    let (_, list) = server.get("/api/devices", Some(&viewer)).await;
    assert_eq!(
        device(&list, KITCHEN)["config"]["screenWakePassword"],
        Value::Null
    );
    assert_eq!(
        device(&list, KITCHEN)["config_sections"],
        json!(["wakeword", "screen"])
    );
    let (_, list) = server.get("/api/devices", Some(&admin)).await;
    assert_eq!(
        device(&list, KITCHEN)["config"]["screenWakePassword"],
        "kiosk-secret"
    );

    // Back to the fleet.
    let (status, response) = server
        .post(&path, json!({"config_sections": []}), Some(&admin))
        .await;
    assert_eq!(status, 200);
    assert_eq!(response["use_global_config"], true);
    assert_eq!(response["config"], json!({"startupVolume": 60}));
}

/// Unknown and removed keys are refused (§4.10) in the `{error, code}` shape whose
/// `error` dashboard.jsx shows (`alert(e.error || 'Failed to push config')`).
#[tokio::test]
async fn unknown_and_removed_keys_are_refused() {
    let Fleet { server, admin, .. } = fleet().await;
    let path = format!("/api/devices/{KITCHEN}/config");
    let cases = [
        (json!({"notASetting": 1}), "unknown_setting", "notASetting"),
        (
            json!({"owwModel": "hey_mycroft"}),
            "removed_setting",
            "owwModel",
        ),
        (json!({"duckDb": "loud"}), "invalid_setting", "duckDb"),
        (
            json!({"config_sections": ["kitchen"]}),
            "invalid_setting",
            "kitchen",
        ),
        (
            json!({"config_sections": ["wakeword"], "wakeModel": "00".repeat(32)}),
            "invalid_setting",
            "wakeModel",
        ),
    ];
    for (body, code, named) in cases {
        let (status, response) = server.post(&path, body, Some(&admin)).await;
        assert_eq!(
            (status, &response["code"]),
            (400, &json!(code)),
            "{response}"
        );
        assert!(
            response["error"].as_str().unwrap().contains(named),
            "{response}"
        );
    }
    let (status, response) = server.post(&path, json!([1, 2]), Some(&admin)).await;
    assert_eq!((status, &response["code"]), (400, &json!("bad_request")));
    let (status, response) = server
        .post(
            "/api/devices/G0NOTREGISTERED0/config",
            json!({"config_sections": []}),
            Some(&admin),
        )
        .await;
    assert_eq!((status, &response["code"]), (404, &json!("not_found")));
    // Nothing was stored, so nothing was applied.
    assert!(server.runtimes.calls().is_empty());
}

/// dashboard.jsx's Logs tab: `GET /api/devices/{id}/logs?limit=50`, shown in order,
/// each `new Date(entry.ts)` (milliseconds), `entry.level`, `entry.source`,
/// `entry.message`.
#[tokio::test]
async fn logs_come_newest_first_in_milliseconds() {
    let Fleet { server, viewer, .. } = fleet().await;
    let base = 1_700_000_000_000_i64;
    for i in 0..60 {
        server.add_log(
            KITCHEN,
            base + i * 1_000,
            "info",
            "device",
            &format!("line {i}"),
        );
    }
    // Two entries in the same millisecond keep their insertion order, newest first.
    server.add_log(
        KITCHEN,
        base + 59_000,
        "error",
        "controller",
        "same instant",
    );
    server.add_log(HALLWAY, base + 100_000, "warn", "device", "another device");

    let (status, logs) = server
        .get(
            &format!("/api/devices/{KITCHEN}/logs?limit=50"),
            Some(&viewer),
        )
        .await;
    assert_eq!(status, 200);
    let logs = logs.as_array().unwrap();
    assert_eq!(logs.len(), 50);
    assert_eq!(logs[0]["message"], "same instant");
    assert_eq!(logs[0]["level"], "error");
    assert_eq!(logs[0]["source"], "controller");
    assert_eq!(logs[0]["ts"], base + 59_000);
    assert_eq!(logs[1]["message"], "line 59");
    assert_eq!(logs[49]["message"], "line 11");
    assert!(logs.iter().all(|e| e["message"] != "another device"));
    assert!(logs[0]["id"].is_i64());

    let (status, logs) = server
        .get(&format!("/api/devices/{KITCHEN}/logs"), Some(&viewer))
        .await;
    assert_eq!(status, 200);
    assert_eq!(logs.as_array().unwrap().len(), 61);
    let (status, logs) = server
        .get(&format!("/api/devices/{NEW}/logs?limit=5"), Some(&viewer))
        .await;
    assert_eq!((status, logs), (200, json!([])));

    for query in ["limit=0", "limit=1001", "limit=lots"] {
        let (status, body) = server
            .get(
                &format!("/api/devices/{KITCHEN}/logs?{query}"),
                Some(&viewer),
            )
            .await;
        assert_eq!(
            (status, &body["code"]),
            (400, &json!("bad_request")),
            "{query}"
        );
    }
    let (status, body) = server
        .get("/api/devices/G0NOTREGISTERED0/logs", Some(&viewer))
        .await;
    assert_eq!((status, &body["code"]), (404, &json!("not_found")));
}

/// A Fire OS Show's gaps and a device without volume control (dashboard.jsx: Volume
/// row `unavailable — ${device.volume_gap}`, `capabilityGap`).
#[tokio::test]
async fn a_fire_os_show_reports_its_platform_gaps() {
    let Fleet { server, viewer, .. } = fleet().await;
    let mut show = connected_dot();
    let session = show.session.as_mut().unwrap();
    session.platform = Platform::FireOs6Crown;
    session.capabilities = Capabilities::from_names([
        "audio_timeline_v1",
        "uplink_leases_v1",
        "device_wake_v1",
        "render_reference_v1",
        "render_progress_v1",
        "focus_leases_v1",
        "alert_cache_v1",
        "turn_protocol_v1",
        "screen_wake",
        "open_rules_v1",
        "near_miss_v1",
    ]);
    session.volume = Some(LiveVolume::Unavailable);
    server.runtimes.set_live(HALLWAY, show);
    let (_, list) = server.get("/api/devices", Some(&viewer)).await;
    let hallway = device(&list, HALLWAY);
    assert_eq!(hallway["volume"], Value::Null);
    assert_eq!(
        hallway["volume_gap"],
        "the device reports no volume control"
    );
    assert_eq!(
        hallway["capability_gaps"]["leds"],
        "no LED ring on this device"
    );
    assert_eq!(
        hallway["capability_gaps"]["wifi_control"],
        "managed by the device"
    );
    assert_eq!(
        hallway["capability_gaps"]["kiosk_plugin"],
        "needs LineageOS on this Echo Show"
    );
    assert!(hallway["capability_gaps"].get("screen_wake").is_none());
}
