//! The session actor's states (post-AFE architecture §7) and the phase each presents: the
//! ring's animation and the ESPHome `voice_state` text sensor (architecture §16.7).

use echomuse_esphome::entities::VoiceState;

/// A device's dialog state as its runtime holds it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ActorState {
    /// No accepted dialog.
    Idle,
    /// A turn is open and waits for command speech.
    Armed,
    /// Command speech is being heard.
    Listening,
    /// An endpoint is provisional and still observed.
    EndPending,
    /// The utterance is committed.
    Committed,
    /// The request is with Home Assistant or a local route.
    Thinking,
    /// The answer or a question is playing.
    Speaking,
    /// The turn is finishing.
    Closing,
}

/// What a state shows the user.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Phase {
    /// Nothing in progress.
    Idle,
    /// The microphone is open for a request or a reply.
    Listening,
    /// Waiting for the answer.
    Thinking,
    /// Playing the answer.
    Speaking,
}

impl ActorState {
    /// The phase it presents.
    pub fn phase(self) -> Phase {
        match self {
            Self::Idle | Self::Closing => Phase::Idle,
            Self::Armed | Self::Listening | Self::EndPending => Phase::Listening,
            Self::Committed | Self::Thinking => Phase::Thinking,
            Self::Speaking => Phase::Speaking,
        }
    }
}

impl Phase {
    /// The `voice_state` text sensor's value.
    pub fn voice_state(self) -> VoiceState {
        match self {
            Self::Idle => VoiceState::Idle,
            Self::Listening => VoiceState::Listening,
            Self::Thinking => VoiceState::Thinking,
            Self::Speaking => VoiceState::Speaking,
        }
    }
}

/// The `voice_state` value for a runtime state; `None` (the sensor's missing state) while
/// the state is unknown, such as before the device's session is attached.
pub fn voice_state(state: Option<ActorState>) -> Option<VoiceState> {
    state.map(|s| s.phase().voice_state())
}

/// Follows the runtime's state and reports each change of the sensor's value once.
#[derive(Clone, Copy, Debug, Default)]
pub struct VoiceStateProjection {
    shown: Option<Option<VoiceState>>,
}

impl VoiceStateProjection {
    /// The new sensor value when `state` changes it, else `None`.
    pub fn update(&mut self, state: Option<ActorState>) -> Option<Option<VoiceState>> {
        let value = voice_state(state);
        if self.shown == Some(value) {
            return None;
        }
        self.shown = Some(value);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_maps_to_its_phase() {
        let expected = [
            (ActorState::Idle, VoiceState::Idle),
            (ActorState::Armed, VoiceState::Listening),
            (ActorState::Listening, VoiceState::Listening),
            (ActorState::EndPending, VoiceState::Listening),
            (ActorState::Committed, VoiceState::Thinking),
            (ActorState::Thinking, VoiceState::Thinking),
            (ActorState::Speaking, VoiceState::Speaking),
            (ActorState::Closing, VoiceState::Idle),
        ];
        for (state, value) in expected {
            assert_eq!(voice_state(Some(state)), Some(value), "{state:?}");
        }
        assert_eq!(voice_state(None), None);
    }

    #[test]
    fn a_turn_reports_each_change_once() {
        let mut projection = VoiceStateProjection::default();
        let turn = [
            None,
            Some(ActorState::Idle),
            Some(ActorState::Armed),
            Some(ActorState::Listening),
            Some(ActorState::EndPending),
            Some(ActorState::Listening),
            Some(ActorState::EndPending),
            Some(ActorState::Committed),
            Some(ActorState::Thinking),
            Some(ActorState::Speaking),
            Some(ActorState::Armed),
            Some(ActorState::Listening),
            Some(ActorState::Committed),
            Some(ActorState::Speaking),
            Some(ActorState::Closing),
            Some(ActorState::Idle),
            None,
        ];
        let reported: Vec<Option<&str>> = turn
            .into_iter()
            .filter_map(|state| projection.update(state))
            .map(|value| value.map(VoiceState::as_str))
            .collect();
        assert_eq!(
            reported,
            [
                None,
                Some("idle"),
                Some("listening"),
                Some("thinking"),
                Some("speaking"),
                Some("listening"),
                Some("thinking"),
                Some("speaking"),
                Some("idle"),
                None,
            ]
        );
    }
}
