//! Firmware delivery against a simulated device shell, a temporary database built from
//! the contract schema, and the real payload files.

mod fake_device;

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use echomuse_core::types::DeviceId;
use echomuse_db::devices::{ApproveDevice, DeviceRow, GetDevice, RecordConnectionAttempt};
use echomuse_db::{DatabaseService, DbHandle, OpenedDatabase, ServiceConfig};
use echomuse_link::SystemClock;
use echomuse_protocol::Platform;
use tokio::sync::mpsc;

use self::fake_device::{FakeAccess, FakeDevice, Node, OLD_BUILD};
use crate::layout::{CA_PEM, CROWN_TIERS, CrownList, ETC_DIR, FileMode, LINK_TOKEN, START_SCRIPT};
use crate::provision::{DiagnosticsUpload, TlsCredentialsRequest};
use crate::transfer::{CHUNK_BYTES, TransferError, TransferLimits, check_tools, transfer_file};
use crate::*;

const DEVICE: &str = "G0FAKE00001";
const NEW_BUILD: &[u8] = b"\x7fELF the bundled build";
const CA: &str = "-----BEGIN CERTIFICATE-----\nMIIBfake\n-----END CERTIFICATE-----\n";

fn payload_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../controller/device_payloads")
}

fn version(text: &str) -> FirmwareVersion {
    FirmwareVersion::new(text).unwrap()
}

/// Every event published, in order.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<FirmwareEvent>>>);

impl FirmwareEvents for Recorder {
    fn publish(&self, event: FirmwareEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Recorder {
    fn events(&self) -> Vec<FirmwareEvent> {
        self.0.lock().unwrap().clone()
    }
}

type Service = FirmwareService<FakeAccess, DbHandle, Recorder>;

struct Harness {
    service: Service,
    device: FakeDevice,
    events: Recorder,
    db: DbHandle,
    firmware_dir: tempfile::TempDir,
    _db_service: DatabaseService,
    _db_dir: tempfile::TempDir,
}

impl Harness {
    async fn start(platform: Platform) -> Self {
        Self::start_with(platform, Some(CA.to_owned())).await
    }

    async fn start_with(platform: Platform, ca: Option<String>) -> Self {
        let db_dir = tempfile::tempdir().unwrap();
        let opened = OpenedDatabase::open(db_dir.path().join("echomuse.db")).unwrap();
        let db_service = DatabaseService::start(opened, ServiceConfig::default()).unwrap();
        let db = db_service.handle();
        db.write(RecordConnectionAttempt {
            device: DeviceId(DEVICE.into()),
            ip: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
            firmware_version: Some("fw-old".into()),
            os_version: Some("Fire OS 6".into()),
            at: SystemTime::now(),
        })
        .await
        .unwrap();
        db.write(ApproveDevice {
            device: DeviceId(DEVICE.into()),
            label: "Kitchen".into(),
        })
        .await
        .unwrap();

        let firmware_dir = tempfile::tempdir().unwrap();
        std::fs::write(firmware_dir.path().join(SERVER_FILE), NEW_BUILD).unwrap();
        std::fs::write(firmware_dir.path().join(VERSION_FILE), "fw-new\n").unwrap();

        let (hello_tx, mut hello_rx) = mpsc::unbounded_channel();
        let device = FakeDevice::new(DEVICE, platform, hello_tx);
        device.state().builds.insert(
            Md5::of(NEW_BUILD),
            fake_device::Build {
                version: "fw-new".into(),
                starts: true,
            },
        );
        let events = Recorder::default();
        let service = FirmwareService::new(
            FakeAccess {
                device: device.clone(),
            },
            db.clone(),
            events.clone(),
            FirmwareServiceConfig {
                bundled: BundledFirmware::load(firmware_dir.path()).unwrap(),
                payloads: DevicePayloads::new(payload_dir()),
                link_ca_pem: ca,
                controller_version: Some("test".into()),
                timings: FirmwareTimings {
                    reconnect: Duration::from_secs(10),
                    ..FirmwareTimings::default()
                },
                clock: Arc::new(SystemClock),
            },
        );
        // The integrator's admission path: every admitted hello reaches the service.
        let observer = service.clone();
        tokio::spawn(async move {
            while let Some(hello) = hello_rx.recv().await {
                observer.observe_hello(&hello);
            }
        });
        Self {
            service,
            device,
            events,
            db,
            firmware_dir,
            _db_service: db_service,
            _db_dir: db_dir,
        }
    }

    fn target(&self) -> DeviceTarget {
        DeviceTarget {
            device: DeviceId(DEVICE.into()),
            platform: self.device.platform,
            running: Some(version("fw-old")),
            previous: Some(version("fw-older")),
        }
    }

    async fn row(&self) -> DeviceRow {
        self.db
            .read(GetDevice {
                device: DeviceId(DEVICE.into()),
            })
            .await
            .unwrap()
            .unwrap()
    }

    fn assert_no_unknown_commands(&self) {
        assert_eq!(self.device.state().unknown, Vec::<String>::new());
    }
}

fn limits() -> TransferLimits {
    let within = CommandLimits::within(Duration::from_secs(5));
    TransferLimits {
        command: within,
        chunk: within,
        verify: within,
    }
}

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

async fn ready_shell(
    device: &FakeDevice,
) -> (DeviceShell<fake_device::FakeShell>, transfer::ToolsReady) {
    let mut shell = DeviceShell::new(device.shell());
    let tools = check_tools(&mut shell, &limits()).await.unwrap();
    (shell, tools)
}

fn dev_path(path: &str) -> layout::DevicePath {
    layout::DevicePath::parse(path).unwrap()
}

// ── Transfer ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_transfer_writes_the_file_in_chunks_and_moves_it_into_place() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let device = FakeDevice::new(DEVICE, Platform::FireOs6, tx);
    let (mut shell, tools) = ready_shell(&device).await;
    let bytes = data(CHUNK_BYTES * 2 + 1000);
    let mut reports = Vec::new();
    let md5 = transfer_file(
        &mut shell,
        &tools,
        &bytes,
        &dev_path("/data/local/bin/server_b"),
        FileMode::EXECUTABLE,
        &limits(),
        &mut |sent, total| reports.push((sent, total)),
    )
    .await
    .unwrap();
    assert_eq!(md5, Md5::of(&bytes));
    assert_eq!(
        device.file("/data/local/bin/server_b"),
        Some((bytes.clone(), 0o755))
    );
    assert!(device.file("/data/local/bin/server_b.part").is_none());
    let total = bytes.len() as u64;
    assert_eq!(
        reports,
        [
            (CHUNK_BYTES as u64, total),
            (2 * CHUNK_BYTES as u64, total),
            (total, total)
        ]
    );
    assert_eq!(device.state().heredoc_writes.len(), 3);
    assert!(device.state().unknown.is_empty());

    // An empty file is a file too.
    transfer_file(
        &mut shell,
        &tools,
        &[],
        &dev_path("/data/local/etc/echomuse/empty"),
        FileMode::READABLE,
        &limits(),
        &mut |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(
        device.file("/data/local/etc/echomuse/empty"),
        Some((vec![], 0o644))
    );
}

#[tokio::test]
async fn a_corrupted_transfer_is_reported_and_leaves_the_target_alone() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let device = FakeDevice::new(DEVICE, Platform::FireOs6, tx);
    device.state().corrupt_writes = true;
    let (mut shell, tools) = ready_shell(&device).await;
    let bytes = data(5000);
    let error = transfer_file(
        &mut shell,
        &tools,
        &bytes,
        &dev_path("/data/local/bin/server_b"),
        FileMode::EXECUTABLE,
        &limits(),
        &mut |_, _| {},
    )
    .await
    .unwrap_err();
    let mut corrupted = bytes.clone();
    corrupted[0] ^= 0xff;
    assert_eq!(
        error,
        TransferError::Mismatch {
            expected: Md5::of(&bytes),
            reported: Md5::of(&corrupted).to_string(),
        }
    );
    assert_eq!(
        device.file("/data/local/bin/server_b").unwrap().0,
        fake_device::OLDER_BUILD
    );
    assert!(device.file("/data/local/bin/server_b.part").is_none());
}

#[tokio::test]
async fn missing_tools_are_named() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let device = FakeDevice::new(DEVICE, Platform::FireOs6, tx);
    device.state().has_base64 = false;
    let mut shell = DeviceShell::new(device.shell());
    let error = check_tools(&mut shell, &limits()).await.unwrap_err();
    assert_eq!(
        error,
        TransferError::MissingTool {
            tool: transfer::Tool::Base64,
            output: "/system/bin/sh: base64: not found\n".into(),
        }
    );

    device.state().has_base64 = true;
    device.state().has_md5sum = false;
    let mut shell = DeviceShell::new(device.shell());
    assert!(matches!(
        check_tools(&mut shell, &limits()).await,
        Err(TransferError::MissingTool {
            tool: transfer::Tool::Md5sum,
            ..
        })
    ));
}

#[tokio::test]
async fn a_full_disk_fails_the_write() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let device = FakeDevice::new(DEVICE, Platform::FireOs6, tx);
    device.state().full_disk = true;
    let (mut shell, tools) = ready_shell(&device).await;
    let error = transfer_file(
        &mut shell,
        &tools,
        &data(100),
        &dev_path("/data/local/bin/server_b"),
        FileMode::EXECUTABLE,
        &limits(),
        &mut |_, _| {},
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, TransferError::WriteFailed { output } if output.contains("No space left"))
    );
    assert_eq!(
        device.file("/data/local/bin/server_b").unwrap().0,
        fake_device::OLDER_BUILD
    );
}

#[tokio::test]
async fn an_interrupted_transfer_leaves_the_target_intact() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let device = FakeDevice::new(DEVICE, Platform::FireOs6, tx);
    device.state().drop_after_chunks = Some(1);
    let (mut shell, tools) = ready_shell(&device).await;
    let error = transfer_file(
        &mut shell,
        &tools,
        &data(CHUNK_BYTES * 3),
        &dev_path("/data/local/bin/server_b"),
        FileMode::EXECUTABLE,
        &limits(),
        &mut |_, _| {},
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        TransferError::Shell(ShellFailure::Closed { .. })
    ));
    // The rollback slot is untouched; only the part file holds the first chunk.
    assert_eq!(
        device.file("/data/local/bin/server_b"),
        Some((fake_device::OLDER_BUILD.to_vec(), 0o755))
    );
    assert_eq!(
        device
            .file("/data/local/bin/server_b.part")
            .unwrap()
            .0
            .len(),
        CHUNK_BYTES
    );
}

// ── Update and rollback ─────────────────────────────────────────────────────

#[tokio::test]
async fn an_update_installs_the_inactive_slot_and_reports_the_new_version() {
    let h = Harness::start(Platform::FireOs6).await;
    let job = h.service.prepare_update(h.target()).unwrap();
    assert_eq!(job.version().as_str(), "fw-new");
    let outcome = job.run().await;
    assert!(
        matches!(&outcome, UpdateOutcome::Updated { from, to }
            if from.as_ref() == Some(&version("fw-old")) && *to == version("fw-new")),
        "{outcome:?}"
    );
    assert_eq!(h.device.link().as_deref(), Some("server_b"));
    assert_eq!(
        h.device.file("/data/local/bin/server_b"),
        Some((NEW_BUILD.to_vec(), 0o755))
    );
    // The active slot was never touched.
    assert_eq!(
        h.device.file("/data/local/bin/server_a").unwrap().0,
        OLD_BUILD
    );
    let script = std::fs::read(payload_dir().join(START_SCRIPT_FILE)).unwrap();
    assert_eq!(h.device.file(START_SCRIPT), Some((script, 0o755)));
    // A Dot gets no Echo Show lists.
    assert!(
        h.device
            .file(CrownList::Hide.device_path().as_str())
            .is_none()
    );
    assert_eq!(h.row().await.firmware_previous.as_deref(), Some("fw-old"));
    h.assert_no_unknown_commands();

    let events = h.events.events();
    assert!(events.iter().any(|e| matches!(e,
        FirmwareEvent::Progress { stage: Stage::Uploading { sent, total }, .. }
            if *sent == *total && *total == NEW_BUILD.len() as u64)));
    assert!(events.iter().any(|e| matches!(
        e,
        FirmwareEvent::Progress {
            stage: Stage::Switching {
                slot: layout::Slot::B
            },
            ..
        }
    )));
    assert!(matches!(
        events.last(),
        Some(FirmwareEvent::UpdateFinished {
            outcome: UpdateOutcome::Updated { .. },
            ..
        })
    ));
    assert!(!h.service.is_busy(&DeviceId(DEVICE.into())));
}

#[tokio::test]
async fn an_update_on_a_fire_os_show_syncs_its_package_lists() {
    let h = Harness::start(Platform::FireOs6Crown).await;
    let outcome = h.service.prepare_update(h.target()).unwrap().run().await;
    assert!(
        matches!(outcome, UpdateOutcome::Updated { .. }),
        "{outcome:?}"
    );
    for list in CrownList::ALL {
        let bytes = std::fs::read(payload_dir().join(list.file_name())).unwrap();
        assert_eq!(
            h.device.file(list.device_path().as_str()),
            Some((bytes, 0o644)),
            "{}",
            list.file_name()
        );
    }
    h.assert_no_unknown_commands();
}

#[tokio::test]
async fn the_start_script_is_not_sent_when_its_md5_matches() {
    let h = Harness::start(Platform::FireOs6).await;
    let script = std::fs::read(payload_dir().join(START_SCRIPT_FILE)).unwrap();
    h.device.state().nodes.insert(
        START_SCRIPT.into(),
        Node::File {
            data: script,
            mode: 0o755,
        },
    );
    let outcome = h.service.prepare_update(h.target()).unwrap().run().await;
    assert!(
        matches!(outcome, UpdateOutcome::Updated { .. }),
        "{outcome:?}"
    );
    let writes = h.device.state().heredoc_writes.clone();
    assert!(
        writes.iter().all(|p| p == "/data/local/bin/server_b.part"),
        "{writes:?}"
    );

    // A different script on the device is replaced.
    let h = Harness::start(Platform::FireOs6).await;
    h.service.prepare_update(h.target()).unwrap().run().await;
    assert!(
        h.device
            .state()
            .heredoc_writes
            .contains(&"/data/local/bin/start_server.sh.part".to_owned())
    );
}

#[tokio::test]
async fn an_automatic_rollback_is_reported_with_the_supervisor_log() {
    let h = Harness::start(Platform::FireOs6).await;
    h.device
        .state()
        .builds
        .get_mut(&Md5::of(NEW_BUILD))
        .unwrap()
        .starts = false;
    let outcome = h.service.prepare_update(h.target()).unwrap().run().await;
    let UpdateOutcome::AutoRolledBack {
        attempted,
        running,
        supervisor_log: SupervisorLog::Read(log),
    } = &outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!(*attempted, version("fw-new"));
    assert_eq!(*running, version("fw-old"));
    assert!(log.contains("fast-exit 3/3"), "{log}");
    assert!(log.contains("rollback server_b -> server_a"), "{log}");
    assert_eq!(h.device.link().as_deref(), Some("server_a"));
    // The rollback slot holds the build that failed to start.
    assert_eq!(h.row().await.firmware_previous, None);
    assert!(matches!(
        h.events.events().last(),
        Some(FirmwareEvent::UpdateFinished {
            outcome: UpdateOutcome::AutoRolledBack { .. },
            ..
        })
    ));
    h.assert_no_unknown_commands();
}

#[tokio::test]
async fn an_update_whose_transfer_fails_changes_nothing() {
    let h = Harness::start(Platform::FireOs6).await;
    h.device.state().corrupt_writes = true;
    let outcome = h.service.prepare_update(h.target()).unwrap().run().await;
    let UpdateOutcome::Failed(FirmwareFailure::Transfer { path, error }) = &outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(path.as_str(), "/data/local/bin/server_b");
    assert!(matches!(error, TransferError::Mismatch { .. }));
    assert_eq!(h.device.link().as_deref(), Some("server_a"));
    assert_eq!(
        h.device.file("/data/local/bin/server_b").unwrap().0,
        fake_device::OLDER_BUILD
    );
    assert_eq!(h.row().await.firmware_previous, None, "never written");
    assert!(outcome_message(&outcome).contains("arrived corrupted"));
}

fn outcome_message(outcome: &UpdateOutcome) -> String {
    match outcome {
        UpdateOutcome::Failed(failure) => failure.to_string(),
        other => format!("{other:?}"),
    }
}

#[tokio::test]
async fn an_update_refuses_an_unknown_slot_layout() {
    let h = Harness::start(Platform::FireOs6).await;
    h.device.state().nodes.insert(
        "/data/local/bin/server".into(),
        Node::Link("/data/local/bin/server_a".into()),
    );
    let outcome = h.service.prepare_update(h.target()).unwrap().run().await;
    assert!(
        matches!(&outcome, UpdateOutcome::Failed(FirmwareFailure::UnknownSlot { link })
            if link.trim() == "/data/local/bin/server_a"),
        "{outcome:?}"
    );
    assert!(h.device.state().heredoc_writes.is_empty());
}

#[tokio::test]
async fn updates_are_refused_while_busy_or_already_current() {
    let h = Harness::start(Platform::FireOs6).await;
    let job = h.service.prepare_update(h.target()).unwrap();
    assert!(matches!(
        h.service.prepare_update(h.target()),
        Err(FirmwareFailure::Busy)
    ));
    assert!(matches!(
        h.service.debloat(&h.target()).await,
        Err(FirmwareFailure::Busy)
    ));
    drop(job);
    let mut current = h.target();
    current.running = Some(version("fw-new"));
    assert!(matches!(
        h.service.prepare_update(current),
        Err(FirmwareFailure::AlreadyCurrent)
    ));
}

#[tokio::test]
async fn an_offline_device_fails_without_a_shell() {
    let h = Harness::start(Platform::FireOs6).await;
    h.device.state().online = false;
    let outcome = h.service.prepare_update(h.target()).unwrap().run().await;
    assert!(matches!(
        outcome,
        UpdateOutcome::Failed(FirmwareFailure::ShellOpen(ShellOpenError::NotConnected))
    ));
}

#[tokio::test]
async fn a_bundled_binary_changed_since_startup_is_refused() {
    let h = Harness::start(Platform::FireOs6).await;
    std::fs::write(h.firmware_dir.path().join(SERVER_FILE), b"\x7fELF swapped").unwrap();
    let outcome = h.service.prepare_update(h.target()).unwrap().run().await;
    assert!(
        matches!(&outcome, UpdateOutcome::Failed(FirmwareFailure::BundledFirmware(e))
            if matches!(**e, BundledFirmwareError::Changed { .. })),
        "{outcome:?}"
    );
    assert!(h.device.state().heredoc_writes.is_empty());
    let error = h.service.provision_firmware().await.unwrap_err();
    assert_eq!(error.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn a_manual_rollback_switches_back_to_the_other_slot() {
    let h = Harness::start(Platform::FireOs6).await;
    let outcome = h.service.prepare_rollback(h.target()).unwrap().run().await;
    assert!(
        matches!(&outcome, RollbackOutcome::RolledBack { from, to }
            if from.as_ref() == Some(&version("fw-old")) && *to == version("fw-older")),
        "{outcome:?}"
    );
    assert_eq!(h.device.link().as_deref(), Some("server_b"));
    // Nothing was transferred: a rollback only flips the link.
    assert!(h.device.state().heredoc_writes.is_empty());
    assert_eq!(h.row().await.firmware_previous.as_deref(), Some("fw-old"));
    assert!(matches!(
        h.events.events().last(),
        Some(FirmwareEvent::RollbackFinished {
            outcome: RollbackOutcome::RolledBack { .. },
            ..
        })
    ));
    h.assert_no_unknown_commands();
}

#[tokio::test]
async fn a_rollback_to_a_slot_that_fails_to_start_is_reverted_by_the_device() {
    let h = Harness::start(Platform::FireOs6).await;
    h.device
        .state()
        .builds
        .get_mut(&Md5::of(fake_device::OLDER_BUILD))
        .unwrap()
        .starts = false;
    let outcome = h.service.prepare_rollback(h.target()).unwrap().run().await;
    assert!(
        matches!(&outcome, RollbackOutcome::Reverted { running, supervisor_log: SupervisorLog::Read(log) }
            if *running == version("fw-old") && log.contains("rollback server_b -> server_a")),
        "{outcome:?}"
    );
    assert_eq!(h.device.link().as_deref(), Some("server_a"));
    assert_eq!(h.row().await.firmware_previous, None);
}

#[tokio::test]
async fn a_rollback_needs_an_executable_other_slot() {
    let h = Harness::start(Platform::FireOs6).await;
    h.device.state().nodes.remove("/data/local/bin/server_b");
    let outcome = h.service.prepare_rollback(h.target()).unwrap().run().await;
    assert!(matches!(
        outcome,
        RollbackOutcome::Failed(FirmwareFailure::SlotMissing {
            slot: layout::Slot::B
        })
    ));
    assert_eq!(h.device.link().as_deref(), Some("server_a"));
}

// ── The install queue ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_queued_update_runs_on_the_next_connection() {
    let h = Harness::start(Platform::FireOs6).await;
    let device = DeviceId(DEVICE.into());
    let at = h.service.queue_update(&device).await.unwrap();
    assert_eq!(h.row().await.update_queued_at, Some(at));
    // Queuing again keeps the first time.
    assert_eq!(h.service.queue_update(&device).await.unwrap(), at);
    assert!(h.events.events().iter().any(|e| matches!(e,
        FirmwareEvent::QueueChanged { queued_at: Some(q), .. } if *q == at)));

    let job = h
        .service
        .queued_update_on_connect(h.target())
        .await
        .unwrap()
        .expect("the queued install starts");
    assert_eq!(h.row().await.update_queued_at, None);
    assert!(matches!(
        h.events.events().last(),
        Some(FirmwareEvent::QueueChanged {
            queued_at: None,
            ..
        })
    ));
    assert!(matches!(job.run().await, UpdateOutcome::Updated { .. }));

    // Nothing queued: nothing runs.
    let mut after = h.target();
    after.running = Some(version("fw-new"));
    assert!(
        h.service
            .queued_update_on_connect(after.clone())
            .await
            .unwrap()
            .is_none()
    );

    // Queued but already current: the queue is cleared and nothing runs.
    h.service.queue_update(&device).await.unwrap();
    assert!(
        h.service
            .queued_update_on_connect(after)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(h.row().await.update_queued_at, None);
}

#[tokio::test]
async fn a_queued_update_waits_while_the_device_is_busy_and_can_be_cancelled() {
    let h = Harness::start(Platform::FireOs6).await;
    let device = DeviceId(DEVICE.into());
    h.service.queue_update(&device).await.unwrap();
    let running = h.service.prepare_rollback(h.target()).unwrap();
    assert!(
        h.service
            .queued_update_on_connect(h.target())
            .await
            .unwrap()
            .is_none()
    );
    assert!(h.row().await.update_queued_at.is_some(), "left for later");
    drop(running);
    assert!(h.service.cancel_queued_update(&device).await.unwrap());
    assert!(!h.service.cancel_queued_update(&device).await.unwrap());
    assert_eq!(h.row().await.update_queued_at, None);

    let unknown = DeviceId("G0UNKNOWN".into());
    assert!(matches!(
        h.service.queue_update(&unknown).await,
        Err(FirmwareFailure::Store(StoreError::UnknownDevice(_)))
    ));
}

// ── Debloat ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn debloat_syncs_the_script_and_parses_the_show_result() {
    let h = Harness::start(Platform::FireOs6Crown).await;
    h.device.state().debloat_output = "HIDE_REASSERTED:com.amazon.x STILL_VISIBLE:\n\
        MIC_REASSERTED: STILL_ALLOWED:com.amazon.y\n\
        DEBLOAT_STOPPED:17 STILL_RUNNING:\n"
        .into();
    let report = h.service.debloat(&h.target()).await.unwrap();
    assert_eq!(report.stopped, 17);
    assert_eq!(report.hide.unwrap().rehidden, ["com.amazon.x"]);
    assert_eq!(report.microphones.unwrap().still_allowed, ["com.amazon.y"]);
    let script = std::fs::read(payload_dir().join(START_SCRIPT_FILE)).unwrap();
    assert_eq!(h.device.file(START_SCRIPT).unwrap().0, script);
    assert!(
        h.device
            .file(CrownList::MicDeny.device_path().as_str())
            .is_some()
    );
    h.assert_no_unknown_commands();
}

#[tokio::test]
async fn debloat_reports_a_missing_result_and_refuses_lineage() {
    let h = Harness::start(Platform::FireOs6).await;
    h.device.state().debloat_output = "sh: oops\n".into();
    assert!(matches!(
        h.service.debloat(&h.target()).await,
        Err(FirmwareFailure::Debloat(_))
    ));
    let h = Harness::start(Platform::Lineage18Crown).await;
    assert!(matches!(
        h.service.debloat(&h.target()).await,
        Err(FirmwareFailure::NothingToDebloat)
    ));
    assert!(h.device.state().commands.is_empty());
}

// ── Secure link ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn secure_link_writes_credentials_with_their_modes_and_bounces_the_link() {
    let h = Harness::start(Platform::FireOs6).await;
    h.service
        .secure_link(&DeviceId(DEVICE.into()))
        .await
        .unwrap();
    let token = h.row().await.token.expect("a token was minted");
    assert_eq!(
        h.device.file(CA_PEM),
        Some((CA.as_bytes().to_vec(), FileMode::READABLE.bits()))
    );
    assert_eq!(
        h.device.file(LINK_TOKEN),
        Some((token.expose().as_bytes().to_vec(), FileMode::PRIVATE.bits()))
    );
    // Neither file was ever readable by others on its way.
    let created = h.device.state().created_modes.clone();
    assert_eq!(
        created,
        [
            (format!("{CA_PEM}.part"), 0o600),
            (format!("{LINK_TOKEN}.part"), 0o600)
        ]
    );
    assert_eq!(h.device.state().disconnects, 1);
    h.assert_no_unknown_commands();

    // Again: the same token, not a new one.
    h.service
        .secure_link(&DeviceId(DEVICE.into()))
        .await
        .unwrap();
    assert_eq!(h.row().await.token, Some(token));
}

#[tokio::test]
async fn secure_link_needs_a_tls_listener() {
    let h = Harness::start_with(Platform::FireOs6, None).await;
    assert!(matches!(
        h.service.secure_link(&DeviceId(DEVICE.into())).await,
        Err(FirmwareFailure::TlsUnavailable)
    ));
    assert!(h.device.state().commands.is_empty());
    assert_eq!(h.row().await.token, None);
}

// ── Provisioning responders ─────────────────────────────────────────────────

#[tokio::test]
async fn the_wizard_gets_the_payloads_as_stored() {
    let h = Harness::start(Platform::FireOs6).await;
    let script = h.service.provision_start_script().await.unwrap();
    assert_eq!(
        script.bytes,
        std::fs::read(payload_dir().join(START_SCRIPT_FILE)).unwrap()
    );

    let lists = h.service.provision_crown_lists().await.unwrap();
    let json = serde_json::to_value(&lists).unwrap();
    assert_eq!(json["dir"], ETC_DIR);
    assert_eq!(json["tiers_file"], CROWN_TIERS);
    assert_eq!(json["hide"][0]["tier"], "1");
    assert_eq!(json["hide"][0]["file"], "crown_hide_packages.txt");
    assert_eq!(json["hide"][1]["tier"], "2");
    assert_eq!(json["mic_deny"]["file"], "crown_mic_deny_packages.txt");
    let text = std::fs::read_to_string(payload_dir().join("crown_hide_packages.txt")).unwrap();
    assert_eq!(json["hide"][0]["text"], text);
    assert_eq!(
        json["hide"][0]["packages"],
        serde_json::to_value(list_packages(&text)).unwrap()
    );

    let firmware = h.service.provision_firmware().await.unwrap();
    assert_eq!(&firmware.bytes[..], NEW_BUILD);
    let headers = firmware.headers();
    assert_eq!(headers["x-firmware-version"], "fw-new");
    assert_eq!(
        headers["x-firmware-sha256"],
        echomuse_protocol::Sha256::of(NEW_BUILD).as_str()
    );
    assert_eq!(headers["content-type"], "application/octet-stream");
}

#[tokio::test]
async fn the_wizard_gets_credentials_for_a_device_not_seen_yet() {
    let h = Harness::start(Platform::FireOs6).await;
    let creds = h
        .service
        .provision_tls_credentials(TlsCredentialsRequest {
            device_id: "G0NEWDEVICE".into(),
        })
        .await
        .unwrap();
    assert!(!format!("{creds:?}").contains(creds.token.expose()));
    let json = serde_json::to_value(&creds).unwrap();
    assert_eq!(json["ca_pem"], CA);
    assert_eq!(json["dir"], ETC_DIR);
    let row =
        h.db.read(GetDevice {
            device: DeviceId("G0NEWDEVICE".into()),
        })
        .await
        .unwrap()
        .expect("a pending row");
    assert!(!row.approved);
    assert_eq!(json["token"], row.token.unwrap().expose());

    let bad = h
        .service
        .provision_tls_credentials(TlsCredentialsRequest {
            device_id: "bad id".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(bad.status(), http::StatusCode::BAD_REQUEST);

    let h = Harness::start_with(Platform::FireOs6, None).await;
    let error = h
        .service
        .provision_tls_credentials(TlsCredentialsRequest {
            device_id: "G0NEWDEVICE".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.status(), http::StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn diagnostics_come_back_with_the_controller_side() {
    let h = Harness::start(Platform::FireOs6).await;
    let upload: DiagnosticsUpload = serde_json::from_value(serde_json::json!({
        "step": "install_em",
        "error": "Install verification failed",
        "probes": {"props": "ro.serialno=G0", "echomuse_service": "running"},
        "transcript": ["Fetching…", "Failed"],
        "selected_ssid": null,
    }))
    .unwrap();
    let report = h.service.provision_diagnostics(upload).unwrap();
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["step"], "install_em");
    assert_eq!(json["controller_version"], "test");
    assert_eq!(json["bundled_firmware"]["version"], "fw-new");
    assert_eq!(json["probes"]["echomuse_service"], "running");
    assert_eq!(json["transcript"][1], "Failed");
    assert!(json["selected_ssid"].is_null());
    assert!(json["received_utc_ms"].as_u64().is_some());

    let too_many: DiagnosticsUpload = serde_json::from_value(serde_json::json!({
        "step": "",
    }))
    .unwrap();
    assert_eq!(
        h.service
            .provision_diagnostics(too_many)
            .unwrap_err()
            .status(),
        http::StatusCode::BAD_REQUEST
    );
}
