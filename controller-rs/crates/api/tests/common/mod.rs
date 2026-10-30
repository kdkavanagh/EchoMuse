//! A real API server on a loopback port over a temporary database built from the
//! contract schema, a real settings service over a registry seeded with the
//! repository's deployed wake model, and simulated device runtimes and model
//! qualification. Every row is synthetic.

#![allow(dead_code)]

pub mod services;

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use echomuse_api::{
    ApiConfig, ApiDeps, ApiServer, BcryptCost, BootstrapToken, BoxFuture, DeployReport, DeploySkip,
    DeploySkipReason, DeviceLabel, DeviceRuntimes, EventPublisher, EventsConfig, FirmwareControl,
    FirmwareRefusal, HaStatus, Ingress, LiveDevice, ModelCandidate, ModelQualifier, ModelRefused,
    RuntimeError, SettingsApplied, ThrottleConfig, UpdateStatus, WallClock,
};
use echomuse_core::types::DeviceId;
use echomuse_db::{DatabaseService, DbHandle, OpenedDatabase, ServiceConfig};
use echomuse_firmware::{
    FirmwareInfo, FirmwareVersion, LinkToken,
    provision::{
        CrownLists, DiagnosticsUpload, FirmwareDownload, HideList, PackageList,
        ProvisionDiagnostics, ProvisionError, StartScriptFile, TlsCredentials,
        TlsCredentialsRequest,
    },
};
use echomuse_protocol::Sha256;
use echomuse_records::{RecordingsLayout, RecordsService, RetentionLimits};
use echomuse_settings::{
    registry::{ProbeResult, SeedModel, WakeRegistry},
    service::{SettingsHandle, SettingsService},
};
use rusqlite::Connection;
use serde_json::Value;
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
    task::JoinHandle,
};

/// The wall time every test starts at: 2023-11-14T22:13:20Z.
pub const START: i64 = 1_700_000_000;
pub const DAY: i64 = 86_400;
/// The cost of hashes tests make; small so tests stay fast.
pub const TEST_COST: u32 = 4;
/// A password every synthetic user starts with.
pub const PASSWORD: &str = "synthetic password";

/// The secret placed beside the static directory, which must never be served.
pub const OUTSIDE_SECRET: &str = "outside-the-static-directory";

/// A wall clock the test moves by hand.
#[derive(Clone)]
pub struct ManualClock(Arc<AtomicI64>);

impl ManualClock {
    pub fn set(&self, seconds: i64) {
        self.0.store(seconds, Ordering::SeqCst);
    }

    pub fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl WallClock for ManualClock {
    fn now(&self) -> SystemTime {
        let seconds = u64::try_from(self.0.load(Ordering::SeqCst)).unwrap();
        UNIX_EPOCH + Duration::from_secs(seconds)
    }
}

/// What the simulated runtimes were asked to do, in order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeCall {
    Approve(DeviceId, String),
    Delete(DeviceId),
    SettingsApplied(SettingsApplied),
    Relabeled(DeviceId, String),
}

/// Simulated device runtimes. `approve` and `delete` change the database the way the
/// registry does; `settings_applied` reports every changed device that is connected.
#[derive(Clone)]
pub struct FakeRuntimes {
    db_path: Arc<Mutex<Option<PathBuf>>>,
    live: Arc<Mutex<HashMap<DeviceId, LiveDevice>>>,
    calls: Arc<Mutex<Vec<RuntimeCall>>>,
    fail: Arc<Mutex<Option<RuntimeError>>>,
}

impl Default for FakeRuntimes {
    fn default() -> Self {
        Self {
            db_path: Arc::new(Mutex::new(None)),
            live: Arc::default(),
            calls: Arc::default(),
            fail: Arc::default(),
        }
    }
}

impl FakeRuntimes {
    pub fn set_live(&self, device: &str, live: LiveDevice) {
        self.live
            .lock()
            .unwrap()
            .insert(DeviceId(device.to_owned()), live);
    }

    pub fn calls(&self) -> Vec<RuntimeCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Make the next commands fail with `error`.
    pub fn fail_with(&self, error: Option<RuntimeError>) {
        *self.fail.lock().unwrap() = error;
    }

    fn sqlite(&self) -> Connection {
        let path = self.db_path.lock().unwrap().clone().unwrap();
        let conn = Connection::open(path).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        conn
    }

    fn failure(&self) -> Result<(), RuntimeError> {
        self.fail.lock().unwrap().clone().map_or(Ok(()), Err)
    }
}

impl DeviceRuntimes for FakeRuntimes {
    fn live(&self, device: &DeviceId) -> Option<LiveDevice> {
        self.live.lock().unwrap().get(device).cloned()
    }

    fn approve(
        &self,
        device: DeviceId,
        label: DeviceLabel,
    ) -> BoxFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(RuntimeCall::Approve(
                device.clone(),
                label.as_str().to_owned(),
            ));
            self.failure()?;
            self.sqlite()
                .execute(
                    "UPDATE devices SET approved = 1, label = ?2 WHERE device_id = ?1",
                    [&device.0, label.as_str()],
                )
                .unwrap();
            Ok(())
        })
    }

    fn delete(&self, device: DeviceId) -> BoxFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(RuntimeCall::Delete(device.clone()));
            self.failure()?;
            let conn = self.sqlite();
            conn.execute("DELETE FROM device_logs WHERE device_id = ?1", [&device.0])
                .unwrap();
            conn.execute("DELETE FROM devices WHERE device_id = ?1", [&device.0])
                .unwrap();
            self.live.lock().unwrap().remove(&device);
            Ok(())
        })
    }

    fn settings_applied(&self, change: SettingsApplied) -> BoxFuture<'_, Vec<DeviceId>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(RuntimeCall::SettingsApplied(change.clone()));
            let live = self.live.lock().unwrap();
            change
                .changed
                .into_iter()
                .filter(|device| live.get(device).is_some_and(LiveDevice::connected))
                .collect()
        })
    }

    fn relabeled(&self, device: DeviceId, label: String) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(RuntimeCall::Relabeled(device, label));
        })
    }
}

/// Simulated qualification: answers with the result set, and counts the models it ran.
#[derive(Clone)]
pub struct FakeQualifier {
    result: Arc<Mutex<Result<ProbeResult, ModelRefused>>>,
    pub runs: Arc<Mutex<Vec<ModelCandidate>>>,
}

impl Default for FakeQualifier {
    fn default() -> Self {
        Self {
            result: Arc::new(Mutex::new(Ok(ProbeResult::new(0.01, 0.02, 0.03).unwrap()))),
            runs: Arc::default(),
        }
    }
}

impl FakeQualifier {
    pub fn answer(&self, result: Result<ProbeResult, ModelRefused>) {
        *self.result.lock().unwrap() = result;
    }
}

impl ModelQualifier for FakeQualifier {
    fn qualify(
        &self,
        candidate: ModelCandidate,
    ) -> BoxFuture<'_, Result<ProbeResult, ModelRefused>> {
        Box::pin(async move {
            self.runs.lock().unwrap().push(candidate);
            self.result.lock().unwrap().clone()
        })
    }
}

/// What the simulated firmware delivery was asked to do, in order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FirmwareCall {
    Update(DeviceId),
    Rollback(DeviceId),
    Queue(DeviceId),
    CancelQueue(DeviceId),
    Debloat(DeviceId),
    SecureLink(DeviceId),
    Deploy,
    TlsCredentials(String),
}

/// The bundled firmware the simulated delivery reports.
pub const BUNDLED_VERSION: &str = "fw-synthetic-bundled";
pub const BUNDLED_BYTES: &[u8] = b"synthetic firmware binary";

/// Simulated firmware delivery: records calls, answers with the refusal set, and
/// reports the status set per device.
#[derive(Clone, Default)]
pub struct FakeFirmware {
    calls: Arc<Mutex<Vec<FirmwareCall>>>,
    refusal: Arc<Mutex<Option<FirmwareRefusal>>>,
    status: Arc<Mutex<HashMap<DeviceId, UpdateStatus>>>,
    queued: Arc<Mutex<HashMap<DeviceId, i64>>>,
}

impl FakeFirmware {
    pub fn calls(&self) -> Vec<FirmwareCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn refuse_with(&self, refusal: Option<FirmwareRefusal>) {
        *self.refusal.lock().unwrap() = refusal;
    }

    pub fn set_status(&self, device: &str, status: UpdateStatus) {
        self.status
            .lock()
            .unwrap()
            .insert(DeviceId(device.to_owned()), status);
    }

    fn record(&self, call: FirmwareCall) -> Result<(), FirmwareRefusal> {
        self.calls.lock().unwrap().push(call);
        self.refusal.lock().unwrap().clone().map_or(Ok(()), Err)
    }

    fn info() -> FirmwareInfo {
        FirmwareInfo {
            version: FirmwareVersion::new(BUNDLED_VERSION).unwrap(),
            size: BUNDLED_BYTES.len() as u64,
            sha256: Sha256::of(BUNDLED_BYTES),
        }
    }
}

impl FirmwareControl for FakeFirmware {
    fn bundled(&self) -> FirmwareInfo {
        Self::info()
    }

    fn status(&self, device: &DeviceId) -> UpdateStatus {
        self.status
            .lock()
            .unwrap()
            .get(device)
            .cloned()
            .unwrap_or_default()
    }

    fn update(&self, device: DeviceId) -> BoxFuture<'_, Result<String, FirmwareRefusal>> {
        Box::pin(async move {
            self.record(FirmwareCall::Update(device))?;
            Ok(BUNDLED_VERSION.to_owned())
        })
    }

    fn rollback(&self, device: DeviceId) -> BoxFuture<'_, Result<(), FirmwareRefusal>> {
        Box::pin(async move { self.record(FirmwareCall::Rollback(device)) })
    }

    fn queue_update(&self, device: DeviceId) -> BoxFuture<'_, Result<i64, FirmwareRefusal>> {
        Box::pin(async move {
            self.record(FirmwareCall::Queue(device.clone()))?;
            Ok(*self.queued.lock().unwrap().entry(device).or_insert(START))
        })
    }

    fn cancel_queued_update(
        &self,
        device: DeviceId,
    ) -> BoxFuture<'_, Result<bool, FirmwareRefusal>> {
        Box::pin(async move {
            self.record(FirmwareCall::CancelQueue(device.clone()))?;
            Ok(self.queued.lock().unwrap().remove(&device).is_some())
        })
    }

    fn debloat(&self, device: DeviceId) -> BoxFuture<'_, Result<(), FirmwareRefusal>> {
        Box::pin(async move { self.record(FirmwareCall::Debloat(device)) })
    }

    fn secure_link(&self, device: DeviceId) -> BoxFuture<'_, Result<(), FirmwareRefusal>> {
        Box::pin(async move { self.record(FirmwareCall::SecureLink(device)) })
    }

    fn deploy(&self) -> BoxFuture<'_, DeployReport> {
        Box::pin(async move {
            let _ = self.record(FirmwareCall::Deploy);
            DeployReport {
                version: BUNDLED_VERSION.to_owned(),
                started: vec![DeviceId("G0SYNTHETIC00001".to_owned())],
                skipped: vec![DeploySkip {
                    device_id: DeviceId("G0SYNTHETIC00002".to_owned()),
                    reason: DeploySkipReason::Offline,
                }],
            }
        })
    }

    fn provision_start_script(&self) -> BoxFuture<'_, Result<StartScriptFile, ProvisionError>> {
        Box::pin(async {
            Ok(StartScriptFile {
                bytes: Bytes::from_static(b"#!/system/bin/sh\n# synthetic\n"),
            })
        })
    }

    fn provision_crown_lists(&self) -> BoxFuture<'_, Result<CrownLists, ProvisionError>> {
        Box::pin(async {
            let list = |file: &'static str| PackageList {
                file,
                text: "com.example.synthetic\n".to_owned(),
                packages: vec!["com.example.synthetic".to_owned()],
            };
            Ok(CrownLists {
                dir: "/data/local/etc/echomuse",
                tiers_file: "crown_tiers",
                hide: vec![HideList {
                    tier: "1",
                    list: list("crown_hide_packages.txt"),
                }],
                mic_deny: list("crown_mic_deny_packages.txt"),
            })
        })
    }

    fn provision_firmware(&self) -> BoxFuture<'_, Result<FirmwareDownload, ProvisionError>> {
        Box::pin(async {
            Ok(FirmwareDownload {
                bytes: Bytes::from_static(BUNDLED_BYTES),
                info: Self::info(),
            })
        })
    }

    fn provision_tls_credentials(
        &self,
        request: TlsCredentialsRequest,
    ) -> BoxFuture<'_, Result<TlsCredentials, ProvisionError>> {
        Box::pin(async move {
            let _ = self.record(FirmwareCall::TlsCredentials(request.device_id.clone()));
            if request.device_id.is_empty() {
                return Err(ProvisionError::InvalidRequest(
                    "device_id is empty".to_owned(),
                ));
            }
            Ok(TlsCredentials {
                ca_pem: "-----BEGIN CERTIFICATE-----\nsynthetic\n".to_owned(),
                token: LinkToken::new("synthetic-token".to_owned()),
                dir: "/data/local/etc/echomuse",
            })
        })
    }

    fn provision_diagnostics(
        &self,
        upload: DiagnosticsUpload,
    ) -> Result<ProvisionDiagnostics, ProvisionError> {
        Ok(ProvisionDiagnostics {
            received_utc_ms: Some(START as u64 * 1000),
            controller_version: None,
            bundled_firmware: Self::info(),
            step: upload.step,
            error: upload.error,
            selected_ssid: upload.selected_ssid,
            probes: upload.probes,
            transcript: upload.transcript,
        })
    }
}

/// The repository's deployed graph and sidecar.
pub fn repo_model() -> (PathBuf, PathBuf) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    (
        root.join("bcresnet_audio.onnx"),
        root.join("bcresnet_audio.json"),
    )
}

pub struct Options {
    pub ingress: Ingress,
    pub with_setup_token: bool,
    pub throttle: ThrottleConfig,
    pub events: EventsConfig,
    /// Transcript correction, clip scoring and the runtime asset; refusing ones by default.
    pub diagnostics: Option<echomuse_api::Diagnostics>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            ingress: Ingress::Off,
            with_setup_token: true,
            throttle: ThrottleConfig::default(),
            events: EventsConfig::default(),
            diagnostics: None,
        }
    }
}

pub struct TestServer {
    pub address: SocketAddr,
    pub publisher: EventPublisher,
    pub clock: ManualClock,
    pub runtimes: FakeRuntimes,
    pub qualifier: FakeQualifier,
    pub firmware: FakeFirmware,
    pub ha_status: watch::Sender<HaStatus>,
    pub settings: SettingsHandle,
    pub db: DbHandle,
    pub records: RecordsService,
    pub db_path: PathBuf,
    pub setup_token: Option<String>,
    pub http: reqwest::Client,
    pub dir: tempfile::TempDir,
    stop: Option<oneshot::Sender<()>>,
    server: Option<JoinHandle<std::io::Result<()>>>,
    settings_service: Option<SettingsService>,
    service: Option<DatabaseService>,
}

/// The shipped landing and dashboard pages, read from the repository, plus synthetic
/// assets.
fn static_dir(root: &Path) -> PathBuf {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../controller/static");
    let dir = root.join("static");
    std::fs::create_dir_all(dir.join("vendor")).unwrap();
    for page in ["index.html", "dashboard.html"] {
        std::fs::copy(shipped.join(page), dir.join(page)).unwrap();
    }
    std::fs::write(
        dir.join("favicon.svg"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    )
    .unwrap();
    std::fs::write(dir.join("dashboard.js"), "console.log('synthetic');").unwrap();
    std::fs::write(dir.join("vendor/xterm.css"), "body{}").unwrap();
    std::fs::write(root.join("secret.txt"), OUTSIDE_SECRET).unwrap();
    dir
}

impl TestServer {
    pub async fn start(options: Options) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let static_dir = static_dir(dir.path());
        let db_path = dir.path().join("echomuse.db");
        let opened = OpenedDatabase::open(&db_path).unwrap();
        let service = DatabaseService::start(opened, ServiceConfig::default()).unwrap();
        let db = service.handle();
        let (graph, sidecar) = repo_model();
        let registry = WakeRegistry::open_or_seed(
            dir.path().join("oww_models"),
            &SeedModel::deployed(graph, sidecar),
        )
        .unwrap();
        let settings_service = SettingsService::start(db.clone(), registry).await.unwrap();
        let settings = settings_service.handle();

        let bootstrap = options
            .with_setup_token
            .then(|| BootstrapToken::generate().unwrap());
        let setup_token = bootstrap.as_ref().map(|t| t.expose().to_owned());
        let clock = ManualClock(Arc::new(AtomicI64::new(START)));
        let runtimes = FakeRuntimes::default();
        *runtimes.db_path.lock().unwrap() = Some(db_path.clone());
        let qualifier = FakeQualifier::default();
        let firmware = FakeFirmware::default();
        let (ha_status, ha_status_receiver) = watch::channel(HaStatus::default());
        let records = RecordsService::new(
            db.clone(),
            RecordingsLayout::beside_database(&db_path),
            RetentionLimits::default(),
        );

        let mut config = ApiConfig::new(static_dir, options.ingress);
        config.bcrypt_cost = BcryptCost::new(TEST_COST).unwrap();
        config.login_throttle = options.throttle;
        config.events = options.events;
        config.controller_version = Some("v0.0.0-synthetic".to_owned());
        let server = ApiServer::new(
            config,
            ApiDeps {
                db: db.clone(),
                bootstrap,
                settings: settings.clone(),
                runtimes: Box::new(runtimes.clone()),
                qualifier: Box::new(qualifier.clone()),
                firmware: Box::new(firmware.clone()),
                records: records.clone(),
                services: services::refusing(),
                capture: services::refusing_capture(),
                diagnostics: options
                    .diagnostics
                    .unwrap_or_else(services::refusing_diagnostics),
                ha_status: ha_status_receiver,
                clock: Box::new(clock.clone()),
            },
        );
        let publisher = server.publisher();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(server.serve(listener, async move {
            let _ = stopped.await;
        }));
        Self {
            address,
            publisher,
            clock,
            runtimes,
            qualifier,
            firmware,
            ha_status,
            settings,
            db,
            records,
            db_path,
            setup_token,
            http: http_client(),
            dir,
            stop: Some(stop),
            server: Some(server),
            settings_service: Some(settings_service),
            service: Some(service),
        }
    }

    /// Register a device row as admission would, then let the settings read it.
    pub async fn add_device(&self, id: &str, label: Option<&str>, approved: bool) {
        self.sqlite()
            .execute(
                "INSERT INTO devices (device_id, label, approved, ip, firmware_ver, first_seen, \
                 last_seen, esphome_api_port, token, kiosk_token) \
                 VALUES (?1, ?2, ?3, '192.0.2.20', 'fw-synthetic', ?4, ?5, 16001, \
                 'synthetic-link-token', 'synthetic-kiosk-token')",
                rusqlite::params![id, label, i64::from(approved), START - DAY, START],
            )
            .unwrap();
        self.settings
            .refresh_device(&DeviceId(id.to_owned()))
            .await
            .unwrap();
    }

    /// Store a device log entry.
    pub fn add_log(&self, id: &str, ts_ms: i64, level: &str, source: &str, message: &str) {
        self.sqlite()
            .execute(
                "INSERT INTO device_logs (device_id, ts, level, source, message) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![id, ts_ms, level, source, message],
            )
            .unwrap();
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    pub fn ws_url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.address)
    }

    /// A direct connection to the database file, for synthetic rows and checks.
    pub fn sqlite(&self) -> Connection {
        let conn = Connection::open(&self.db_path).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        conn
    }

    /// Insert a user whose password is [`PASSWORD`], hashed at [`TEST_COST`].
    pub fn add_user(&self, username: &str, role: &str) -> i64 {
        let hash = bcrypt::hash_with_salt(PASSWORD, TEST_COST, [7; 16])
            .unwrap()
            .format_for_version(bcrypt::Version::TwoB);
        self.add_user_with_hash(username, role, &hash)
    }

    pub fn add_user_with_hash(&self, username: &str, role: &str, hash: &str) -> i64 {
        let conn = self.sqlite();
        conn.execute(
            "INSERT INTO users (username, password_hash, role, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![username, hash, role, START - DAY],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    pub fn set_system(&self, key: &str, value: &str) {
        self.sqlite()
            .execute(
                "INSERT OR REPLACE INTO system_config (key, value) VALUES (?1, ?2)",
                [key, value],
            )
            .unwrap();
    }

    /// A request with a JSON body, or none.
    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> (u16, Value) {
        let mut request = self.http.request(method, self.url(path));
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        status_json(request.send().await.unwrap()).await
    }

    pub async fn post(&self, path: &str, body: Value, token: Option<&str>) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, Some(body), token)
            .await
    }

    pub async fn patch(&self, path: &str, body: Value, token: Option<&str>) -> (u16, Value) {
        self.send(reqwest::Method::PATCH, path, Some(body), token)
            .await
    }

    pub async fn delete(&self, path: &str, token: Option<&str>) -> (u16, Value) {
        self.send(reqwest::Method::DELETE, path, None, token).await
    }

    pub async fn get(&self, path: &str, token: Option<&str>) -> (u16, Value) {
        self.send(reqwest::Method::GET, path, None, token).await
    }

    /// Sign in and return the session token.
    pub async fn login(&self, username: &str, password: &str) -> String {
        let (status, body) = self
            .post(
                "/api/auth/login",
                serde_json::json!({"username": username, "password": password}),
                None,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        body["token"].as_str().unwrap().to_owned()
    }

    /// Stop serving and wait for the server to return.
    pub async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            tokio::time::timeout(Duration::from_secs(10), server)
                .await
                .expect("the server stops")
                .unwrap()
                .unwrap();
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        drop(self.settings_service.take());
        drop(self.service.take());
    }
}

/// The HTTP client the tests talk to the server with. These requests are plain HTTP,
/// but in a workspace build reqwest also gets rustls without a crypto provider (the
/// Home Assistant crate turns that feature on), and building a client then panics.
/// So the tests install ring as the process default once before the first client.
fn http_client() -> reqwest::Client {
    static PROVIDER: std::sync::Once = std::sync::Once::new();
    PROVIDER.call_once(|| {
        // Another test in this process may have installed it first; either way one is set.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    reqwest::Client::new()
}

pub async fn status_json(response: reqwest::Response) -> (u16, Value) {
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, body)
}

/// The proxy address that is not this test's client, for refusing.
pub const NOT_LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::new(172, 30, 32, 2));
