//! What the dashboard learns from the device runtimes: each runtime's published snapshot
//! as the API's [`LiveDevice`], and the connection and state changes published as they
//! happen (§3.4). The device object's shape belongs to the API crate; this module only
//! translates the runtime's state into it. It also tells the dashboard of each new turn
//! the records commit (`turn_complete`).

use echomuse_api::{
    ApiEvent, EventPublisher, LiveDevice, LiveFailure, LiveHealth, LiveSession, LiveVolume,
};
use echomuse_link::Transport;
use tokio::sync::{broadcast, watch};

use crate::{
    device::{DeviceSnapshot, Lifecycle, Volume},
    supervision::{Availability, DeviceHealth},
};

/// The part of the device object the runtime's snapshot decides.
pub fn live_device(snapshot: &DeviceSnapshot) -> LiveDevice {
    let session = snapshot
        .link
        .as_ref()
        .filter(|_| snapshot.lifecycle == Lifecycle::Attached)
        .map(|link| LiveSession {
            ip: link.ip,
            platform: link.platform,
            capabilities: link.capabilities.clone(),
            tls: link.transport == Transport::Tls,
            firmware_version: link.firmware_version.clone(),
            os_version: link.os_version.clone(),
            muted: snapshot.muted,
            volume: link.volume.map(|volume| match volume {
                Volume::Level(level) => LiveVolume::Level(level),
                Volume::Unavailable => LiveVolume::Unavailable,
            }),
            stats: link.stats.clone(),
            installed_assets: link.installed_assets.clone(),
        });
    LiveDevice {
        // The port the satellite serves while it listens; otherwise the stored one shows.
        esphome_port: snapshot
            .esphome
            .voice_listening
            .map(|address| address.port()),
        session,
        health: Some(live_health(&snapshot.health)),
    }
}

/// The runtime's health as the device object shows it.
fn live_health(health: &DeviceHealth) -> LiveHealth {
    LiveHealth {
        unavailable: health.availability.reason().map(str::to_owned),
        restarting: matches!(health.availability, Availability::Restarting { .. }),
        recent_failures: health.errors.failures,
        last_failure: health.errors.last.as_ref().map(|failure| LiveFailure {
            reason: failure.reason.clone(),
            at: failure
                .at
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| {
                    i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
                }),
        }),
    }
}

/// Publishes one device's connection and state changes, across rebuilds of its runtime
/// (their snapshots share one channel), until the device's runtime is removed.
pub(crate) async fn publish_device_changes(
    mut snapshots: watch::Receiver<DeviceSnapshot>,
    publisher: EventPublisher,
) {
    let (device, mut previous) = {
        let snapshot = snapshots.borrow_and_update();
        (snapshot.device.clone(), live_device(&snapshot))
    };
    while snapshots.changed().await.is_ok() {
        let current = live_device(&snapshots.borrow_and_update());
        if current.connected() != previous.connected() {
            let device_id = device.clone();
            publisher.publish(&if current.connected() {
                ApiEvent::DeviceConnected { device_id }
            } else {
                ApiEvent::DeviceDisconnected { device_id }
            });
        }
        let state = current.state_patch_since(&previous);
        if !state.0.is_empty() {
            publisher.publish(&ApiEvent::DeviceUpdate {
                device_id: device.clone(),
                state,
            });
        }
        previous = current;
    }
}

/// Publishes `turn_complete` for each new `turns` row the records commit, until the records
/// service is gone. A notice missed by falling behind is not resent: the Activity views
/// also re-read their lists every 10 s.
pub(crate) async fn publish_turns_stored(
    mut turns: broadcast::Receiver<echomuse_records::TurnStored>,
    publisher: EventPublisher,
) {
    loop {
        match turns.recv().await {
            Ok(stored) => publisher.publish(&ApiEvent::TurnComplete {
                device_id: stored.device,
            }),
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                tracing::debug!(missed, "turn_complete notices skipped while behind");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}
