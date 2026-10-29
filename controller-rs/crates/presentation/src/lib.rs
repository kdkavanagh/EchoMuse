//! What a device shows and plays outside speech (spec §4.3, §4.8, §4.9): the LED ring's
//! projection, the action button's gestures, the Echo Show's turn view, the `voice_state`
//! sensor, and the alert sound catalog.
//!
//! Everything but the sound service is a pure state machine the device runtime owns and
//! drives: it passes in what happened and the time on the controller's monotonic clock,
//! sends what comes back, and calls `on_timer` at `next_deadline`. Nothing here does I/O
//! or reads a clock, except the sound catalog, which runs on its own thread.
//!
//! What the device runtime calls:
//!
//! - **Ring** ([`ring::LedProjector`]): build one per session from the hello's
//!   capabilities and the device's `ring` settings. Call `session_started` at
//!   `session.ready`, `set_phase` with [`phase::ActorState::phase`] on every actor state
//!   change, `set_recording` when a recording mode starts or ends, `set_timers` with the
//!   alerts service's timer view whenever it changes (every second while a timer runs),
//!   `cue` with [`ring::Cue::for_turn_end`] (or [`ring::Cue::Error`] for a refused wake or
//!   press) after a turn ends, and `set_settings` on a settings change. Send each returned
//!   [`ring::wire::LedCommand`] as `leds` or `led_anim` with generation 0.
//! - **Button** ([`button::ButtonPolicy`]): one per session. Decode each `button.action`
//!   with [`button::ButtonAction::parse`] and pass it to `on_action` with the dialog's
//!   [`button::ButtonContext`]; pass each `privacy.changed` sequence number to
//!   `accept_physical_seq`. Act on each [`button::ButtonOutput`]: fire the ESPHome
//!   `action_button` event, cancel the turn, open a button turn, or play the error cue.
//! - **Turn view** ([`turn_view::TurnView`]): one per session; call `listening` at the
//!   turn's `dialog_input` start (not for a wake that lost arbitration), `thinking` at
//!   dispatch, `answer_so_far` on each streamed answer delta, `final_answer` at
//!   `intent-end` or for EchoMuse's own answer, and `done` when the turn ends. Send each
//!   body as `turn.view` with generation 0. `timer.view` is sent by the alerts service.
//! - **Voice state** ([`phase::VoiceStateProjection`]): feed it the actor state and pass
//!   each change to the ESPHome satellite's `set_voice_state`.
//! - **Sounds** ([`sounds::service::SoundService`]): start it once at startup with
//!   [`sounds::SoundsDir::beside_database`] and the image's `ffmpeg`. The dashboard's
//!   upload and delete go through its handle; a speaker's
//!   [`sounds::catalog::CatalogSnapshot::endpoint_sounds`] gives the alerts service the
//!   `alarm_sound` and `timer_sound` of its endpoint configuration; the asset socket serves
//!   [`sounds::SoundsDir::asset_path`]; a preview plays the summary's hash as an
//!   `alert_preview`.

#![warn(missing_docs)]

pub mod button;
pub mod phase;
pub mod ring;
pub mod sounds;
pub mod turn_view;
