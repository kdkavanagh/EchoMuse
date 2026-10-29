//! The device's `config` message (`device/internal/config`): the settings the firmware
//! applies itself. The firmware does not keep them across a restart, so the machine sends
//! the whole message after every `session.ready` and again whenever what it would send
//! changes (a settings change, a new kiosk token).
//!
//! Each field goes only to a device whose capabilities use it, as the firmware's own
//! comments and the controller's capability rules say: `wakeSound` to `local_wake_chime`
//! devices (the device plays the wake chime itself), `bleProxyEnabled` to `ble_proxy`
//! devices, the kiosk screen fields to `screen_wake` devices, the Kiosk Satellite token and
//! port to `kiosk_plugin` devices, and `turnViewLinger` to `turn_view_v1` devices.
//! `startupVolume` and `duckDb` go to every device.

use echomuse_protocol::{Capabilities, Capability, ControlBody, MessageType};
use echomuse_settings::DeviceSettings;
use serde::Serialize;

/// The `config` body. An absent field leaves the device's current value.
#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceConfig {
    /// The volume the device starts at; it seeds its volume only once per run.
    pub startup_volume: u8,
    /// How far dialog output ducks content, in dB (at most 0).
    pub duck_db: f64,
    /// The Home Assistant Bluetooth proxy (`ble_proxy` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ble_proxy_enabled: Option<bool>,
    /// The device plays the wake chime as an idle candidate opens (`local_wake_chime`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_sound: Option<bool>,
    /// Light the kiosk screen on wake (`screen_wake`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_wake: Option<bool>,
    /// The kiosk REST API; empty restores the firmware's default (`screen_wake`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_wake_url: Option<String>,
    /// The kiosk's remote administration password (`screen_wake`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_wake_password: Option<String>,
    /// Kiosk Satellite's remote API token (`kiosk_plugin`, once one is stored).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiosk_token: Option<String>,
    /// Kiosk Satellite's remote API port (`kiosk_plugin`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiosk_port: Option<u16>,
    /// Seconds the plugin's turn view stays up after a turn (`turn_view_v1`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_view_linger: Option<f64>,
}

impl ControlBody for DeviceConfig {
    const MESSAGE_TYPE: MessageType = MessageType::Config;
}

impl std::fmt::Debug for DeviceConfig {
    /// The kiosk password and token are secrets: shown only as set or not.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceConfig")
            .field("startup_volume", &self.startup_volume)
            .field("duck_db", &self.duck_db)
            .field("ble_proxy_enabled", &self.ble_proxy_enabled)
            .field("wake_sound", &self.wake_sound)
            .field("screen_wake", &self.screen_wake)
            .field("screen_wake_url", &self.screen_wake_url)
            .field(
                "screen_wake_password",
                &self.screen_wake_password.as_ref().map(|_| "…"),
            )
            .field("kiosk_token", &self.kiosk_token.as_ref().map(|_| "…"))
            .field("kiosk_port", &self.kiosk_port)
            .field("turn_view_linger", &self.turn_view_linger)
            .finish()
    }
}

impl DeviceConfig {
    /// What a device with `capabilities` is sent under `settings`; `kiosk_token` is the
    /// device row's Kiosk Satellite token, `None` while none is stored.
    pub fn new(
        capabilities: &Capabilities,
        settings: &DeviceSettings,
        kiosk_token: Option<&str>,
    ) -> Self {
        let has = |capability| capabilities.contains(capability);
        let screen = has(Capability::ScreenWake);
        let kiosk = has(Capability::KioskPlugin);
        Self {
            startup_volume: settings.state.startup_volume.get(),
            duck_db: settings.playback.duck_db.get(),
            ble_proxy_enabled: has(Capability::BleProxy)
                .then_some(settings.bluetooth.ble_proxy_enabled),
            wake_sound: has(Capability::LocalWakeChime).then_some(settings.wakeword.wake_sound),
            screen_wake: screen.then_some(settings.screen.screen_wake),
            screen_wake_url: screen.then(|| settings.screen.screen_wake_url.as_str().to_owned()),
            screen_wake_password: screen
                .then(|| settings.screen.screen_wake_password.expose().to_owned()),
            kiosk_token: kiosk
                .then_some(kiosk_token)
                .flatten()
                .filter(|token| !token.is_empty())
                .map(str::to_owned),
            kiosk_port: kiosk.then_some(settings.screen.kiosk_port.get()),
            turn_view_linger: has(Capability::TurnView)
                .then_some(settings.screen.turn_view_linger.get()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::rig::capabilities;

    fn body(config: &DeviceConfig) -> serde_json::Value {
        serde_json::to_value(config).expect("serializes")
    }

    #[test]
    fn a_dot_gets_its_wake_sound_and_nothing_for_a_screen() {
        let mut settings = DeviceSettings::default();
        settings.wakeword.wake_sound = true;
        let dot = capabilities(&["local_wake_chime", "ble_proxy", "wifi_control"]);
        let config = DeviceConfig::new(&dot, &settings, Some("kiosk-secret"));
        assert_eq!(
            body(&config),
            serde_json::json!({
                "startupVolume": 85, "duckDb": -18.0, "bleProxyEnabled": false,
                "wakeSound": true,
            })
        );
    }

    #[test]
    fn firmware_without_the_local_chime_gets_no_wake_sound() {
        let mut settings = DeviceSettings::default();
        settings.wakeword.wake_sound = true;
        let config = DeviceConfig::new(&capabilities(&[]), &settings, None);
        assert_eq!(
            body(&config),
            serde_json::json!({"startupVolume": 85, "duckDb": -18.0})
        );
    }

    #[test]
    fn a_lineage_show_gets_its_screen_kiosk_and_turn_view_fields() {
        let settings = DeviceSettings::default();
        let show = capabilities(&[
            "screen_wake",
            "screen_overlay",
            "turn_view_v1",
            "timer_view_v1",
            "kiosk_plugin",
            "local_wake_chime",
        ]);
        let config = DeviceConfig::new(&show, &settings, Some("kiosk-secret"));
        assert_eq!(
            body(&config),
            serde_json::json!({
                "startupVolume": 85, "duckDb": -18.0, "wakeSound": false,
                "screenWake": false, "screenWakeUrl": "http://127.0.0.1:2323",
                "screenWakePassword": "", "kioskToken": "kiosk-secret", "kioskPort": 2324,
                "turnViewLinger": 6.0,
            })
        );
        // The secrets never reach a log line.
        let shown = format!("{config:?}");
        assert!(!shown.contains("kiosk-secret"), "{shown}");
        // No stored token: the field is left out, never sent empty.
        let without = DeviceConfig::new(&show, &settings, None);
        assert!(body(&without).get("kioskToken").is_none());
    }
}
