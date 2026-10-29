//! Why a connected device lacks a capability a dashboard control needs (§4.10), when the
//! reason depends on its platform: the dashboard's `capability_gaps`.
//!
//! The dashboard shows a control the device cannot support disabled, with a reason. Its
//! own reasons say the firmware needs updating; the controller's come first because
//! they know the platform (dashboard.jsx, `capabilityGap`): an Echo Show's Wi-Fi is
//! "managed by the device", a Fire OS Show has "no LED ring on this device", and a
//! device whose firmware predates something its platform's current firmware announces
//! has a "firmware update available".
//!
//! The capabilities considered are the ones the settings' controls require
//! ([`SettingKey::requirements`]) plus the dashboard's other gated controls: Wi-Fi,
//! privacy mute and near misses. What each platform's current firmware announces comes
//! from the firmware (`device/internal/supervisor/session.go`, `platformCapabilities`,
//! and the capabilities every image announces). A capability the firmware announces
//! only while it works at run time (the AFE metadata decoder) gets no reason here; the
//! dashboard's own applies.

use std::collections::BTreeMap;

use echomuse_protocol::{Capabilities, Capability, Platform};
use echomuse_settings::{Requirement, SettingKey};
use serde::{Serialize, Serializer};

/// A reason a control is unavailable, as the dashboard shows it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GapReason {
    /// The platform's current firmware announces it; this device runs an older one.
    FirmwareUpdate,
    /// Android owns it on an Echo Show (Wi-Fi, Bluetooth).
    ManagedByDevice,
    /// A Fire OS Show has no LED ring the firmware drives.
    NoLedRing,
    /// A Fire OS Show has no action button the firmware reads.
    NoActionButton,
    /// An Echo Dot has no screen.
    NoScreen,
    /// Only the LineageOS image runs the companion app and the Kiosk Satellite plugin.
    NeedsLineage,
    /// The Echo Show's privacy mute is not integrated yet.
    NoPrivacyMute,
    /// The device reported that it has no volume control this run (`volume_gap`).
    NoVolumeControl,
}

impl GapReason {
    /// The sentence the dashboard shows after "unavailable —" or in place of a control.
    pub const fn text(self) -> &'static str {
        match self {
            Self::FirmwareUpdate => "firmware update available",
            Self::ManagedByDevice => "managed by the device",
            Self::NoLedRing => "no LED ring on this device",
            Self::NoActionButton => "no action button on this device",
            Self::NoScreen => "no screen on this device",
            Self::NeedsLineage => "needs LineageOS on this Echo Show",
            Self::NoPrivacyMute => "no privacy mute on the Echo Show yet",
            Self::NoVolumeControl => "the device reports no volume control",
        }
    }
}

impl Serialize for GapReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.text())
    }
}

/// Gated controls that are not settings (dashboard.jsx: the Wi-Fi panel, the Mute row,
/// near-miss listings and the AFE panels).
const OTHER_GATED: [Capability; 4] = [
    Capability::WifiControl,
    Capability::PrivacyMute,
    Capability::NearMiss,
    Capability::AfeMetadata,
];

/// What every image's current firmware announces unconditionally, beyond the required
/// eight (`Hello` in device/internal/supervisor/session.go).
const EVERY_PLATFORM: [Capability; 6] = [
    Capability::AlertPrefetch,
    Capability::LocalWakeChime,
    Capability::NoInputChime,
    Capability::AnnouncementChime,
    Capability::OpenRules,
    Capability::NearMiss,
];

/// What each platform's current firmware announces for that platform
/// (`platformCapabilities`).
fn platform_capabilities(platform: Platform) -> &'static [Capability] {
    use Capability as C;
    match platform {
        Platform::FireOs6 => &[
            C::Leds,
            C::LedAnim,
            C::Buttons,
            C::ButtonHold,
            C::WifiControl,
            C::BleProxy,
            C::PrivacyMute,
        ],
        Platform::FireOs6Crown => &[C::ScreenWake],
        Platform::Lineage18Crown => &[
            C::Leds,
            C::LedAnim,
            C::Buttons,
            C::ButtonHold,
            C::ScreenWake,
            C::ScreenOverlay,
            C::TurnView,
            C::TimerView,
            C::KioskPlugin,
        ],
    }
}

/// Why this platform can never have `capability`, when it cannot.
fn platform_reason(platform: Platform, capability: Capability) -> Option<GapReason> {
    use Capability as C;
    match (platform, capability) {
        (
            Platform::FireOs6,
            C::ScreenWake | C::ScreenOverlay | C::TurnView | C::TimerView | C::KioskPlugin,
        ) => Some(GapReason::NoScreen),
        (Platform::FireOs6Crown | Platform::Lineage18Crown, C::WifiControl | C::BleProxy) => {
            Some(GapReason::ManagedByDevice)
        }
        (Platform::FireOs6Crown | Platform::Lineage18Crown, C::PrivacyMute) => {
            Some(GapReason::NoPrivacyMute)
        }
        (Platform::FireOs6Crown, C::Leds | C::LedAnim) => Some(GapReason::NoLedRing),
        (Platform::FireOs6Crown, C::Buttons | C::ButtonHold) => Some(GapReason::NoActionButton),
        (
            Platform::FireOs6Crown,
            C::ScreenOverlay | C::TurnView | C::TimerView | C::KioskPlugin,
        ) => Some(GapReason::NeedsLineage),
        _ => None,
    }
}

/// Every capability a dashboard control is gated on.
fn gated() -> impl Iterator<Item = Capability> {
    SettingKey::ALL
        .iter()
        .flat_map(|key| key.requirements())
        .filter_map(|requirement| match requirement {
            Requirement::Has(capability) => Some(*capability),
            Requirement::Lacks(_) => None,
        })
        .chain(OTHER_GATED)
}

/// The controller's reason for each gated capability a connected device on `platform`
/// did not announce, when the reason depends on the platform.
pub fn capability_gaps(
    platform: Platform,
    capabilities: &Capabilities,
) -> BTreeMap<Capability, GapReason> {
    gated()
        .filter(|capability| !capabilities.contains(*capability))
        .filter_map(|capability| {
            let reason = platform_reason(platform, capability).or_else(|| {
                (platform_capabilities(platform).contains(&capability)
                    || EVERY_PLATFORM.contains(&capability))
                .then_some(GapReason::FirmwareUpdate)
            })?;
            Some((capability, reason))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use echomuse_protocol::REQUIRED_CAPABILITIES;

    use super::*;

    fn announced(platform: Platform, extra: &[Capability]) -> Capabilities {
        let names: Vec<&str> = REQUIRED_CAPABILITIES
            .iter()
            .chain(platform_capabilities(platform))
            .chain(&EVERY_PLATFORM)
            .chain(extra)
            .map(|c| c.as_str())
            .collect();
        Capabilities::from_names(names)
    }

    fn reasons(
        platform: Platform,
        capabilities: &Capabilities,
    ) -> Vec<(&'static str, &'static str)> {
        capability_gaps(platform, capabilities)
            .into_iter()
            .map(|(c, r)| (c.as_str(), r.text()))
            .collect()
    }

    #[test]
    fn a_current_dot_lacks_only_what_has_a_platform_reason() {
        let dot = announced(Platform::FireOs6, &[Capability::AfeMetadata]);
        assert_eq!(
            reasons(Platform::FireOs6, &dot),
            [
                ("screen_wake", "no screen on this device"),
                ("turn_view_v1", "no screen on this device"),
                ("kiosk_plugin", "no screen on this device"),
            ]
        );
    }

    #[test]
    fn a_fire_os_show_names_what_its_image_cannot_do() {
        let show = announced(Platform::FireOs6Crown, &[]);
        assert_eq!(
            reasons(Platform::FireOs6Crown, &show),
            [
                ("leds", "no LED ring on this device"),
                ("led_anim", "no LED ring on this device"),
                ("button_hold", "no action button on this device"),
                ("wifi_control", "managed by the device"),
                ("ble_proxy", "managed by the device"),
                ("privacy_mute", "no privacy mute on the Echo Show yet"),
                ("turn_view_v1", "needs LineageOS on this Echo Show"),
                ("kiosk_plugin", "needs LineageOS on this Echo Show"),
            ]
        );
    }

    #[test]
    fn older_firmware_is_told_an_update_is_available() {
        // A LineageOS Show whose firmware predates the plugin and the open rules.
        let names: Vec<&str> = REQUIRED_CAPABILITIES
            .iter()
            .chain(&[
                Capability::Leds,
                Capability::LedAnim,
                Capability::ButtonHold,
                Capability::ScreenWake,
                Capability::ScreenOverlay,
                Capability::TurnView,
                Capability::NearMiss,
            ])
            .map(|c| c.as_str())
            .collect();
        let old = Capabilities::from_names(names);
        assert_eq!(
            reasons(Platform::Lineage18Crown, &old),
            [
                ("open_rules_v1", "firmware update available"),
                ("wifi_control", "managed by the device"),
                ("ble_proxy", "managed by the device"),
                ("privacy_mute", "no privacy mute on the Echo Show yet"),
                ("kiosk_plugin", "firmware update available"),
            ]
        );
    }

    #[test]
    fn a_run_time_capability_has_no_controller_reason() {
        let dot = announced(Platform::FireOs6, &[]);
        assert!(!capability_gaps(Platform::FireOs6, &dot).contains_key(&Capability::AfeMetadata));
    }
}
