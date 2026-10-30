//! The action button (spec §4.3): hold events and tap events for Home Assistant, and
//! taps that start or cancel a turn. The gesture rules are the presentation crate's
//! [`ButtonPolicy`]; this module gives it the conversation's situation and acts on its
//! outputs.
//!
//! [`ButtonPolicy`]: echomuse_presentation::button::ButtonPolicy

use echomuse_presentation::{
    button::{ButtonAction, ButtonContext, ButtonDecision, ButtonOutput},
    ring::Cue,
};
use serde_json::{Map, Value};

use super::Voice;
use crate::io::{Effect, Now, SatelliteCommand};

impl Voice {
    pub(super) fn button_action(&mut self, body: &Map<String, Value>, now: Now) {
        let action = match ButtonAction::parse(body) {
            Ok(action) => action,
            Err(_) => {
                self.malformed("button.action");
                return;
            }
        };
        let context = ButtonContext {
            activity: self.conversation.activity(),
            recording: self.recording_mode_runs(),
            speech_available: self.facts.speech_ready,
        };
        match self.buttons.on_action(&action, context, now.at) {
            ButtonDecision::Ignored(_) => {}
            ButtonDecision::Act(outputs) => {
                for output in outputs {
                    self.button_output(output, now);
                }
            }
        }
    }

    pub(super) fn button_output(&mut self, output: ButtonOutput, now: Now) {
        match output {
            ButtonOutput::Event(event) => self
                .link
                .effects
                .push(Effect::Satellite(SatelliteCommand::ButtonEvent(event))),
            ButtonOutput::CancelTurn => self.with_conversation(|conversation, cx, out| {
                conversation.button_cancel(cx, out, now);
            }),
            ButtonOutput::StartTurn(turn) => self.with_conversation(|conversation, cx, out| {
                conversation.start_button_turn(cx, out, turn, now);
            }),
            ButtonOutput::Refused(_) => self.cue(Cue::Error, now),
        }
    }
}
