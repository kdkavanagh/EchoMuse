//! The Home Assistant session (§3.3.3, §5.4): one authenticated WebSocket connection
//! shared by every device, plus REST calls, implementing
//! [`echomuse_core::services::HomeAssistant`].
//!
//! Start it with [`HaClient::start`] and supervise the returned task. Requests from all
//! devices run concurrently on the one connection; each is checked against its
//! [`WorkScope`] immediately before it is written, and an
//! intent run's events stop being delivered once its scope is cancelled or its connection
//! generation ends.
//!
//! Besides the device-runtime contract, the client offers the operations the alert
//! service builds on (§3.3.4): calendar events and subscriptions, Local Calendar
//! provisioning and HA's time zone (`calendar`), and script configuration, entity
//! exposure and custom events (`scripts`).
//!
//! The message shapes follow Home Assistant 2026.8.1's own code, cited in each module.

mod automations;
mod calendar;
mod client;
mod connection;
mod endpoint;
mod intent;
mod pipeline;
mod recognize;
mod satellite;
mod scripts;
mod vocabulary;

use echomuse_core::{
    services::{
        DirectIntentRequest, EffectOutcome, HaDeviceId, HaError, HomeAssistant, IntentRequest,
        IntentRun, MediaUrl, NotDispatched, PipelineId, RecognizeRequest, SentenceRecognition,
        SttRequest, TimerStatus, Transcript, TtsRequest,
    },
    types::WorkScope,
};

pub use automations::{AUTOMATIONS_EVENT, conversation_commands};
pub use calendar::{
    CalendarItem, CalendarPush, CalendarSubscription, CalendarTime, ConfigEntry, EventTarget,
    EventWrite,
};
pub use client::{HaClient, HaOptions, StartError, Subscription};
pub use connection::{ConnectionFault, HaEvent, HaStatus, SessionEnd};
pub use endpoint::{EndpointError, HaEndpoint};
pub use vocabulary::{AssistVocabulary, VOCABULARY_EVENTS};

impl HomeAssistant for HaClient {
    /// Reads the satellite's pipeline select and applies HA's rule; `None` gives HA's
    /// preferred pipeline.
    async fn pipeline_for(
        &self,
        scope: WorkScope,
        ha_device: Option<HaDeviceId>,
    ) -> Result<PipelineId, HaError> {
        self.satellite_pipeline(scope, ha_device).await
    }

    /// Sends the span with HA's segmenter off. A transport failure (the connection drops,
    /// a write fails, or no connection exists) is retried once on a new connection,
    /// waiting up to [`HaOptions::stt_reconnect_wait`] for it. HA's refusals, its `error`
    /// events and timeouts are not retried.
    async fn run_stt(&self, request: SttRequest) -> Result<Transcript, HaError> {
        self.transcribe(request).await
    }

    /// HA bounds the run with the timeout it is given and then sends an `error` event; the
    /// caller applies its own response limit on top.
    async fn run_intent_tts(&self, request: IntentRequest) -> Result<IntentRun, NotDispatched> {
        self.dispatch_intent_run(request).await
    }

    async fn run_tts(&self, request: TtsRequest) -> Result<MediaUrl, HaError> {
        self.synthesize(request).await
    }

    async fn recognize(
        &self,
        request: RecognizeRequest,
    ) -> Result<Vec<Option<SentenceRecognition>>, HaError> {
        self.recognize_sentences(request).await
    }

    async fn timer_status(
        &self,
        scope: WorkScope,
        ha_device: Option<HaDeviceId>,
    ) -> Result<TimerStatus, HaError> {
        self.timers(scope, ha_device).await
    }

    /// A refusal is HA's `error` intent response or a 4xx; an HTTP 500 or an unreadable
    /// answer is an unknown outcome. Cancelling the scope while the request is out
    /// withdraws interest and reports the outcome as unknown.
    async fn call_intent(&self, request: DirectIntentRequest) -> EffectOutcome<()> {
        self.run_direct_intent(request).await
    }
}
