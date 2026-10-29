//! Transcript correction's shared tables (spec §4.7, §5.4 "Correction tables").
//!
//! One background thread (the correction crate's [`TableBuilder`]) builds the tables from
//! the local grammar's fixed words, the names Home Assistant's Assist matches against
//! (exposed entities' names and aliases, areas, floors) and the known phrases: every
//! sentence of the enabled automations' conversation triggers (each template expanded,
//! at most [`TRIGGER_SENTENCES`] sentences, a template saying more left out), and the
//! requests Home Assistant answered before, as they were sent ([`AnsweredRequests`]).
//! Each build is published whole on a `watch`. A task here keeps them current: when Home
//! Assistant connects (and on each reconnect) the tables are withdrawn, HA's names and
//! automations are read and a build is queued; a change to HA's entity, device, area or
//! floor registries or to its automations (`automation_reloaded`) queues another, two
//! seconds after the last change of a burst; new turn rows queue one too, at most once a
//! minute, for the requests HA answered. Nothing here waits on a device's thread.
//!
//! Each device session compiles its speaker's own preferences (terms, phrases,
//! pronunciations) against the published tables on the same builder thread
//! ([`CorrectionTables::compile`]), and uses the pair as its [`Snapshot`] per turn.

use std::{sync::Arc, time::Duration};

use echomuse_correction::{
    KnownPhrase, PhraseSource, Preferences, Snapshot, TableBuilder, TableInputs, Tables,
};
use echomuse_db::DbHandle;
use echomuse_grammar::{Grammar, Vocabulary};
use echomuse_ha::{AUTOMATIONS_EVENT, HaClient, VOCABULARY_EVENTS};
use echomuse_records::{AnsweredRequests, TurnStored};
use echomuse_settings::settings::TranscriptSettings;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::supervision::TaskSupervisor;

/// How long a burst of registry changes may run before the tables are rebuilt.
const SETTLE: Duration = Duration::from_secs(2);
/// How long a failed read of HA's names waits before it is tried again.
const RETRY: Duration = Duration::from_secs(10);
/// The least time between rebuilds for new turn rows.
const HISTORY_SETTLE: Duration = Duration::from_secs(60);
/// The most sentences one trigger template may say to be taken as known phrases.
pub const TRIGGER_SENTENCES: usize = 256;
/// The most earlier requests taken as known phrases.
pub const HISTORY_REQUESTS: u32 = 5_000;

/// The published correction tables and the builder that compiles preferences against
/// them. Cheap to clone.
#[derive(Clone, Debug)]
pub struct CorrectionTables {
    builder: Arc<TableBuilder>,
    tables: watch::Receiver<Option<Arc<Tables>>>,
}

impl CorrectionTables {
    /// Starts the builder thread and the task that keeps the tables current with `ha`,
    /// reading earlier answered requests through `db` and rebuilding for new `turns`.
    pub fn start(
        ha: HaClient,
        grammar: Grammar,
        db: DbHandle,
        turns: broadcast::Receiver<TurnStored>,
        tasks: &TaskSupervisor,
    ) -> std::io::Result<Self> {
        let (builder, tables) = TableBuilder::spawn()?;
        let builder = Arc::new(builder);
        let sources = Sources {
            ha,
            grammar,
            db,
            builder: Arc::clone(&builder),
        };
        tasks.spawn("correction tables", keep_current(sources, turns));
        Ok(Self { builder, tables })
    }

    /// The current tables; `None` while none are usable.
    pub fn tables(&self) -> watch::Receiver<Option<Arc<Tables>>> {
        self.tables.clone()
    }

    /// Compiles a speaker's preferences against `tables` on the builder thread; the
    /// receiver yields nothing if the thread stopped.
    pub fn compile(
        &self,
        tables: Arc<Tables>,
        settings: TranscriptSettings,
    ) -> oneshot::Receiver<Arc<Preferences>> {
        self.builder.compile_preferences(tables, settings)
    }

    /// A speaker's snapshot: `tables` and its preferences compiled against them.
    pub async fn snapshot(
        &self,
        tables: Arc<Tables>,
        settings: TranscriptSettings,
    ) -> Option<Snapshot> {
        let preferences = self.compile(Arc::clone(&tables), settings).await.ok()?;
        Some(Snapshot {
            tables,
            preferences,
        })
    }
}

/// What the tables are built from.
struct Sources {
    ha: HaClient,
    grammar: Grammar,
    db: DbHandle,
    builder: Arc<TableBuilder>,
}

/// The known phrases of conversation-trigger `templates`: each template's sentences, a
/// template saying more than [`TRIGGER_SENTENCES`] (or that does not parse) left out.
pub fn trigger_phrases(templates: &[String]) -> Vec<KnownPhrase> {
    let mut phrases = Vec::new();
    for template in templates {
        match echomuse_grammar::expand_template(template, TRIGGER_SENTENCES) {
            Ok(Some(sentences)) => phrases.extend(sentences.into_iter().map(|text| KnownPhrase {
                text,
                source: PhraseSource::Trigger,
            })),
            Ok(None) => {
                tracing::debug!(template, "a trigger says too many sentences to use whole");
            }
            Err(error) => tracing::debug!(%error, "a trigger template did not parse"),
        }
    }
    phrases
}

impl Sources {
    /// Reads HA's names, its automations' triggers and the answered requests, and queues
    /// a build; `false` when HA's names could not be read. Triggers or requests that
    /// cannot be read leave their phrases out of this build.
    async fn build(&self) -> bool {
        let names = match self.ha.assist_vocabulary().await {
            Ok(names) => names,
            Err(error) => {
                tracing::warn!(%error, "Home Assistant's names could not be read for transcript correction");
                return false;
            }
        };
        let mut phrases = match self.ha.automation_sentences().await {
            Ok(templates) => trigger_phrases(&templates),
            Err(error) => {
                tracing::warn!(%error, "automation triggers could not be read for transcript correction");
                Vec::new()
            }
        };
        match self
            .db
            .read(AnsweredRequests {
                limit: HISTORY_REQUESTS,
            })
            .await
        {
            Ok(texts) => phrases.extend(texts.into_iter().map(|text| KnownPhrase {
                text,
                source: PhraseSource::History,
            })),
            Err(error) => {
                tracing::warn!(%error, "earlier requests could not be read for transcript correction");
            }
        }
        let vocabulary = Vocabulary {
            names: Some(names.names),
            areas: Some(names.areas),
            floors: Some(names.floors),
        };
        let revision = self
            .builder
            .rebuild(TableInputs::new(&self.grammar, vocabulary, phrases));
        tracing::debug!(revision, "correction tables queued");
        true
    }
}

async fn keep_current(sources: Sources, mut turns: broadcast::Receiver<TurnStored>) {
    let ha = sources.ha.clone();
    let mut status = ha.status();
    let mut turns_open = true;
    loop {
        let generation = match status.wait_for(|s| s.generation.is_some()).await {
            Ok(status) => status.generation,
            Err(_) => return,
        };
        // Tables built from an older connection's names may be wrong now.
        sources.builder.withdraw();
        let (changes, mut changed) = mpsc::channel::<()>(64);
        let mut forwarders = Vec::new();
        for event in VOCABULARY_EVENTS.into_iter().chain([AUTOMATIONS_EVENT]) {
            match ha.subscribe_events(event).await {
                Ok(mut subscription) => {
                    let changes = changes.clone();
                    forwarders.push(tokio::spawn(async move {
                        while subscription.events.recv().await.is_some() {
                            let _ = changes.try_send(());
                        }
                    }));
                }
                Err(error) => {
                    tracing::warn!(event, %error, "an event could not be followed; correction tables may go stale until HA reconnects");
                }
            }
        }
        drop(changes);
        let mut built = sources.build().await;
        let mut history_due: Option<tokio::time::Instant> = None;
        loop {
            let retry = async {
                if built {
                    std::future::pending::<()>().await;
                } else {
                    tokio::time::sleep(RETRY).await;
                }
            };
            let history = async {
                match history_due {
                    Some(due) => tokio::time::sleep_until(due).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                moved = status.changed() => {
                    if moved.is_err() {
                        return;
                    }
                    if status.borrow_and_update().generation != generation {
                        break;
                    }
                }
                change = changed.recv(), if !changed.is_closed() => {
                    if change.is_none() {
                        continue;
                    }
                    // Let a burst of changes settle, then build once.
                    while let Ok(Some(())) = tokio::time::timeout(SETTLE, changed.recv()).await {}
                    built = sources.build().await;
                }
                notice = turns.recv(), if turns_open => match notice {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        history_due.get_or_insert_with(|| {
                            tokio::time::Instant::now() + HISTORY_SETTLE
                        });
                    }
                    Err(broadcast::error::RecvError::Closed) => turns_open = false,
                },
                () = history => {
                    history_due = None;
                    built = sources.build().await;
                }
                () = retry => built = sources.build().await,
            }
        }
        for forwarder in forwarders {
            forwarder.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_templates_become_their_sentences() {
        let phrases = trigger_phrases(&[
            "(arm|set) the alarm".to_owned(),
            "turn off {area}".to_owned(),
            "(a|b|c|d) (e|f|g|h) (i|j|k|l) (m|n|o|p) (q|r|s|t)".to_owned(),
        ]);
        let texts: Vec<&str> = phrases.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, ["arm the alarm", "set the alarm"]);
        assert!(phrases.iter().all(|p| p.source == PhraseSource::Trigger));
    }
}
