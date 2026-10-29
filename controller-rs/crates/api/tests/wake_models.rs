//! The wake-model registry routes over real HTTP (§4.11), checked against
//! controller/static/dashboard.jsx's `WakeModelPicker`.

mod common;

use common::{Options, PASSWORD, TestServer};
use echomuse_api::ModelRefused;
use echomuse_settings::registry::ProbeResult;
use serde_json::{Value, json};

/// The deployed model's graph SHA-256 (configuration.md "Defaults at a glance").
const DEPLOYED: &str = "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f";
const KITCHEN: &str = "G0SYNTHETIC00001";
const BOUNDARY: &str = "echomuse-synthetic-boundary";

async fn server() -> (TestServer, String, String) {
    let server = TestServer::start(Options::default()).await;
    server.add_user("admin", "admin");
    server.add_user("viewer", "readonly");
    let admin = server.login("admin", PASSWORD).await;
    let viewer = server.login("viewer", PASSWORD).await;
    (server, admin, viewer)
}

/// A sidecar the firmware accepts, different from the deployed one.
fn sidecar(label: &str) -> Vec<u8> {
    format!(
        r#"{{"type": "bcresnet", "sampleRate": 16000, "window": 22400, "labels": ["noise", "{label}", "unknown"], "wakeIndex": 1, "nMels": 40, "clipSeconds": 1.4, "normPeak": 0.8}}"#
    )
    .into_bytes()
}

/// The form dashboard.jsx's `upload()` sends: `graph`, `sidecar`, `idle`, `playback`,
/// `reference`, `wake_phrase`, `verify_core`.
fn form(graph: &[u8], sidecar: &[u8], fields: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, filename, bytes) in [
        ("graph", "model.onnx", graph),
        ("sidecar", "model.json", sidecar),
    ] {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

const FIELDS: [(&str, &str); 5] = [
    ("idle", "0.9"),
    ("playback", "0.65"),
    ("reference", "0.3"),
    ("wake_phrase", "computer"),
    ("verify_core", "comput"),
];

async fn upload(server: &TestServer, token: &str, body: Vec<u8>) -> (u16, Value) {
    let response = server
        .http
        .post(server.url("/api/wake_models/upload"))
        .bearer_auth(token)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    common::status_json(response).await
}

/// `useWakeRegistry` reads `{active, models}`; each tile reads `graph_sha256`,
/// `sidecar_sha256`, `wake_phrase`, `thresholds.{idle,playback,reference}`, `probe`,
/// `active` and `in_use_by` (no delete button while it has entries).
#[tokio::test]
async fn the_registry_lists_the_seeded_model() {
    let (server, _, viewer) = server().await;
    let (status, body) = server.get("/api/wake_models", Some(&viewer)).await;
    assert_eq!(status, 200);
    assert_eq!(body["active"], DEPLOYED);
    let models = body["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    let model = &models[0];
    assert_eq!(model["graph_sha256"], DEPLOYED);
    assert_eq!(model["wake_phrase"], "ophelia");
    assert_eq!(model["verify_core"], "ophel");
    assert_eq!(
        model["thresholds"],
        json!({"idle": 0.9, "playback": 0.65, "reference": 0.3})
    );
    // The seeded model was never probed: unknown, not zero.
    assert_eq!(model["probe"], Value::Null);
    assert_eq!(model["active"], true);
    assert_eq!(model["in_use_by"], json!([{"scope": "fleet"}]));
    assert!(model["sidecar_sha256"].as_str().unwrap().len() == 64);
}

/// dashboard.jsx `upload()` reads `r.model.graph_sha256` and `r.model.probe`.
#[tokio::test]
async fn an_upload_is_qualified_then_registered_unselected() {
    let (server, admin, viewer) = server().await;
    let graph = b"synthetic graph bytes".to_vec();
    let (status, body) = upload(
        &server,
        &viewer,
        form(&graph, &sidecar("computer"), &FIELDS),
    )
    .await;
    assert_eq!((status, &body["code"]), (403, &json!("forbidden")));
    assert!(server.qualifier.runs.lock().unwrap().is_empty());

    let (status, body) = upload(&server, &admin, form(&graph, &sidecar("computer"), &FIELDS)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["added"], true);
    let sha = body["model"]["graph_sha256"].as_str().unwrap().to_owned();
    assert_eq!(
        body["model"]["probe"],
        json!({"silence": 0.01, "noise": 0.02, "tone": 0.03})
    );
    assert_eq!(body["model"]["active"], false);
    assert_eq!(body["model"]["in_use_by"], json!([]));
    {
        let runs = server.qualifier.runs.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].graph_sha256.as_str(), sha);
        assert_eq!(&*runs[0].graph, graph.as_slice());
    }
    let (_, list) = server.get("/api/wake_models", Some(&viewer)).await;
    assert_eq!(list["active"], DEPLOYED);
    assert_eq!(list["models"].as_array().unwrap().len(), 2);

    // The same graph again returns the entry unchanged without running it again.
    let (status, again) =
        upload(&server, &admin, form(&graph, &sidecar("computer"), &FIELDS)).await;
    assert_eq!(status, 200);
    assert_eq!(again["added"], false);
    assert_eq!(again["model"], body["model"]);
    assert_eq!(server.qualifier.runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn bad_uploads_are_refused_with_a_reason() {
    let (server, admin, _) = server().await;
    let graph = b"another synthetic graph".to_vec();

    server.qualifier.answer(Err(ModelRefused(
        "the graph's input is not [1, 22400]".to_owned(),
    )));
    let (status, body) = upload(&server, &admin, form(&graph, &sidecar("computer"), &FIELDS)).await;
    assert_eq!((status, &body["code"]), (422, &json!("model_rejected")));
    assert!(body["error"].as_str().unwrap().contains("[1, 22400]"));

    // A probe at the reference threshold is refused by the registry.
    server
        .qualifier
        .answer(Ok(ProbeResult::new(0.0, 0.31, 0.0).unwrap()));
    let (status, body) = upload(&server, &admin, form(&graph, &sidecar("computer"), &FIELDS)).await;
    assert_eq!((status, &body["code"]), (422, &json!("model_rejected")));
    assert!(body["error"].as_str().unwrap().contains("noise"));

    // Checks made before anything runs.
    server
        .qualifier
        .answer(Ok(ProbeResult::new(0.0, 0.0, 0.0).unwrap()));
    let runs_before = server.qualifier.runs.lock().unwrap().len();
    let bad_order = [
        ("idle", "0.5"),
        ("playback", "0.65"),
        ("reference", "0.3"),
        ("wake_phrase", "computer"),
        ("verify_core", "comput"),
    ];
    let bad_phrase = [
        ("idle", "0.9"),
        ("playback", "0.65"),
        ("reference", "0.3"),
        ("wake_phrase", "Computer!"),
        ("verify_core", "comput"),
    ];
    for (body, code) in [
        (
            form(&graph, b"{\"type\": \"other\"}", &FIELDS),
            "model_rejected",
        ),
        (
            form(&graph, &sidecar("computer"), &bad_order),
            "model_rejected",
        ),
        (
            form(&graph, &sidecar("computer"), &bad_phrase),
            "model_rejected",
        ),
        (
            form(&graph, &sidecar("computer"), &FIELDS[..4]),
            "bad_request",
        ),
        (form(&[], &sidecar("computer"), &FIELDS), "model_rejected"),
    ] {
        let (status, response) = upload(&server, &admin, body).await;
        assert_eq!(response["code"], code, "{response}");
        assert!(status == 400 || status == 422);
    }
    assert_eq!(server.qualifier.runs.lock().unwrap().len(), runs_before);

    let response = server
        .http
        .post(server.url("/api/wake_models/upload"))
        .bearer_auth(&admin)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    let (status, body) = common::status_json(response).await;
    assert_eq!(
        (status, &body["code"]),
        (415, &json!("unsupported_media_type"))
    );

    let (_, list) = server.get("/api/wake_models", Some(&admin)).await;
    assert_eq!(list["models"].as_array().unwrap().len(), 1);
}

/// dashboard.jsx `remove(model)`: `DELETE /api/wake_models/{sha}`, showing `e.error`
/// ("Model is still in use") when refused.
#[tokio::test]
async fn a_model_in_use_cannot_be_deleted() {
    let (server, admin, viewer) = server().await;
    let (status, body) = server
        .delete(&format!("/api/wake_models/{DEPLOYED}"), Some(&admin))
        .await;
    assert_eq!((status, &body["code"]), (409, &json!("in_use")), "{body}");
    assert!(body["error"].as_str().unwrap().contains("the fleet"));

    let (_, added) = upload(
        &server,
        &admin,
        form(b"graph to select", &sidecar("computer"), &FIELDS),
    )
    .await;
    let sha = added["model"]["graph_sha256"].as_str().unwrap().to_owned();

    // A device that selects it holds it too.
    server.add_device(KITCHEN, Some("Kitchen"), true).await;
    let (status, _) = server
        .post(
            &format!("/api/devices/{KITCHEN}/config"),
            json!({"config_sections": ["wakeword"], "wakeModel": sha}),
            Some(&admin),
        )
        .await;
    assert_eq!(status, 200);
    let (_, list) = server.get("/api/wake_models", Some(&viewer)).await;
    let model = list["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["graph_sha256"] == sha.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        model["in_use_by"],
        json!([{"scope": "device", "device_id": KITCHEN}])
    );
    let (status, body) = server
        .delete(&format!("/api/wake_models/{sha}"), Some(&viewer))
        .await;
    assert_eq!((status, &body["code"]), (403, &json!("forbidden")));
    let (status, body) = server
        .delete(&format!("/api/wake_models/{sha}"), Some(&admin))
        .await;
    assert_eq!((status, &body["code"]), (409, &json!("in_use")));
    assert!(body["error"].as_str().unwrap().contains(KITCHEN));

    // Once nothing selects it, it goes.
    let (status, _) = server
        .post(
            &format!("/api/devices/{KITCHEN}/config"),
            json!({"config_sections": []}),
            Some(&admin),
        )
        .await;
    assert_eq!(status, 200);
    let (status, body) = server
        .delete(&format!("/api/wake_models/{sha}"), Some(&admin))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"deleted": sha}));
    let (_, list) = server.get("/api/wake_models", Some(&viewer)).await;
    assert_eq!(list["models"].as_array().unwrap().len(), 1);

    let (status, body) = server
        .delete(&format!("/api/wake_models/{sha}"), Some(&admin))
        .await;
    assert_eq!((status, &body["code"]), (404, &json!("not_found")));
    let (status, body) = server
        .delete("/api/wake_models/not-a-sha", Some(&admin))
        .await;
    assert_eq!((status, &body["code"]), (400, &json!("bad_request")));
}
