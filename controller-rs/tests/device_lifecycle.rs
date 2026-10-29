use std::{
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use echomuse_controller::{
    app::Controller,
    device::{DeviceRuntime, DeviceRuntimeConfig, DeviceSnapshot, Lifecycle, RuntimeServices},
    speech_workers::{SpeechWorkerKind, WorkerLimits},
    supervision::{
        Availability, FatalHook, RestartPolicy, Supervision, SupervisionOptions, WatchdogConfig,
    },
};
use echomuse_core::types::{DeviceId, QueueLimits, RuntimeError, TurnId};
use tokio::sync::oneshot;

fn config() -> DeviceRuntimeConfig {
    DeviceRuntimeConfig {
        ingress: QueueLimits {
            max_age: Duration::from_secs(5),
            ..QueueLimits::default()
        },
        workers: WorkerLimits::default(),
        shutdown_timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn replacement_revokes_work_and_old_disconnect_cannot_remove_new_session() {
    let runtime = DeviceRuntime::start(DeviceId("kitchen".into()), config())
        .await
        .unwrap();
    let device = runtime.handle();
    let old = device.attach(false).await.unwrap();
    let old_scope = old.scope().unwrap();
    let turn = old_scope.for_turn(TurnId(1));
    let new = device.attach(false).await.unwrap();
    assert!(old_scope.is_cancelled());
    assert!(turn.is_cancelled());
    assert_eq!(old.detach().await, Err(RuntimeError::StaleSession));
    assert_eq!(old.set_muted(true).await, Err(RuntimeError::StaleSession));
    assert_eq!(device.snapshot().session, Some(new.id()));
    assert!(!new.scope().unwrap().is_cancelled());
    runtime.shutdown().await.unwrap();
    assert!(new.scope().is_err());
    assert_eq!(device.snapshot().lifecycle, Lifecycle::Stopped);
}

#[tokio::test]
async fn mute_invalidates_running_work_without_waiting_for_its_thread() {
    let runtime = DeviceRuntime::start(DeviceId("office".into()), config())
        .await
        .unwrap();
    let device = runtime.handle();
    let session = device.attach(false).await.unwrap();
    let original_scope = session.scope().unwrap();
    let (started, running) = oneshot::channel();
    let (release, wait) = mpsc::channel();
    let work = session
        .submit(SpeechWorkerKind::WakeVerification, 16, move || {
            let _ = started.send(());
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            "must not be delivered"
        })
        .unwrap();
    running.await.unwrap();
    session.set_muted(true).await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(1), work.result()).await;
    // Always release the worker before asserting so a failure cannot strand a thread.
    release.send(()).unwrap();
    assert_eq!(outcome.unwrap(), Err(RuntimeError::Cancelled));
    assert_eq!(device.snapshot().muted, Some(true));
    assert!(matches!(session.scope(), Err(RuntimeError::Muted)));
    session.set_muted(false).await.unwrap();
    assert!(original_scope.is_cancelled());
    assert!(!session.scope().unwrap().is_cancelled());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn muted_hello_prevents_capture_until_unmuted() {
    let runtime = DeviceRuntime::start(DeviceId("bedroom".into()), config())
        .await
        .unwrap();
    let session = runtime.handle().attach(true).await.unwrap();
    assert!(matches!(
        session.submit(SpeechWorkerKind::MicrophoneAnalysis, 0, || panic!(
            "muted work ran"
        )),
        Err(RuntimeError::Muted)
    ));
    session.set_muted(false).await.unwrap();
    let work = session
        .submit(SpeechWorkerKind::MicrophoneAnalysis, 6, || {
            [2_i16, -3, 4]
                .into_iter()
                .map(i32::from)
                .map(|x| x * x)
                .sum::<i32>()
        })
        .unwrap();
    assert_eq!(work.result().await.unwrap(), 29);
    runtime.shutdown().await.unwrap();
}

/// Failure handling with the restart delays given and a fatal hook that records.
fn supervision(
    initial: Duration,
    call_deadline: Duration,
    recovery_deadline: Duration,
) -> (Supervision, Arc<Mutex<Vec<String>>>) {
    let reasons = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&reasons);
    let supervision = Supervision::start(SupervisionOptions {
        fatal: FatalHook::new(move |reason| recorded.lock().unwrap().push(reason.to_owned())),
        watchdog: WatchdogConfig {
            call_deadline,
            recovery_deadline,
        },
        restart: RestartPolicy {
            initial,
            max: initial * 4,
            stable: Duration::from_secs(60),
        },
    })
    .unwrap();
    (supervision, reasons)
}

fn supervised(supervision: Supervision) -> Controller {
    Controller::with_services(
        config(),
        RuntimeServices {
            supervision: Some(supervision),
            ..RuntimeServices::default()
        },
    )
    .unwrap()
}

async fn wait_for(
    snapshots: &mut tokio::sync::watch::Receiver<DeviceSnapshot>,
    check: impl FnMut(&DeviceSnapshot) -> bool,
) -> DeviceSnapshot {
    tokio::time::timeout(Duration::from_secs(10), snapshots.wait_for(check))
        .await
        .expect("in time")
        .unwrap()
        .clone()
}

fn restart_delay(snapshot: &DeviceSnapshot) -> Option<Duration> {
    match &snapshot.health.availability {
        Availability::Restarting { retry_in, .. } => Some(*retry_in),
        _ => None,
    }
}

#[tokio::test]
async fn a_worker_panic_rebuilds_only_that_runtime_with_doubling_delays() {
    let (supervision, fatal) = supervision(
        Duration::from_millis(200),
        Duration::from_secs(60),
        Duration::from_secs(120),
    );
    let mut controller = supervised(supervision);
    let first = controller
        .add_device(DeviceId("first".into()))
        .await
        .unwrap();
    let other = controller
        .add_device(DeviceId("other".into()))
        .await
        .unwrap();
    let session = first.attach(false).await.unwrap();
    let scope = session.scope().unwrap();
    let other_session = other.attach(false).await.unwrap();
    let mut status = first.subscribe();
    let mut runtime = first.runtime();
    let mut delays = Vec::new();
    for round in 1..=2 {
        let current = controller.device(&DeviceId("first".into())).unwrap();
        let session = if round == 1 {
            session.clone()
        } else {
            current.attach(false).await.unwrap()
        };
        let work = session
            .submit(SpeechWorkerKind::SpanDecoding, 0, || {
                panic!("intentional worker failure")
            })
            .unwrap();
        assert!(work.result().await.is_err());
        let failed = wait_for(&mut status, |state| restart_delay(state).is_some()).await;
        assert_eq!(failed.lifecycle, Lifecycle::Failed);
        assert_eq!(failed.session, None);
        assert_eq!(failed.health.errors.failures, round);
        assert_eq!(
            failed.health.availability.reason(),
            Some("the device runtime failed: worker thread panicked")
        );
        assert!(session.scope().is_err(), "the session was revoked");
        assert!(matches!(
            current.attach(false).await,
            Err(RuntimeError::Closed)
        ));
        delays.push(restart_delay(&failed).unwrap());
        let rebuilt = wait_for(&mut status, |state| {
            state.runtime != runtime && state.health.availability == Availability::Available
        })
        .await;
        assert_eq!(rebuilt.lifecycle, Lifecycle::Ready);
        runtime = rebuilt.runtime;
        assert_eq!(
            controller
                .device(&DeviceId("first".into()))
                .unwrap()
                .runtime(),
            runtime
        );
    }
    assert_eq!(
        delays,
        [Duration::from_millis(200), Duration::from_millis(400)]
    );
    assert!(scope.is_cancelled());
    // The other device kept its runtime and its session throughout.
    assert_eq!(other.snapshot().lifecycle, Lifecycle::Attached);
    assert_eq!(other.snapshot().health.errors.failures, 0);
    assert!(!other_session.scope().unwrap().is_cancelled());
    // The rebuilt runtime serves sessions and fresh workers.
    let current = controller.device(&DeviceId("first".into())).unwrap();
    let fresh = current.attach(false).await.unwrap();
    let answer = fresh
        .submit(SpeechWorkerKind::SpanDecoding, 0, || 5_u32)
        .unwrap();
    assert_eq!(answer.result().await, Ok(5));
    assert!(fatal.lock().unwrap().is_empty());
    controller
        .remove_device(&DeviceId("first".into()))
        .await
        .unwrap();
    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_device_removed_while_it_waits_to_be_rebuilt_stays_removed() {
    let (supervision, _) = supervision(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(120),
    );
    let mut controller = supervised(supervision);
    let id = DeviceId("first".into());
    let first = controller.add_device(id.clone()).await.unwrap();
    let session = first.attach(false).await.unwrap();
    let mut status = first.subscribe();
    let _ = session
        .submit(SpeechWorkerKind::MicrophoneAnalysis, 0, || {
            panic!("intentional worker failure")
        })
        .unwrap()
        .result()
        .await;
    wait_for(&mut status, |state| restart_delay(state).is_some()).await;
    tokio::time::timeout(Duration::from_secs(5), controller.remove_device(&id))
        .await
        .expect("removal does not wait for the backoff")
        .unwrap();
    assert!(controller.device(&id).is_none());
    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_stuck_call_marks_the_speech_path_and_then_fires_the_fatal_hook() {
    let (supervision, fatal) = supervision(
        Duration::from_secs(1),
        Duration::from_millis(100),
        Duration::from_millis(500),
    );
    let mut controller = supervised(supervision);
    let device = controller
        .add_device(DeviceId("kitchen".into()))
        .await
        .unwrap();
    let other = controller
        .add_device(DeviceId("hall".into()))
        .await
        .unwrap();
    let session = device.attach(false).await.unwrap();
    let mut status = device.subscribe();
    let (release, gate) = mpsc::channel::<()>();
    let stuck = session
        .submit(SpeechWorkerKind::WakeVerification, 0, move || {
            let _ = gate.recv();
            1_u32
        })
        .unwrap();
    let marked = wait_for(&mut status, |state| {
        matches!(
            state.health.availability,
            Availability::SpeechUnavailable { .. }
        )
    })
    .await;
    assert_eq!(
        marked.health.availability.reason(),
        Some("the wake verification speech worker is stuck in a model call past its deadline")
    );
    assert_eq!(marked.lifecycle, Lifecycle::Attached, "the session stays");
    assert_eq!(
        other.snapshot().health.availability,
        Availability::Available
    );

    let reason = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(reason) = fatal.lock().unwrap().first().cloned() {
                return reason;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the watchdog fires");
    assert!(reason.contains("device kitchen"), "{reason}");
    assert!(reason.contains("wake verification"), "{reason}");

    release.send(()).unwrap();
    assert_eq!(stuck.result().await, Err(RuntimeError::Overdue));
    let recovered = wait_for(&mut status, |state| {
        state.health.availability == Availability::Available
    })
    .await;
    assert_eq!(recovered.health.errors.failures, 1);
    assert_eq!(fatal.lock().unwrap().len(), 1, "fired once");
    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn registry_rejects_duplicates_and_removal_revokes_existing_handles() {
    let mut controller = Controller::new(config()).unwrap();
    let id = DeviceId("living-room".into());
    let device = controller.add_device(id.clone()).await.unwrap();
    let old_runtime = device.snapshot().runtime;
    let session = device.attach(false).await.unwrap();
    let scope = session.scope().unwrap();
    assert!(matches!(
        controller.add_device(id.clone()).await,
        Err(RuntimeError::DuplicateDevice)
    ));
    controller.remove_device(&id).await.unwrap();
    assert!(scope.is_cancelled());
    assert!(matches!(
        device.attach(false).await,
        Err(RuntimeError::Closed)
    ));
    assert!(controller.device(&id).is_none());
    let replacement = controller.add_device(id).await.unwrap();
    assert_ne!(replacement.snapshot().runtime, old_runtime);
    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_work_ends_only_its_session() {
    let runtime = DeviceRuntime::start(DeviceId("office".into()), config())
        .await
        .unwrap();
    let device = runtime.handle();
    let session = device.attach(false).await.unwrap();
    let scope = session.scope().unwrap();
    assert!(matches!(
        session.submit(SpeechWorkerKind::ReferenceScoring, usize::MAX, || ()),
        Err(RuntimeError::Overloaded)
    ));
    assert!(scope.is_cancelled());
    let next = device.attach(false).await.unwrap();
    assert!(!next.scope().unwrap().is_cancelled());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_the_last_session_handle_revokes_capture() {
    let runtime = DeviceRuntime::start(DeviceId("office".into()), config())
        .await
        .unwrap();
    let device = runtime.handle();
    let session = device.attach(false).await.unwrap();
    let scope = session.scope().unwrap();
    let clone = session.clone();
    let mut status = device.subscribe();
    drop(session);
    assert!(!scope.is_cancelled());
    drop(clone);
    assert!(scope.is_cancelled());
    tokio::time::timeout(
        Duration::from_secs(5),
        status.wait_for(|state| state.lifecycle == Lifecycle::Ready),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(device.snapshot().session, None);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn turn_cancellation_does_not_revoke_the_session() {
    let runtime = DeviceRuntime::start(DeviceId("office".into()), config())
        .await
        .unwrap();
    let device = runtime.handle();
    let session = device.attach(false).await.unwrap();
    let capture = session.scope().unwrap();
    let turn = capture.for_turn(TurnId(42));
    turn.cancel();
    assert!(matches!(
        session.submit_scoped(turn, SpeechWorkerKind::SpanDecoding, 0, || panic!(
            "cancelled turn ran"
        )),
        Err(RuntimeError::Cancelled)
    ));
    assert!(!capture.is_cancelled());
    assert_eq!(device.snapshot().session, Some(session.id()));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_late_attach_is_refused_without_failing_the_runtime() {
    // Every command is late: the runtime's thread is as slow as a starved host's.
    let slow = DeviceRuntimeConfig {
        ingress: QueueLimits {
            max_age: Duration::from_nanos(1),
            ..QueueLimits::default()
        },
        ..config()
    };
    let runtime = DeviceRuntime::start(DeviceId("kitchen".into()), slow)
        .await
        .unwrap();
    let device = runtime.handle();
    assert!(matches!(
        device.attach(false).await,
        Err(RuntimeError::Expired)
    ));
    let state = device.snapshot();
    assert_eq!(
        state.lifecycle,
        Lifecycle::Ready,
        "the runtime keeps running"
    );
    assert_eq!(state.failure, None);
    assert_eq!(state.session, None);
    runtime.shutdown().await.unwrap();
}
