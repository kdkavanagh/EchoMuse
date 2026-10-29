//! Requests the controller handles itself (spec §4.4): commands that act on what is
//! going on before STT (an alert ringing, the device's reply, media), and after STT the
//! repeat, alarms, alarm questions and cancelling this speaker's timers, each with the
//! questions it may need to ask. Everything else goes to Home Assistant, timer requests
//! included: HA's own timer intents (with the custom sentences in its configuration) set
//! timers, answer "how much time is left", and cancel a timer named by name or length.
//!
//! Changes go through the services that own them and follow their outcome rules: an
//! alarm operation is journaled under the asking turn's id (kept across an AM/PM
//! question), a timer cancel is sent to HA once and never re-sent, and an outcome that may
//! or may not have happened is said as "I'm not sure that worked." A failure the speaker
//! can say nothing more useful about ends the turn `ha_error` with "Something went wrong."

use echomuse_alerts::{
    Act, ActOutcome, AlarmEntry, CancelOutcome, OpId, Outcome, Refusal, RingRef,
    wire::{ActAction, ActSource},
};
use echomuse_core::services::{ConversationId, EffectOutcome, HaError, HaTimer, TimerStatus};
use echomuse_grammar::{
    AlarmCancel, AlarmCancelPlan, AlarmChoice, AlarmQuery, AlarmRequest, Choice, ChosenTimers,
    ContextCommand, Line, PendingAlarm, ReplyKind, Route, SaidTime, SpeakerTimers, SpokenAlarm,
    SpokenTimer, TimerCancel, TimerCancelPlan, TimerPick,
};
use echomuse_records::vocab::{TerminalReason, TurnOutcome};
use uuid::Uuid;

use crate::{
    io::{AlertReply, AlertRequest, Effect, HaRequest, Now},
    machine::Voice,
    turn::{Call, Stage},
};

/// What a reply window expects: the question it answers.
#[derive(Clone, Debug)]
pub(crate) enum Expectation {
    /// HA's `continue_conversation`: the reply goes to HA with its conversation id.
    Conversation {
        conversation: Option<ConversationId>,
    },
    /// "Sorry, I didn't catch that.": the reply is a fresh request.
    Retry,
    /// "AM or PM?" for a new alarm; `origin` keeps the operation id (§3.3.4).
    Meridiem {
        pending: PendingAlarm,
        origin: Uuid,
        asked_again: bool,
    },
    /// "AM or PM?" for cancelling alarms at an hour both readings match.
    CancelMeridiem {
        hour: u8,
        minute: u8,
        origin: Uuid,
        asked_again: bool,
    },
    /// "Which one?" among this speaker's timers.
    WhichTimer {
        timers: Vec<HaTimer>,
        asked_again: bool,
    },
    /// "Which alarm?" among this speaker's alarms.
    WhichAlarm {
        alarms: Vec<AlarmChoice>,
        origin: Uuid,
        asked_again: bool,
    },
    /// A reply to a conversation HA started through the satellite.
    HaConversation,
}

impl Expectation {
    pub(crate) fn reply_kind(&self) -> Option<ReplyKind> {
        match self {
            Self::Meridiem { .. } | Self::CancelMeridiem { .. } => Some(ReplyKind::Meridiem),
            Self::WhichTimer { .. } | Self::WhichAlarm { .. } => Some(ReplyKind::Which),
            Self::Conversation { .. } | Self::Retry | Self::HaConversation => None,
        }
    }

    pub(crate) fn conversation(&self) -> Option<ConversationId> {
        match self {
            Self::Conversation { conversation } => conversation.clone(),
            _ => None,
        }
    }
}

/// An alert operation a turn waits on, and what to say about its outcome.
#[derive(Clone, Debug)]
pub(crate) enum AlertWait {
    /// Stopping or snoozing a ring: nothing is said unless the outcome is unknown.
    Act,
    Set {
        request: AlarmRequest,
    },
    ListForCancel {
        cancel: AlarmCancel,
        origin: Uuid,
    },
    Cancel,
    Query(AlarmQuery),
}

/// A timer request waiting on HA's timer list (`HassTimerStatus`).
#[derive(Clone, Debug)]
pub(crate) enum TimerRequest {
    /// "Cancel the timer" or "cancel all timers", and the text as routed: when HA cannot
    /// list its timers, the text goes to HA's own intent handling instead.
    Cancel { cancel: TimerCancel, text: String },
    /// The answer to "Which one?", carried out against the timers as they are now.
    Chosen(ChosenTimers),
}

impl Voice {
    fn today(&self, now: Now) -> jiff::civil::Date {
        self.local_now(now)
            .map(|zoned| zoned.date())
            .unwrap_or_else(|| {
                jiff::Timestamp::from_millisecond(now.wall_ms)
                    .map(|t| t.to_zoned(jiff::tz::TimeZone::UTC).date())
                    .unwrap_or_default()
            })
    }

    /// The local time in Home Assistant's zone; `None` while the zone is unknown.
    fn local_now(&self, now: Now) -> Option<jiff::Zoned> {
        self.time_zone.as_ref()?.at_ms(now.wall_ms)
    }

    pub(crate) fn line_text(&self, line: &Line, now: Now) -> String {
        line.text(self.today(now))
    }

    fn alert_call(&mut self, request: AlertRequest, wait: AlertWait) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.alert_wait = Some(wait);
        let uuid = turn.uuid;
        let call = self.call_id();
        self.calls.insert(call, Call::Alert { turn: uuid });
        self.link.effects.push(Effect::Alert { call, request });
    }

    fn set_outcome(&mut self, outcome: TurnOutcome) {
        if let Some(turn) = self.turn.as_mut() {
            turn.facts.outcome = Some(outcome);
        }
    }

    /// A failure with nothing more useful to say: the turn ends `ha_error` (error cue) with
    /// "Something went wrong."
    fn went_wrong(&mut self, now: Now) {
        if let Some(turn) = self.turn.as_mut() {
            turn.facts.terminal = Some(TerminalReason::HaError);
        }
        self.say(Line::SomethingWentWrong, now);
    }

    /// An operation that may or may not have happened: the turn ends `outcome_unknown`
    /// with "I'm not sure that worked."
    fn not_sure(&mut self, now: Now) {
        if let Some(turn) = self.turn.as_mut() {
            turn.facts.terminal = Some(TerminalReason::OutcomeUnknown);
        }
        self.say(Line::NotSureItWorked, now);
    }

    /// A command acting on what is going on, before STT (§4.4).
    pub(crate) fn context_command(&mut self, command: ContextCommand, text: String, now: Now) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.stage = Stage::Thinking;
        turn.facts.stt_text = Some(text);
        turn.facts.outcome = Some(TurnOutcome::LocalCommand);
        let uuid = turn.uuid;
        let ring = turn.ring.clone();
        let alert = turn.context.alert.clone();
        match command {
            ContextCommand::DismissAlert | ContextCommand::SnoozeAlert => {
                let (Some((device, ring_id)), Some(_)) = (ring, alert) else {
                    self.finish_turn(now);
                    return;
                };
                let action = if command == ContextCommand::SnoozeAlert {
                    ActAction::Snooze
                } else {
                    ActAction::Dismiss
                };
                let act = Act {
                    op_id: OpId::local_action(&uuid.to_string(), action.as_str()),
                    target: RingRef { device, ring_id },
                    action,
                    source: ActSource::Voice,
                };
                self.alert_call(AlertRequest::Act(act), AlertWait::Act);
            }
            ContextCommand::StopReply => {
                // The reply stopped when this turn began; nothing more to do or say.
                self.finish_turn(now);
            }
            ContextCommand::StopMedia | ContextCommand::PauseMedia => {
                let request = if command == ContextCommand::StopMedia {
                    echomuse_esphome::voice::MediaRequest::Stop
                } else {
                    echomuse_esphome::voice::MediaRequest::Pause
                };
                // During the dialog the player records the intent and applies it when
                // the turn ends (§4.9).
                let _ = self.media.command(request);
                let effects = self.media_effects();
                self.apply_media_effects(effects, now);
                self.finish_turn(now);
            }
        }
    }

    /// Routes a final transcript (wake phrase removed).
    pub(crate) fn route(&mut self, text: String, now: Now) {
        let expectation = self.turn.as_ref().and_then(|t| t.expectation.clone());
        if let Some(expectation) = expectation
            && self.answer_question(&expectation, &text, now)
        {
            return;
        }
        match self.grammar.route(&text) {
            Route::Repeat => self.repeat(now),
            Route::SetAlarm(request) => self.set_alarm(request, None, now),
            Route::AskMeridiem(pending) => {
                self.set_outcome(TurnOutcome::Clarification);
                let origin = self.turn_uuid();
                self.ask(
                    Line::AmOrPm,
                    crate::local::Expectation::Meridiem {
                        pending,
                        origin,
                        asked_again: false,
                    },
                    now,
                );
            }
            Route::CancelAlarms(cancel) => {
                self.set_outcome(TurnOutcome::Alarm);
                let origin = self.turn_uuid();
                self.alert_call(
                    AlertRequest::ListAlarms,
                    AlertWait::ListForCancel { cancel, origin },
                );
            }
            Route::AlarmQuery(query) => {
                self.set_outcome(TurnOutcome::AlarmQuery);
                self.alert_call(AlertRequest::ListAlarms, AlertWait::Query(query));
            }
            Route::CancelTimers(cancel) => {
                self.set_outcome(TurnOutcome::Timer);
                if self.ha_device.is_none() {
                    // Without this satellite's HA device, HA's timers cannot be told apart
                    // by speaker: refusing is better than cancelling another speaker's.
                    self.say(Line::TimerNotCancelled, now);
                    return;
                }
                self.request_timers(TimerRequest::Cancel { cancel, text }, now);
            }
            Route::SendToHa => self.dispatch_to_ha(text, now),
        }
    }

    fn turn_uuid(&self) -> Uuid {
        self.turn.as_ref().map(|t| t.uuid).unwrap_or_default()
    }

    /// Answers the question the turn replies to; `false` when the text is not an answer
    /// to it (it is then routed as a request of its own).
    fn answer_question(&mut self, expectation: &Expectation, text: &str, now: Now) -> bool {
        match expectation {
            Expectation::Meridiem {
                pending,
                origin,
                asked_again,
            } => {
                match self.grammar.meridiem(text) {
                    Some(meridiem) => {
                        self.set_alarm(pending.with_meridiem(meridiem), Some(*origin), now)
                    }
                    None => self.ask_again(
                        Expectation::Meridiem {
                            pending: pending.clone(),
                            origin: *origin,
                            asked_again: true,
                        },
                        *asked_again,
                        Line::PleaseSayAmOrPm,
                        now,
                    ),
                }
                true
            }
            Expectation::CancelMeridiem {
                hour,
                minute,
                origin,
                asked_again,
            } => {
                match self.grammar.meridiem(text) {
                    Some(meridiem) => {
                        let time = SaidTime::Ambiguous {
                            hour: *hour,
                            minute: *minute,
                        }
                        .with_meridiem(meridiem);
                        self.set_outcome(TurnOutcome::Alarm);
                        self.alert_call(
                            AlertRequest::ListAlarms,
                            AlertWait::ListForCancel {
                                cancel: AlarmCancel::At(SaidTime::Exact(time)),
                                origin: *origin,
                            },
                        );
                    }
                    None => self.ask_again(
                        Expectation::CancelMeridiem {
                            hour: *hour,
                            minute: *minute,
                            origin: *origin,
                            asked_again: true,
                        },
                        *asked_again,
                        Line::PleaseSayAmOrPm,
                        now,
                    ),
                }
                true
            }
            Expectation::WhichTimer {
                timers,
                asked_again,
            } => {
                let pick = self
                    .grammar
                    .which(text)
                    .map_or(TimerPick::NoMatch, |reply| reply.choose_timer(timers));
                match pick {
                    TimerPick::Chosen(chosen) => {
                        self.set_outcome(TurnOutcome::Timer);
                        if self.ha_device.is_none() {
                            self.went_wrong(now);
                        } else {
                            self.request_timers(TimerRequest::Chosen(chosen), now);
                        }
                    }
                    TimerPick::Declined => {
                        self.set_outcome(TurnOutcome::Clarification);
                        self.say(Line::LeftIt, now);
                    }
                    TimerPick::NoMatch => self.ask_again(
                        Expectation::WhichTimer {
                            timers: timers.clone(),
                            asked_again: true,
                        },
                        *asked_again,
                        Line::WhichTimerAgain,
                        now,
                    ),
                }
                true
            }
            Expectation::WhichAlarm {
                alarms,
                origin,
                asked_again,
            } => {
                let choice = self
                    .grammar
                    .which(text)
                    .map(|reply| reply.choose_alarms(alarms));
                match choice {
                    Some(Choice::Chosen(chosen)) => {
                        self.set_outcome(TurnOutcome::Alarm);
                        let plan = AlarmCancelPlan::Cancel(chosen);
                        self.cancel_alarms(plan, *origin, now);
                    }
                    Some(Choice::Declined) => {
                        self.set_outcome(TurnOutcome::Clarification);
                        self.say(Line::LeftIt, now);
                    }
                    Some(Choice::NoMatch) | None => self.ask_again(
                        Expectation::WhichAlarm {
                            alarms: alarms.clone(),
                            origin: *origin,
                            asked_again: true,
                        },
                        *asked_again,
                        Line::WhichAlarm,
                        now,
                    ),
                }
                true
            }
            Expectation::Conversation { .. } | Expectation::Retry | Expectation::HaConversation => {
                false
            }
        }
    }

    /// An unclear answer: ask once more, then give up (§4.4 "Okay, I left it.").
    fn ask_again(&mut self, again: Expectation, asked_again: bool, line: Line, now: Now) {
        self.set_outcome(TurnOutcome::Clarification);
        if asked_again {
            self.say(Line::LeftIt, now);
        } else {
            self.ask(line, again, now);
        }
    }

    fn repeat(&mut self, now: Now) {
        self.set_outcome(TurnOutcome::Repeat);
        let recent = self
            .last_answer
            .as_ref()
            .filter(|last| now.at.saturating_duration_since(last.ended_at).as_secs() <= 120)
            .cloned();
        match recent {
            Some(last) => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.answering.follow_up = last.follow_up.clone();
                }
                self.speak_answer(last.url, now);
            }
            None => self.say(Line::NothingToRepeat, now),
        }
    }

    fn set_alarm(&mut self, request: AlarmRequest, origin: Option<Uuid>, now: Now) {
        self.set_outcome(TurnOutcome::Alarm);
        let Some(local) = self.local_now(now) else {
            // Without HA's time zone no alarm time can be placed: report failure.
            self.went_wrong(now);
            return;
        };
        let origin = origin.unwrap_or_else(|| self.turn_uuid());
        let op_id = OpId::voice_alarm(&origin.to_string());
        match request.set_alarm(op_id, self.device.clone(), local.datetime()) {
            Ok(set) => self.alert_call(AlertRequest::SetAlarm(set), AlertWait::Set { request }),
            Err(_) => self.say(Line::NoSuchDate, now),
        }
    }

    fn cancel_alarms(&mut self, plan: AlarmCancelPlan, origin: Uuid, now: Now) {
        let op_id = OpId::voice_alarm(&origin.to_string());
        match plan.request(op_id, self.device.clone()) {
            Some(request) => {
                self.alert_call(AlertRequest::CancelAlarms(request), AlertWait::Cancel)
            }
            None => self.went_wrong(now),
        }
    }

    pub(crate) fn alert_reply(&mut self, call: crate::io::CallId, reply: AlertReply, now: Now) {
        let Some(Call::Alert { turn }) = self.calls.remove(&call) else {
            self.counters.stale_results += 1;
            return;
        };
        if self.turn.as_ref().map(|t| t.uuid) != Some(turn) {
            self.counters.stale_results += 1;
            return;
        }
        let Some(wait) = self.turn.as_mut().and_then(|t| t.alert_wait.take()) else {
            return;
        };
        if let Some(turn) = self.turn.as_mut() {
            turn.facts.intent_ended = Some(now.at);
        }
        match (wait, reply) {
            (AlertWait::Act, AlertReply::Act(result)) => match result {
                // Stopped or snoozed, or the ring had already ended: the sound stopping is
                // the answer.
                Ok(
                    ActOutcome::Applied { .. }
                    | ActOutcome::Pending { .. }
                    | ActOutcome::Refused(_)
                    | ActOutcome::Rejected { .. },
                ) => self.finish_turn(now),
                Ok(ActOutcome::Unknown { .. }) | Err(_) => self.not_sure(now),
            },
            (AlertWait::Set { request }, AlertReply::SetAlarm(result)) => match result {
                Ok(Outcome::Applied { result, .. }) => self.say(
                    Line::AlarmSet(SpokenAlarm::from_request(&request, &result.first_due_local)),
                    now,
                ),
                Ok(Outcome::Pending { .. }) => {
                    // Journaled but not yet confirmed: never said to be stored. The time
                    // said is when it would first ring, computed here.
                    let due = self.first_due_text(&request, now).unwrap_or_default();
                    self.say(
                        Line::AlarmSaving(SpokenAlarm::from_request(&request, &due)),
                        now,
                    );
                }
                Ok(Outcome::Refused(_) | Outcome::Rejected { .. }) | Err(_) => self.went_wrong(now),
            },
            (AlertWait::ListForCancel { cancel, origin }, AlertReply::ListAlarms(result)) => {
                let Some(entries) = list(result) else {
                    self.went_wrong(now);
                    return;
                };
                let choices = AlarmChoice::from_entries(&entries);
                match cancel.plan(&choices) {
                    AlarmCancelPlan::NoAlarms | AlarmCancelPlan::NotFound => {
                        self.say(Line::AlarmNotFound, now)
                    }
                    AlarmCancelPlan::WhichOne(alarms) => self.ask(
                        Line::WhichAlarm,
                        Expectation::WhichAlarm {
                            alarms,
                            origin,
                            asked_again: false,
                        },
                        now,
                    ),
                    AlarmCancelPlan::AskMeridiem => {
                        let AlarmCancel::At(SaidTime::Ambiguous { hour, minute }) = cancel else {
                            self.went_wrong(now);
                            return;
                        };
                        self.set_outcome(TurnOutcome::Clarification);
                        self.ask(
                            Line::AmOrPm,
                            Expectation::CancelMeridiem {
                                hour,
                                minute,
                                origin,
                                asked_again: false,
                            },
                            now,
                        );
                    }
                    plan @ (AlarmCancelPlan::Cancel(_) | AlarmCancelPlan::CancelAll(_)) => {
                        self.cancel_alarms(plan, origin, now)
                    }
                }
            }
            (AlertWait::Cancel, AlertReply::CancelAlarms(result)) => match result {
                Ok(CancelOutcome::Journaled { targets, .. }) => {
                    let applied = targets
                        .iter()
                        .filter(|t| matches!(t.outcome, Outcome::Applied { .. }))
                        .count();
                    if targets
                        .iter()
                        .any(|t| matches!(t.outcome, Outcome::Pending { .. }))
                    {
                        self.not_sure(now);
                    } else if applied == 0 {
                        self.went_wrong(now);
                    } else {
                        self.say(Line::AlarmsCancelled(applied), now);
                    }
                }
                Ok(CancelOutcome::Refused(Refusal::NothingToCancel)) => {
                    self.say(Line::AlarmNotFound, now)
                }
                Ok(CancelOutcome::Refused(_)) | Err(_) => self.went_wrong(now),
            },
            (AlertWait::Query(query), AlertReply::ListAlarms(result)) => match list(result) {
                Some(entries) => {
                    let choices = AlarmChoice::from_entries(&entries);
                    let spoken: Vec<SpokenAlarm> =
                        choices.iter().map(SpokenAlarm::from_choice).collect();
                    let line = match query {
                        AlarmQuery::List => Line::Alarms(spoken),
                        AlarmQuery::Next => Line::NextAlarm(spoken.into_iter().next()),
                    };
                    self.say(line, now);
                }
                None => self.say(Line::AlarmsUnavailable, now),
            },
            _ => self.counters.stale_results += 1,
        }
    }

    // ---- timers ---------------------------------------------------------------------

    /// Asks HA for its timers (`HassTimerStatus`, which changes nothing); the request's
    /// time is the turn's `intent_ms`.
    fn request_timers(&mut self, request: TimerRequest, now: Now) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.facts.intent_started = Some(now.at);
        turn.timer_request = Some(request);
        let uuid = turn.uuid;
        let scope = turn.scope.clone();
        let call = self.call_id();
        self.calls.insert(call, Call::TimerStatus { turn: uuid });
        self.link.effects.push(Effect::Ha {
            call,
            scope,
            request: HaRequest::TimerStatus {
                ha_device: self.ha_device.clone(),
            },
        });
    }

    /// HA listed its timers, or could not.
    pub(crate) fn timer_status(&mut self, result: Result<TimerStatus, HaError>, now: Now) {
        let Some(request) = self.turn.as_mut().and_then(|t| t.timer_request.take()) else {
            self.counters.stale_results += 1;
            return;
        };
        let listed = match (result, self.ha_device.clone()) {
            (Ok(status), Some(device)) => Some((status, device)),
            (Err(error), _) => {
                tracing::warn!(device = %self.device, %error, "HA's timer list failed");
                None
            }
            (Ok(_), None) => None,
        };
        let Some((status, device)) = listed else {
            match request {
                // HA could not say which timers run: the request is HA's after all.
                TimerRequest::Cancel { text, .. } => self.dispatch_to_ha(text, now),
                TimerRequest::Chosen(_) => self.went_wrong(now),
            }
            return;
        };
        if let Some(turn) = self.turn.as_mut() {
            turn.facts.intent_ended = Some(now.at);
            turn.facts.response_type = Some(status.response_type);
        }
        self.set_outcome(TurnOutcome::Timer);
        let timers = SpeakerTimers::new(status.timers, device);
        let plan = match &request {
            TimerRequest::Cancel { cancel, .. } => timers.plan(*cancel),
            TimerRequest::Chosen(chosen) => timers.plan_chosen(chosen),
        };
        match plan {
            TimerCancelPlan::NoTimers => self.say(Line::NoTimers, now),
            TimerCancelPlan::WhichOne(timers) => {
                self.set_outcome(TurnOutcome::Clarification);
                let spoken = timers.iter().map(SpokenTimer::from_timer).collect();
                self.ask(
                    Line::WhichTimer(spoken),
                    Expectation::WhichTimer {
                        timers,
                        asked_again: false,
                    },
                    now,
                );
            }
            TimerCancelPlan::AlreadyFinished(timer) => {
                self.say(Line::TimerFinished(SpokenTimer::from_timer(&timer)), now)
            }
            TimerCancelPlan::Indistinct { timer, count } => self.say(
                Line::TimersIndistinct {
                    timer: SpokenTimer::from_timer(&timer),
                    count,
                },
                now,
            ),
            TimerCancelPlan::Cancel(run) => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.timers_cancelling = Some(run);
                }
                self.next_timer_cancel(now);
            }
        }
    }

    /// Sends the next cancel, or says what was cancelled once none is left.
    fn next_timer_cancel(&mut self, now: Now) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let Some(run) = turn.timers_cancelling.as_mut() else {
            return;
        };
        let Some(intent) = run.next_intent() else {
            let run = turn.timers_cancelling.take().expect("present");
            let line = Line::TimersCancelled {
                timers: run
                    .cancelled()
                    .iter()
                    .map(SpokenTimer::from_timer)
                    .collect(),
                total: run.total(),
            };
            self.say(line, now);
            return;
        };
        let uuid = turn.uuid;
        let scope = turn.scope.clone();
        let call = self.call_id();
        self.calls.insert(call, Call::CancelTimers { turn: uuid });
        self.link.effects.push(Effect::Ha {
            call,
            scope,
            request: HaRequest::CallIntent {
                intent,
                ha_device: self.ha_device.clone(),
            },
        });
    }

    /// HA answered one cancel. A refused or unsent cancel leaves that timer running and
    /// the rest go on; one whose outcome is unknown stops the run, never re-sent (§3.3.3).
    pub(crate) fn timer_cancelled(&mut self, outcome: EffectOutcome<()>, now: Now) {
        let Some(run) = self
            .turn
            .as_mut()
            .and_then(|t| t.timers_cancelling.as_mut())
        else {
            self.counters.stale_results += 1;
            return;
        };
        match outcome {
            EffectOutcome::Applied(()) => run.answered(true),
            EffectOutcome::Rejected { code, message } => {
                tracing::warn!(device = %self.device, code, message, "HA refused a timer cancel");
                run.answered(false);
            }
            EffectOutcome::NotDispatched(reason) => {
                tracing::warn!(device = %self.device, ?reason, "a timer cancel was not sent");
                run.answered(false);
            }
            EffectOutcome::Unknown(cause) => {
                tracing::warn!(device = %self.device, ?cause, "a timer cancel's outcome is unknown");
                if let Some(turn) = self.turn.as_mut() {
                    turn.timers_cancelling = None;
                }
                self.not_sure(now);
                return;
            }
        }
        self.next_timer_cancel(now);
    }
}

fn list(
    result: Result<Result<Vec<AlarmEntry>, Refusal>, echomuse_alerts::ServiceStopped>,
) -> Option<Vec<AlarmEntry>> {
    result.ok()?.ok()
}

impl Voice {
    /// When `request` would first ring, as local RFC 3339 text in HA's zone.
    fn first_due_text(&self, request: &AlarmRequest, now: Now) -> Option<String> {
        let zone = self.time_zone.as_ref()?;
        let local = zone.at_ms(now.wall_ms)?;
        let when = request.when(local.datetime()).ok()?;
        let instant = jiff::Timestamp::from_millisecond(now.wall_ms).ok()?;
        let due = when.first_due(instant, zone).ok()?;
        Some(echomuse_alerts::schedule::local_text(&due))
    }
}
