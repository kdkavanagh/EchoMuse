//! What a device runtime hands to the device services as it reads its session: log
//! lines, Wi-Fi reports, the session recorder's ingress tap, and the recording modes'
//! and capture's audio through one [`DiagnosticTee`]. [`DeviceTaps`] is shared by every
//! runtime; [`SessionTaps`] belongs to one link session and lives in its runtime, which
//! calls it in ingress order and never waits on it.

use std::sync::Arc;

use echomuse_core::types::{DeviceId, RuntimeId, SessionId};
use echomuse_link::{Ingress, IngressOrdinal, SessionLink};
use echomuse_protocol::MessageType;
use echomuse_recorder::{DisconnectReason, PrivacyState};
use echomuse_voice::DiagnosticAudio;
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::{
    capture::{CaptureHub, DiagnosticTee},
    device_logs::LogSink,
    recorder_link::{RecorderHub, RecorderTap, identity},
    recording_modes::RecordingHub,
    wifi::WifiService,
};

/// The device services every runtime reports to. Cheap to clone.
#[derive(Clone)]
pub struct DeviceTaps {
    pub logs: LogSink,
    pub wifi: Arc<WifiService>,
    pub recording: Arc<RecordingHub>,
    pub recorder: Arc<RecorderHub>,
    pub capture: Arc<CaptureHub>,
}

/// What the runtime knows about a session when it attaches.
pub struct SessionStart<'a> {
    pub device: &'a DeviceId,
    pub runtime: RuntimeId,
    pub session: SessionId,
    pub link: &'a SessionLink,
    pub firmware: Option<String>,
    pub afe_metadata: bool,
    pub muted: bool,
    pub capture_permitted: bool,
}

impl DeviceTaps {
    /// The taps of a session that just attached.
    pub fn session(&self, start: SessionStart<'_>) -> SessionTaps {
        self.capture.attached(start.device, start.session);
        let recorder = self.recorder.tap(
            identity(
                start.device.clone(),
                start.runtime,
                start.session,
                start.link.session_id().clone(),
            ),
            start.firmware,
            PrivacyState {
                muted: start.muted,
                capture_permitted: start.capture_permitted,
            },
        );
        SessionTaps {
            taps: self.clone(),
            device: start.device.clone(),
            session: start.session,
            link: start.link.clone(),
            afe_metadata: start.afe_metadata,
            capture_permitted: start.capture_permitted,
            recorder,
            tee: None,
        }
    }
}

/// One link session's taps.
pub struct SessionTaps {
    taps: DeviceTaps,
    device: DeviceId,
    session: SessionId,
    link: SessionLink,
    afe_metadata: bool,
    capture_permitted: bool,
    recorder: RecorderTap,
    /// The fan-out the voice path's recording sink feeds, while any mode is armed.
    tee: Option<DiagnosticTee>,
}

/// What the runtime should do with the voice path's recording sink.
pub enum SinkChange {
    /// Nothing changes.
    Keep,
    /// Arm with this sink.
    Arm(mpsc::Sender<DiagnosticAudio>),
    /// Disarm.
    Disarm,
}

impl SessionTaps {
    /// One ordered ingress item the runtime accepted.
    pub fn ingress(&mut self, ordinal: IngressOrdinal, ingress: &Ingress) {
        self.recorder.ingress(ordinal, ingress);
    }

    /// A control message no other part of the runtime handles; true when a tap took it.
    pub fn control(&mut self, message_type: MessageType, body: &Map<String, Value>) -> bool {
        match message_type {
            MessageType::WifiResult | MessageType::WifiScanResult => {
                self.taps
                    .wifi
                    .device_message(&self.device, &self.link, message_type, body);
                true
            }
            _ => false,
        }
    }

    /// A `log` body.
    pub fn log(&self, body: &Map<String, Value>) {
        self.taps.logs.offer(&self.device, body);
    }

    /// The device's privacy mute changed.
    pub fn privacy(&mut self, muted: bool) {
        self.recorder.privacy(muted, self.capture_permitted);
    }

    /// Whether the session recorder is writing this session (the voice path then queues
    /// its decision evidence).
    pub fn session_recording(&self) -> bool {
        self.recorder.recording()
    }

    /// The voice path's decision evidence for the session recorder.
    pub fn evidence(
        &mut self,
        origin: tokio::time::Instant,
        items: Vec<(
            echomuse_turn::MonoInstant,
            echomuse_voice::evidence::Evidence,
        )>,
    ) {
        self.recorder.evidence(origin, items);
    }

    /// Whether the voice path's recording sink must change to match the armed modes
    /// (sample collection or ambient recording, and capture). One sink serves them all
    /// while any is armed; a mode arming or disarming beside another changes only what
    /// the sink feeds.
    pub fn recording(&mut self) -> SinkChange {
        let modes = self.taps.recording.modes(&self.device);
        let recording = modes.collect || modes.ambient;
        let capture = self.taps.capture.tap(&self.device, self.session);
        let live = self.tee.take().filter(DiagnosticTee::is_live);
        let keep_recorder =
            recording && live.as_ref().and_then(DiagnosticTee::recorder_modes) == Some(modes);
        let recorder = (recording && !keep_recorder)
            .then(|| {
                self.taps
                    .recording
                    .sink(&self.device, self.session, self.afe_metadata)
            })
            .flatten();
        if !keep_recorder && recorder.is_none() && capture.is_none() {
            // Nothing records: the subscription closes, and its end reaches whatever the
            // tee still feeds.
            return match live {
                Some(_) => SinkChange::Disarm,
                None => SinkChange::Keep,
            };
        }
        let (tee, change) = match live {
            Some(tee) => (tee, SinkChange::Keep),
            None => {
                let (tee, sink) = self.taps.capture.tee();
                (tee, SinkChange::Arm(sink))
            }
        };
        if !keep_recorder {
            tee.set_recorder(recorder.map(|sink| (modes, sink)));
        }
        tee.set_capture(capture);
        self.tee = Some(tee);
        change
    }

    /// The session ended.
    pub fn ended(self, reason: DisconnectReason) {
        self.taps.capture.detached(&self.device, self.session);
        self.recorder.ended(reason);
    }
}
