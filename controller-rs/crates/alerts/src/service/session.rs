//! Per Home Assistant connection: read HA's time zone, install the LLM tool scripts and
//! keep the `echomuse_alert_request` subscription open (§3.3.4).

use std::{collections::HashMap, sync::Arc, time::Duration};

use echomuse_core::services::{EffectOutcome, HaConnectionGeneration};
use echomuse_db::DbHandle;
use echomuse_ha::HaStatus;
use tokio::sync::{mpsc, watch};

use super::Msg;
use crate::{
    api::{AlertNotice, ScriptState},
    journal::{LoadScripts, RecordScript, ScriptRecord},
    ports::{AlertHa, RelayFeed},
    schedule::HaTimeZone,
    scripts::{SCRIPT_REVISION, ScriptDecision, ScriptSituation, decide, scripts},
};

pub(crate) struct HaSession<H> {
    pub ha: Arc<H>,
    pub db: DbHandle,
    pub status: watch::Receiver<HaStatus>,
    pub actor: mpsc::UnboundedSender<Msg>,
    pub scripts: watch::Sender<Vec<(String, ScriptState)>>,
    pub retry_initial: Duration,
    pub retry_max: Duration,
}

impl<H: AlertHa> HaSession<H> {
    fn notice(&self, notice: AlertNotice) {
        let _ = self.actor.send(Msg::Notice(notice));
    }

    pub(crate) async fn run(mut self) {
        let mut last = None;
        loop {
            let generation = loop {
                let current = self.status.borrow_and_update().generation;
                match current {
                    Some(g) if Some(g) != last => break g,
                    None if last.is_some() => {
                        last = None;
                        let _ = self.actor.send(Msg::HaDown);
                    }
                    _ => {}
                }
                if self.status.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            last = Some(generation);
            if !self.time_zone(generation).await {
                continue;
            }
            self.install_scripts().await;
            self.relay(generation).await;
        }
    }

    fn same(&self, generation: HaConnectionGeneration) -> bool {
        self.status.borrow().generation == Some(generation)
    }

    /// Reads the time zone, retrying while the connection lasts; false if it ended.
    async fn time_zone(&mut self, generation: HaConnectionGeneration) -> bool {
        let mut backoff = self.retry_initial;
        while self.same(generation) {
            match self.ha.time_zone().await {
                Ok(name) => {
                    let zone = HaTimeZone::named(&name).map_err(|e| e.to_string());
                    let _ = self.actor.send(Msg::HaUp(generation, zone));
                    return true;
                }
                Err(_) => {
                    let _ = tokio::time::timeout(backoff, self.status.changed()).await;
                    backoff = (backoff * 2).min(self.retry_max);
                }
            }
        }
        false
    }

    async fn relay(&mut self, generation: HaConnectionGeneration) {
        let mut backoff = self.retry_initial;
        while self.same(generation) {
            let mut feed = match self.ha.subscribe_relay().await {
                Ok(feed) => feed,
                Err(_) => {
                    let _ = tokio::time::timeout(backoff, self.status.changed()).await;
                    backoff = (backoff * 2).min(self.retry_max);
                    continue;
                }
            };
            backoff = self.retry_initial;
            loop {
                tokio::select! {
                    event = feed.next() => match event {
                        Some(data) => { let _ = self.actor.send(Msg::Relay(data)); }
                        None => break,
                    },
                    changed = self.status.changed() => {
                        if changed.is_err() || !self.same(generation) {
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Installs or upgrades each script unless the user edited it, then reloads scripts
    /// and exposes them to the conversation assistant.
    async fn install_scripts(&mut self) {
        let records: HashMap<String, ScriptRecord> = match self.db.read(LoadScripts).await {
            Ok(rows) => rows.into_iter().map(|r| (r.object_id.clone(), r)).collect(),
            Err(error) => {
                self.notice(AlertNotice::DatabaseProblem(format!(
                    "reading scripts: {error}"
                )));
                return;
            }
        };
        let mut states = Vec::new();
        let mut wrote = false;
        let mut exposable = Vec::new();
        for script in scripts() {
            let id = script.object_id.to_owned();
            let current = match self.ha.script_config(id.clone()).await {
                Ok(current) => current,
                Err(error) => {
                    states.push((id, ScriptState::Failed(format!("reading it: {error}"))));
                    continue;
                }
            };
            let record = records.get(&id);
            let situation = ScriptSituation {
                current: current.as_ref(),
                recorded: record.map(|r| (r.sha256.as_str(), r.revision)),
            };
            let sha = script.sha256();
            let state = match decide(&script, &situation) {
                ScriptDecision::Install => {
                    match self
                        .ha
                        .write_script_config(id.clone(), script.config.clone())
                        .await
                    {
                        EffectOutcome::Applied(()) => {
                            wrote = true;
                            self.record(&id, &sha).await;
                            ScriptState::Installed
                        }
                        EffectOutcome::Rejected { code, message } => {
                            ScriptState::Failed(format!("HA refused it: {code} {message}"))
                        }
                        EffectOutcome::NotDispatched(reason) => {
                            ScriptState::Failed(format!("not sent: {reason}"))
                        }
                        EffectOutcome::Unknown(cause) => {
                            ScriptState::Failed(format!("no answer: {cause:?}"))
                        }
                    }
                }
                ScriptDecision::Current => {
                    if record.is_none_or(|r| r.sha256 != sha || r.revision != SCRIPT_REVISION) {
                        self.record(&id, &sha).await;
                    }
                    ScriptState::Current
                }
                ScriptDecision::LeaveEdited => ScriptState::LeftEdited,
            };
            if matches!(state, ScriptState::Installed | ScriptState::Current) {
                exposable.push(script.entity_id());
            }
            states.push((id, state));
        }
        if wrote {
            let _ = self.ha.reload_scripts().await;
        }
        if !exposable.is_empty() {
            let _ = self.ha.expose_scripts(exposable).await;
        }
        for (object_id, state) in &states {
            if !matches!(state, ScriptState::Current) {
                self.notice(AlertNotice::Script {
                    object_id: object_id.clone(),
                    state: state.clone(),
                });
            }
        }
        self.scripts.send_replace(states);
    }

    async fn record(&self, object_id: &str, sha: &str) {
        let record = ScriptRecord {
            object_id: object_id.to_owned(),
            sha256: sha.to_owned(),
            revision: SCRIPT_REVISION,
        };
        if let Err(error) = self.db.write(RecordScript(record)).await {
            self.notice(AlertNotice::DatabaseProblem(format!(
                "recording script {object_id}: {error}"
            )));
        }
    }
}
