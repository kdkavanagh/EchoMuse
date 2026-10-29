use std::{path::Path, process::ExitCode};

use echomuse_config::{ControllerConfig, Environment, addon};
use echomuse_controller::{
    control_plane::{ControlPlane, ControlPlaneOptions},
    wake_qualify::{PROBE_SUBCOMMAND, probe_child_main},
};
use echomuse_db::Startup;
use echomuse_speech::qualify::{QUALIFY_MODEL_SUBCOMMAND, child_main};
use tokio::signal::unix::{SignalKind, signal};

const HELP: &str = "EchoMuse controller\n\nUsage: echomuse-controller\n\nReads its settings from the environment (and /data/options.json in the Home Assistant\nadd-on), opens the database at DB_PATH, and serves devices, Home Assistant and the\ndashboard until SIGINT or SIGTERM.\n\nThe controller also starts itself as a short-lived child with qualify-model or\nqualify-wake-probes to run an uploaded wake model away from the main process.\n";

fn main() -> ExitCode {
    if let Some(argument) = std::env::args().nth(1) {
        match argument.as_str() {
            QUALIFY_MODEL_SUBCOMMAND => return child_main(),
            PROBE_SUBCOMMAND => return probe_child_main(),
            "--help" | "-h" => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            _ => {
                eprintln!("unknown argument {argument}; use --help");
                return ExitCode::FAILURE;
            }
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    // Several crates build TLS clients. One process-wide provider means a client that
    // relies on rustls' default never panics for lack of one; the crates that pass a
    // provider explicitly use this same ring implementation.
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        eprintln!("a TLS crypto provider was already installed; keeping it");
    }

    let mut env: Environment = std::env::vars().collect();
    match addon::merge_options_file(&mut env, Path::new(addon::OPTIONS_PATH)) {
        Ok(Some(merge)) => {
            for key in &merge.unknown_keys {
                eprintln!("add-on option {key} is not one this controller reads; ignored");
            }
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("configuration error: {error}");
            return ExitCode::FAILURE;
        }
    }
    let config = match ControllerConfig::from_env(&env, Some(env!("CARGO_PKG_VERSION"))) {
        Ok(config) => config,
        Err(errors) => {
            for error in &errors.0 {
                eprintln!("configuration error: {error}");
            }
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("the shared runtime did not start: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(config))
}

async fn run(config: ControllerConfig) -> ExitCode {
    // Install handlers before starting anything so a signal during startup is kept.
    let (mut terminate, mut interrupt) = match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(terminate), Ok(interrupt)) => (terminate, interrupt),
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("signal handlers could not be installed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let version = config.controller_version.clone();
    let plane = match ControlPlane::start(ControlPlaneOptions::new(config)).await {
        Ok(plane) => plane,
        Err(error) => {
            eprintln!("the controller cannot start: {error}");
            return ExitCode::FAILURE;
        }
    };
    let notes = plane.notes();
    match &notes.database {
        Startup::Created => eprintln!("database: created a new installation"),
        Startup::Existing { cutover_backup } => {
            eprintln!(
                "database: opened the existing installation (cutover backup: {cutover_backup:?})"
            )
        }
    }
    if let Some(tls) = &notes.tls {
        eprintln!(
            "device TLS: {} CA, server certificate {:?}",
            if tls.created_ca {
                "created a new"
            } else {
                "loaded the"
            },
            tls.server_cert
        );
    }
    eprintln!("runtime asset: sha256 {}", notes.runtime_asset.as_str());
    eprintln!(
        "advertising {} at {} (port {}{})",
        notes.advertised.name,
        notes.advertised.address,
        notes.advertised.port,
        notes
            .advertised
            .tls_port
            .map_or(String::new(), |port| format!(", TLS port {port}"))
    );
    eprintln!("device runtimes: {} approved", notes.approved_devices);
    eprintln!(
        "bundled firmware: {} ({} bytes, sha256 {})",
        notes.firmware.version.as_str(),
        notes.firmware.size,
        notes.firmware.sha256.as_str()
    );
    match &notes.wake_uploads_refused {
        None => eprintln!("wake-model uploads: qualified in child processes"),
        Some(reason) => eprintln!("wake-model uploads are refused: {reason}"),
    }
    eprintln!("dashboard API on {}", plane.api_address());
    match plane.device_tls_address() {
        Some(tls) => eprintln!(
            "device listeners on {} and {tls} (TLS)",
            plane.device_address()
        ),
        None => eprintln!("device listener on {}", plane.device_address()),
    }
    if let Some(token) = plane.setup_token() {
        eprintln!(
            "No dashboard user exists yet. Create the administrator with this setup token: {token}"
        );
    }
    eprintln!("EchoMuse controller {version} running; SIGINT or SIGTERM stops it.");

    tokio::select! {
        _ = terminate.recv() => eprintln!("SIGTERM received; shutting down"),
        _ = interrupt.recv() => eprintln!("SIGINT received; shutting down"),
    }
    let report = plane.shutdown().await;
    if report.is_clean() {
        eprintln!("controller stopped");
        ExitCode::SUCCESS
    } else {
        for problem in &report.problems {
            eprintln!("shutdown problem: {problem}");
        }
        ExitCode::FAILURE
    }
}
