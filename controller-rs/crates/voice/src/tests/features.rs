//! The device `config` push, the timer arc, and the alert sound preview.

use echomuse_playback::{AlertPreviewSound, Command};
use echomuse_protocol::MessageType;

use super::rig::{Options, Rig, capabilities};
use crate::{Input, Outbound, PreviewId, PreviewOutcome};

fn configs(rig: &Rig) -> Vec<serde_json::Value> {
    rig.sent
        .iter()
        .filter_map(|(body, _)| match body {
            Outbound::Config(config) => Some(serde_json::to_value(config).expect("serializes")),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn config_goes_out_at_attach_and_again_only_when_it_changes() {
    let mut rig = Rig::new(Options {
        capabilities: capabilities(&["local_wake_chime"]),
        ..Options::default()
    });
    assert_eq!(configs(&rig).len(), 1, "sent once at the session's start");
    assert_eq!(configs(&rig)[0]["wakeSound"], false);
    // Unchanged settings send nothing.
    rig.handle(Input::Settings(std::sync::Arc::new(
        echomuse_settings::DeviceSettings::default(),
    )));
    assert_eq!(configs(&rig).len(), 1);
    let mut settings = echomuse_settings::DeviceSettings::default();
    settings.wakeword.wake_sound = true;
    rig.handle(Input::Settings(std::sync::Arc::new(settings)));
    let sent = configs(&rig);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["wakeSound"], true);
}

#[tokio::test(start_paused = true)]
async fn a_new_kiosk_token_reaches_a_kiosk_plugin_device() {
    let mut rig = Rig::new(Options {
        capabilities: capabilities(&["kiosk_plugin"]),
        ..Options::default()
    });
    assert!(configs(&rig)[0].get("kioskToken").is_none());
    rig.handle(Input::KioskToken(Some("token-1".to_owned())));
    assert_eq!(configs(&rig).last().unwrap()["kioskToken"], "token-1");
    let _ = MessageType::Config;
}

#[tokio::test(start_paused = true)]
async fn a_running_timer_lights_the_ring() {
    let mut rig = Rig::new(Options::default());
    let before = rig.sent.len();
    rig.handle(Input::Timers(echomuse_alerts::wire::TimerView {
        timers: vec![echomuse_alerts::wire::TimerViewEntry {
            id: "t1".to_owned(),
            name: None,
            total_ms: 60_000,
            remaining_ms: 30_000,
            active: true,
        }],
    }));
    assert!(
        rig.sent[before..]
            .iter()
            .any(|(body, _)| matches!(body, Outbound::Led(_))),
        "the timer arc was drawn"
    );
}

#[tokio::test(start_paused = true)]
async fn a_preview_starts_once_the_device_accepts_it_and_stops_on_request() {
    let mut rig = Rig::new(Options::default());
    let sha = echomuse_protocol::Sha256::parse(&"ab".repeat(32)).unwrap();
    rig.handle(Input::Preview {
        id: PreviewId(1),
        sound: AlertPreviewSound::Sound(sha.clone()),
    });
    let start = rig
        .requests
        .iter()
        .find_map(|(_, body, _)| match body {
            Outbound::Playback(Command::RenderStart(start)) => Some(start.clone()),
            _ => None,
        })
        .expect("render.start sent");
    assert_eq!(start.local_asset.as_deref(), Some(sha.as_str()));
    assert!(start.epoch.is_none());
    assert_eq!(rig.previews, vec![(PreviewId(1), PreviewOutcome::Started)]);
    rig.handle(Input::StopPreview { id: PreviewId(2) });
    assert_eq!(
        rig.previews.last(),
        Some(&(PreviewId(2), PreviewOutcome::Stopped { was_playing: true }))
    );
    rig.handle(Input::StopPreview { id: PreviewId(3) });
    assert_eq!(
        rig.previews.last(),
        Some(&(PreviewId(3), PreviewOutcome::Stopped { was_playing: false }))
    );
}
