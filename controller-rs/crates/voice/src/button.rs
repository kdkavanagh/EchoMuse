//! The action button (spec §4.3): hold events and tap events for Home Assistant, and
//! taps that start or cancel a turn. The gesture rules are the presentation crate's
//! [`ButtonPolicy`]; this module gives it the turn's situation and acts on its outputs.
//!
//! [`ButtonPolicy`]: echomuse_presentation::button::ButtonPolicy

use echomuse_presentation::{
    button::{ButtonAction, ButtonContext, ButtonDecision, ButtonOutput, TurnActivity},
    ring::Cue,
};
use echomuse_records::vocab::{ContinuationEnd, FollowUp, TerminalReason, TriggerType};
use serde_json::{Map, Value};

use crate::{
    io::{Effect, Now, SatelliteCommand},
    machine::Voice,
};

impl Voice {
    fn turn_activity(&self) -> TurnActivity {
        if let Some(turn) = &self.turn {
            let reply = matches!(turn.trigger, TriggerType::Reply | TriggerType::HaReply);
            return if reply && !turn.heard {
                TurnActivity::ReplyWindow
            } else {
                TurnActivity::Turn
            };
        }
        if self.window.is_some() {
            return TurnActivity::ReplyWindow;
        }
        TurnActivity::Idle
    }

    pub(crate) fn button_action(&mut self, body: &Map<String, Value>, now: Now) {
        let action = match ButtonAction::parse(body) {
            Ok(action) => action,
            Err(_) => {
                self.malformed("button.action");
                return;
            }
        };
        let context = ButtonContext {
            activity: self.turn_activity(),
            recording: self.recording_mode_runs(),
            speech_available: self.speech_ready,
        };
        match self.buttons.on_action(&action, context, now.at) {
            ButtonDecision::Ignored(_) => {}
            ButtonDecision::Act(outputs) => {
                for output in outputs {
                    self.button_output(output, Some(&action), now);
                }
            }
        }
    }

    pub(crate) fn button_output(
        &mut self,
        output: ButtonOutput,
        _action: Option<&ButtonAction>,
        now: Now,
    ) {
        match output {
            ButtonOutput::Event(event) => self
                .link
                .effects
                .push(Effect::Satellite(SatelliteCommand::ButtonEvent(event))),
            ButtonOutput::CancelTurn => {
                if self.turn.is_some() {
                    self.end_turn(TerminalReason::Superseded, now);
                } else {
                    self.close_window(ContinuationEnd::FollowUp(FollowUp::PromptCancelled), now);
                }
            }
            ButtonOutput::StartTurn(turn) => self.start_button_turn(turn, now),
            ButtonOutput::Refused(_) => self.cue(Cue::Error, now),
        }
    }
}
