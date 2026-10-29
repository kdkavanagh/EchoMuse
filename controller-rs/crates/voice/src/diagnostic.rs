//! The dashboard's recording modes on the voice path (spec §4.11; playback-capture.md):
//! one `diagnostic` uplink subscription feeds every armed mode, mic live plus AFE
//! records live in an `afe_metadata` session.
//!
//! While a mode is armed, the machine keeps that subscription open whenever it can: it
//! opens it when armed, and opens it again after it ended for any reason but the
//! controller's own close, as soon as capture allows (after an unmute, on a new capture
//! epoch). A subscription the device refused is not asked for again until the capture
//! epoch changes, so a refusal cannot turn into a stream of requests. Every chunk it
//! delivers, and its end, go to the runtime as [`Effect::Diagnostic`]; the runtime's
//! recorder owns the files. While the subscription is live the uplink refuses wake
//! candidates (§4.1) and the ring shows the recording pulse.

use std::num::NonZeroU64;

use echomuse_uplink::{CloseReason, Delivery, EndCause, Owner, SubscriptionId};

use crate::{
    io::{Effect, Now, Outbound},
    machine::Voice,
};

/// The owner the diagnostic subscription is opened under.
const OWNER: &str = "diagnostic";

/// What the recording modes' subscription delivers to the runtime.
#[derive(Debug)]
pub enum DiagnosticAudio {
    /// Mic samples or AFE records, in order per stream.
    Chunk(echomuse_uplink::Chunk),
    /// The subscription ended; nothing more follows from it. Whatever the recorder has
    /// not saved must be discarded when the cause is [`EndCause::Muted`].
    Ended(EndCause),
}

/// The machine's recording-mode state.
#[derive(Debug, Default)]
pub(crate) struct Diagnostic {
    /// A mode is armed.
    armed: bool,
    /// The open subscription.
    subscription: Option<SubscriptionId>,
    /// The device refused a subscription under this capture epoch.
    refused_epoch: Option<NonZeroU64>,
}

impl Voice {
    /// Arms or disarms the recording modes' subscription ([`crate::Input::Diagnostic`]).
    pub(crate) fn set_diagnostic(&mut self, armed: bool, now: Now) {
        self.diagnostic.armed = armed;
        if armed {
            self.diagnostic.refused_epoch = None;
            self.reconcile_diagnostic(now);
        } else if let Some(id) = self.diagnostic.subscription.clone() {
            // Its end arrives as a delivery, which tells the recorder to finish.
            let _ = self
                .uplink
                .close(&id, CloseReason::Closed, self.mono(now.at));
        }
    }

    /// Whether the recording modes' subscription is open.
    pub fn diagnostic_active(&self) -> bool {
        self.diagnostic.subscription.is_some()
    }

    /// Whether a recording mode runs: one is armed, or its subscription is still open
    /// (a disarmed mode's subscription stays until its end arrives). Turns that would
    /// take the microphone from it are refused meanwhile (§4.11).
    pub(crate) fn recording_mode_runs(&self) -> bool {
        self.diagnostic.armed || self.diagnostic_active()
    }

    /// Opens the subscription when a mode is armed and none is open, and capture allows.
    pub(crate) fn reconcile_diagnostic(&mut self, now: Now) {
        let state = &self.diagnostic;
        if !state.armed || state.subscription.is_some() || self.muted || self.capture.is_none() {
            return;
        }
        let epoch = self.uplink.timeline().capture_epoch();
        if epoch.is_none()
            || state
                .refused_epoch
                .is_some_and(|refused| Some(refused) == epoch)
        {
            return;
        }
        let owner = Owner::new(OWNER).expect("the owner is not empty");
        match self.uplink.open_diagnostic(owner, self.mono(now.at)) {
            Ok(opened) => {
                self.apply_grants(&opened.grants);
                self.diagnostic.subscription = Some(opened.subscription_id);
                self.diagnostic_ring(now);
            }
            Err(error) => {
                tracing::debug!(device = %self.device, ?error, "the recording subscription cannot open yet");
            }
        }
    }

    /// Whether `subscription` is the recording modes'.
    pub(crate) fn is_diagnostic(&self, subscription: &SubscriptionId) -> bool {
        self.diagnostic.subscription.as_ref() == Some(subscription)
    }

    /// Passes one of the subscription's deliveries to the runtime.
    pub(crate) fn diagnostic_delivery(&mut self, delivery: Delivery, now: Now) {
        match delivery {
            Delivery::Chunk(chunk) => self
                .link
                .effects
                .push(Effect::Diagnostic(DiagnosticAudio::Chunk(chunk))),
            Delivery::Ended(ended) => {
                self.diagnostic.subscription = None;
                if matches!(ended.cause, EndCause::Rejected { .. }) {
                    self.diagnostic.refused_epoch = self.uplink.timeline().capture_epoch();
                }
                self.link
                    .effects
                    .push(Effect::Diagnostic(DiagnosticAudio::Ended(ended.cause)));
                self.diagnostic_ring(now);
            }
        }
    }

    fn diagnostic_ring(&mut self, now: Now) {
        let on = self.diagnostic.subscription.is_some();
        if let Some(command) = self.ring.set_recording(on, now.at) {
            self.link.send(Outbound::Led(command), 0);
        }
    }
}
