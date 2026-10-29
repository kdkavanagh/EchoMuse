//! Transcript correction's tools for the dashboard (spec §4.7): trying a transcript as `on`
//! would correct it, and listing the words whose pronunciation is only guessed.
//!
//! Both use the shared correction tables ([`CorrectionTables`]) and compile the speaker's
//! (or the fleet's) preferences against them on the table builder's thread. A trial asks
//! Home Assistant's non-executing sentence matcher, with the speaker's satellite device
//! and pipeline when HA knows them; nothing runs. It is prepared on a blocking thread, as
//! a turn's correction is, and its check runs under `on`'s own deadline
//! ([`echomuse_correction::FINAL_DEADLINE`]) from the moment the matcher is asked, so the
//! record says what `on` would really have sent.

use std::sync::Arc;

use echomuse_api::{
    BoxFuture, LocalRoute, ServiceError, SoundedWord, TextFixError, TextFixService, TextFixTrial,
    TextFixTrialRequest, UnknownWord,
};
use echomuse_core::{
    services::{HaConnectionGeneration, HomeAssistant},
    types::{DeviceId, RuntimeId, SessionId, WorkIdentity, WorkScope},
};
use echomuse_correction::{
    FixContext, HaContext, Lexicon, Preferences, Prepared, Pronouncer, Snapshot, SoundSource,
    Tables, TurnText,
};
use echomuse_grammar::{Grammar, Route};
use echomuse_ha::HaClient;
use echomuse_settings::{
    service::SettingsHandle, settings::TranscriptSettings, values::TranscriptFixMode,
};
use tokio_util::sync::CancellationToken;

use crate::correction::CorrectionTables;

/// The dashboard's transcript correction tools.
pub struct DashboardTextFix {
    pub ha: HaClient,
    pub grammar: Grammar,
    pub settings: SettingsHandle,
    pub correction: CorrectionTables,
}

fn not_ready(why: &str) -> TextFixError {
    TextFixError::NotReady(why.to_owned())
}

/// The scope of a dashboard request: no device runtime issued it, so it carries the
/// "no runtime yet" identity 0 that no runtime or session ever takes.
fn dashboard_scope(device: Option<&DeviceId>) -> WorkScope {
    WorkScope::new(
        WorkIdentity {
            device: device.cloned().unwrap_or_else(|| DeviceId(String::new())),
            runtime: RuntimeId(0),
            session: SessionId(0),
            turn: None,
        },
        CancellationToken::new(),
    )
}

fn source_word(source: SoundSource) -> &'static str {
    match source {
        SoundSource::Guide => "guide",
        SoundSource::Dictionary => "dictionary",
        SoundSource::Number => "number",
        SoundSource::Letters => "letters",
        SoundSource::Compound => "compound",
        SoundSource::Spelling => "spelling",
    }
}

/// How each word of `text` is pronounced, by correction's own word split.
fn sounded(text: &str, lexicon: &Lexicon, preferences: &Preferences) -> Vec<SoundedWord> {
    let pronouncer = Pronouncer::new(lexicon, preferences.guides());
    echomuse_correction::words(text)
        .into_iter()
        .map(|word| match pronouncer.word(&word) {
            Some(sounds) => SoundedWord {
                phonemes: sounds.variants.iter().map(ToString::to_string).collect(),
                source: Some(source_word(sounds.source)),
                word,
            },
            None => SoundedWord {
                word,
                phonemes: Vec::new(),
                source: None,
            },
        })
        .collect()
}

fn local_route(route: &Route) -> Option<LocalRoute> {
    match route {
        Route::Repeat => Some(LocalRoute::Repeat),
        Route::SetAlarm(_) | Route::AskMeridiem(_) | Route::CancelAlarms(_) => {
            Some(LocalRoute::Alarm)
        }
        Route::AlarmQuery(_) => Some(LocalRoute::AlarmQuery),
        Route::CancelTimers(_) => Some(LocalRoute::TimerCancel),
        Route::SendToHa => None,
    }
}

impl DashboardTextFix {
    /// The speaker's transcript settings, or the fleet's.
    fn transcript(&self, device: Option<&DeviceId>) -> TranscriptSettings {
        match device {
            Some(device) => self.settings.effective(device).transcript.clone(),
            None => self.settings.snapshot().fleet().transcript.clone(),
        }
    }

    /// The wake phrase of the model the speaker (or the fleet) selects.
    fn wake_phrase(&self, device: Option<&DeviceId>) -> Option<String> {
        let snapshot = self.settings.snapshot();
        let model = match device {
            Some(device) => snapshot.selected_model(device).ok(),
            None => snapshot
                .registry()
                .model(&snapshot.fleet().wakeword.wake_model.0),
        }?;
        Some(model.wake_phrase().as_str().to_owned())
    }

    /// The current tables and `settings`' preferences compiled against them.
    async fn snapshot(&self, settings: TranscriptSettings) -> Result<Snapshot, TextFixError> {
        let tables: Arc<Tables> =
            self.correction.tables().borrow().clone().ok_or_else(|| {
                not_ready("The correction tables are being built; try again soon")
            })?;
        self.correction
            .snapshot(tables, settings)
            .await
            .ok_or_else(|| not_ready("The correction table builder is not running"))
    }

    async fn run_trial(&self, request: TextFixTrialRequest) -> Result<TextFixTrial, TextFixError> {
        let device = request.device.as_ref();
        let saved = self.transcript(device);
        let mode = saved.transcript_fix.as_str();
        // The trial says what `on` would send, so it runs under the saved settings with
        // the mode set to `on`.
        let mut on = saved;
        on.transcript_fix = TranscriptFixMode::On;
        let snapshot = self.snapshot(on.clone()).await?;
        let wake_phrase = request.wake.then(|| self.wake_phrase(device)).flatten();
        let route = local_route(&self.grammar.route(&request.text));

        let Some(generation) = self.ha.status().borrow().generation else {
            return Err(not_ready("Home Assistant is not connected"));
        };
        let scope = dashboard_scope(device);
        let (ha_device, satellite) = match device {
            None => (None, None),
            Some(device) => {
                let mac = echomuse_discovery::voice_mac(device).to_string();
                match self.ha.device_with_mac(&mac).await {
                    Ok(found) => {
                        let known = found.is_some();
                        (found, Some(known))
                    }
                    Err(_) => (None, None),
                }
            }
        };
        let pipeline = self
            .ha
            .pipeline_for(scope.clone(), ha_device.clone())
            .await
            .map_err(|error| {
                TextFixError::Service(ServiceError::Upstream(format!(
                    "Home Assistant did not name the satellite's pipeline: {error}"
                )))
            })?;
        let language = self
            .ha
            .pipeline_language(&scope, &pipeline)
            .await
            .ok()
            .flatten();
        let context = HaContext {
            connection: generation,
            ha_device,
            pipeline: Some(pipeline),
            language: language.clone(),
        };
        let turn = TurnText {
            heard: request.text.clone(),
            wake_phrase: wake_phrase.clone(),
            contextual: false,
        };
        let prepared = {
            let snapshot = snapshot.clone();
            let on = on.clone();
            tokio::task::spawn_blocking(move || {
                echomuse_correction::prepare(&on, &turn, Some(&snapshot), &context)
            })
            .await
            .map_err(|_| {
                TextFixError::Service(ServiceError::Failed(
                    "the correction proposal failed".to_owned(),
                ))
            })?
        };
        let outcome = match prepared {
            Prepared::Done(outcome) => *outcome,
            Prepared::Check(check) => {
                let held = check.context().clone();
                let ha = self.ha.clone();
                let current = move || FixContext {
                    connection: ha
                        .status()
                        .borrow()
                        .generation
                        .unwrap_or(HaConnectionGeneration(0)),
                    ..held
                };
                echomuse_correction::run(
                    &self.ha,
                    *check,
                    scope,
                    tokio::time::Instant::now(),
                    current,
                )
                .await
            }
        };
        let lexicon = snapshot.tables.lexicon().clone();
        let preferences = snapshot.preferences.clone();
        let proposed = outcome
            .record
            .as_ref()
            .and_then(|record| record.proposed.clone());
        let heard_words = sounded(&request.text, &lexicon, &preferences);
        let proposed_words = proposed.map(|text| sounded(&text, &lexicon, &preferences));
        let record = outcome
            .record
            .as_ref()
            .and_then(|record| serde_json::from_str(&record.to_json()).ok());
        Ok(TextFixTrial {
            text: request.text,
            mode,
            route,
            record,
            heard_words,
            proposed_words,
            satellite,
            language,
            wake_phrase,
        })
    }

    async fn run_unknown_words(
        &self,
        device: Option<DeviceId>,
    ) -> Result<Vec<UnknownWord>, TextFixError> {
        let settings = self.transcript(device.as_ref());
        let snapshot = self.snapshot(settings).await?;
        tokio::task::spawn_blocking(move || {
            snapshot
                .tables
                .guessed_words(&snapshot.preferences)
                .into_iter()
                .map(|(word, count)| UnknownWord { word, count })
                .collect()
        })
        .await
        .map_err(|_| {
            TextFixError::Service(ServiceError::Failed(
                "listing the unknown words failed".to_owned(),
            ))
        })
    }
}

impl TextFixService for DashboardTextFix {
    fn trial(
        &self,
        request: TextFixTrialRequest,
    ) -> BoxFuture<'_, Result<TextFixTrial, TextFixError>> {
        Box::pin(self.run_trial(request))
    }

    fn unknown_words(
        &self,
        device: Option<DeviceId>,
    ) -> BoxFuture<'_, Result<Vec<UnknownWord>, TextFixError>> {
        Box::pin(self.run_unknown_words(device))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_local_route_has_the_dashboards_word() {
        let grammar = Grammar::new().unwrap();
        assert_eq!(
            local_route(&grammar.route("say that again")),
            Some(LocalRoute::Repeat)
        );
        assert_eq!(
            local_route(&grammar.route("turn on the kitchen light")),
            None
        );
        assert_eq!(
            serde_json::to_value(LocalRoute::AlarmQuery).unwrap(),
            "alarm_query"
        );
        assert_eq!(
            serde_json::to_value(LocalRoute::TimerCancel).unwrap(),
            "timer_cancel"
        );
    }
}
