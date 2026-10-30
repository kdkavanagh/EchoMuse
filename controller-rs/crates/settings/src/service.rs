//! The settings service (§5.4 "Configuration"): one task owns the stored documents and
//! the wake-model registry, applies edits one at a time, and publishes whole immutable
//! snapshots.
//!
//! Every change is written through the database service and published only after its
//! transaction committed, so a snapshot never shows a value the database does not hold.
//! A device runtime takes its [`DeviceSettings`] as an `Arc` when it needs them and keeps
//! that snapshot for the turn it opened; [`DeviceSettingsWatch`] wakes only when that
//! device's settings change.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use echomuse_core::types::DeviceId;
use echomuse_db::{DbHandle, ServiceError};
use echomuse_protocol::Sha256;
use echomuse_protocol::session::ReadyGrant;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::db::{
    ReadAllSettings, ReadDeviceSettings, StoreError, WriteDeviceConfig, WriteFleetConfig,
};
use crate::edit::{DeviceEdit, EditError, EditOutcome, SettingsPatch};
use crate::ready::{ComposeError, compose_ready};
use crate::registry::{AddOutcome, NewModel, RegistryError, WakeModel, WakeRegistry};
use crate::settings::{DeviceSettings, Scope, Section, SectionSet, SettingKey, SettingValue};
use crate::stored::{
    StoredConfig, StoredIssue, StoredSections, effective_settings, fleet_settings,
};

/// Where a wake model is selected.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Selector {
    /// The fleet's `wakeModel`.
    Fleet,
    /// A device's effective `wakeModel`.
    Device(DeviceId),
}

/// One device in a snapshot.
#[derive(Clone, Debug)]
pub struct DeviceEntry {
    settings: Arc<DeviceSettings>,
    overridden: SectionSet,
    issues: Vec<StoredIssue>,
}

impl DeviceEntry {
    /// Its effective settings.
    pub fn settings(&self) -> &Arc<DeviceSettings> {
        &self.settings
    }

    /// The sections it overrides.
    pub fn overridden(&self) -> &SectionSet {
        &self.overridden
    }

    /// What its stored `config` and `config_sections` hold that is not taken as a
    /// setting.
    pub fn issues(&self) -> &[StoredIssue] {
        &self.issues
    }
}

/// Everything the settings service publishes, as one immutable value.
#[derive(Clone, Debug)]
pub struct SettingsSnapshot {
    generation: u64,
    fleet: Arc<DeviceSettings>,
    fleet_issues: Vec<StoredIssue>,
    devices: HashMap<DeviceId, DeviceEntry>,
    registry: WakeRegistry,
}

impl SettingsSnapshot {
    /// Counts publications: each snapshot's is one more than the last.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The fleet's settings.
    pub fn fleet(&self) -> &Arc<DeviceSettings> {
        &self.fleet
    }

    /// What the stored fleet document holds that is not taken as a setting.
    pub fn fleet_issues(&self) -> &[StoredIssue] {
        &self.fleet_issues
    }

    /// One device; `None` when it has no row.
    pub fn device(&self, device: &DeviceId) -> Option<&DeviceEntry> {
        self.devices.get(device)
    }

    /// Every device with a row.
    pub fn devices(&self) -> impl Iterator<Item = (&DeviceId, &DeviceEntry)> {
        self.devices.iter()
    }

    /// A device's effective settings. A device without a row gets the fleet's, which is
    /// what a new row (config `{}`, sections `[]`) resolves to.
    pub fn effective(&self, device: &DeviceId) -> Arc<DeviceSettings> {
        self.devices
            .get(device)
            .map_or_else(|| self.fleet.clone(), |e| e.settings.clone())
    }

    /// The wake-model registry.
    pub fn registry(&self) -> &WakeRegistry {
        &self.registry
    }

    /// Where `graph_sha256` is selected: the fleet and every device whose effective
    /// settings name it.
    pub fn selections(&self, graph_sha256: &Sha256) -> Vec<Selector> {
        let mut out = Vec::new();
        if self.fleet.wakeword.wake_model.0 == *graph_sha256 {
            out.push(Selector::Fleet);
        }
        let mut devices: Vec<&DeviceId> = self
            .devices
            .iter()
            .filter(|(_, e)| e.settings.wakeword.wake_model.0 == *graph_sha256)
            .map(|(d, _)| d)
            .collect();
        devices.sort_by(|a, b| a.0.cmp(&b.0));
        out.extend(devices.into_iter().cloned().map(Selector::Device));
        out
    }

    /// The model a device's effective settings select.
    pub fn selected_model(&self, device: &DeviceId) -> Result<&WakeModel, ComposeError> {
        let settings = self.effective(device);
        let sha = &settings.wakeword.wake_model.0;
        self.registry
            .model(sha)
            .ok_or_else(|| ComposeError::ModelNotInRegistry(sha.clone()))
    }

    /// The device's `session.ready` grant: assets and detector from its effective
    /// settings and selected model, with the session's capture permission and whether to
    /// ask for the `afe` stream.
    pub fn ready_grant(
        &self,
        device: &DeviceId,
        runtime_sha256: Sha256,
        capture_permitted: bool,
        afe_metadata: bool,
    ) -> Result<ReadyGrant, ComposeError> {
        let settings = self.effective(device);
        let model = self.selected_model(device)?;
        compose_ready(
            &settings,
            model,
            runtime_sha256,
            capture_permitted,
            afe_metadata,
        )
    }
}

/// Why the settings could not be loaded.
#[derive(Debug)]
pub enum LoadError {
    /// The database did not answer.
    Database(String),
    /// The settings service has stopped.
    Closed,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(reason) => write!(f, "reading the settings: {reason}"),
            Self::Closed => f.write_str("the settings service has stopped"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Why a registry change was refused or failed.
#[derive(Debug)]
pub enum ModelError {
    /// The registry refused it or its files could not be written.
    Registry(RegistryError),
    /// The model is selected by the fleet or a device.
    InUse(Vec<Selector>),
    /// The file work stopped without a result.
    Interrupted,
    /// The settings service has stopped.
    Closed,
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(e) => e.fmt(f),
            Self::InUse(users) => {
                let names: Vec<String> = users
                    .iter()
                    .map(|u| match u {
                        Selector::Fleet => "the fleet".to_owned(),
                        Selector::Device(d) => d.to_string(),
                    })
                    .collect();
                write!(f, "the model is selected by {}", names.join(", "))
            }
            Self::Interrupted => f.write_str("the registry change stopped without a result"),
            Self::Closed => f.write_str("the settings service has stopped"),
        }
    }
}

impl std::error::Error for ModelError {}

/// The settings service stopped, so no more changes will come.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceStopped;

impl fmt::Display for ServiceStopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the settings service has stopped")
    }
}

impl std::error::Error for ServiceStopped {}

type Reply<T> = oneshot::Sender<T>;

enum Command {
    FleetEdit(SettingsPatch, Reply<Result<EditOutcome, EditError>>),
    DeviceEdit(DeviceId, DeviceEdit, Reply<Result<EditOutcome, EditError>>),
    Refresh(DeviceId, Reply<Result<Arc<SettingsSnapshot>, LoadError>>),
    AddModel(Box<NewModel>, Reply<Result<AddOutcome, ModelError>>),
    DeleteModel(Sha256, Reply<Result<Arc<SettingsSnapshot>, ModelError>>),
    Stop,
}

/// How callers reach the settings service. Cheap to clone.
#[derive(Clone)]
pub struct SettingsHandle {
    commands: mpsc::Sender<Command>,
    snapshots: watch::Receiver<Arc<SettingsSnapshot>>,
}

impl fmt::Debug for SettingsHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsHandle")
            .field("generation", &self.snapshots.borrow().generation)
            .finish_non_exhaustive()
    }
}

impl SettingsHandle {
    /// The current snapshot.
    pub fn snapshot(&self) -> Arc<SettingsSnapshot> {
        self.snapshots.borrow().clone()
    }

    /// A device's current effective settings (the fleet's for a device without a row).
    pub fn effective(&self, device: &DeviceId) -> Arc<DeviceSettings> {
        self.snapshots.borrow().effective(device)
    }

    /// Watch one device's effective settings.
    pub fn watch_device(&self, device: DeviceId) -> DeviceSettingsWatch {
        let mut snapshots = self.snapshots.clone();
        let current = snapshots.borrow_and_update().effective(&device);
        DeviceSettingsWatch {
            device,
            snapshots,
            current,
        }
    }

    async fn ask<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Option<T> {
        let (reply, answer) = oneshot::channel();
        self.commands.send(command(reply)).await.ok()?;
        answer.await.ok()
    }

    /// Store a fleet edit and publish it. Returns once it committed and was published.
    pub async fn apply_fleet_edit(&self, patch: SettingsPatch) -> Result<EditOutcome, EditError> {
        self.ask(|r| Command::FleetEdit(patch, r))
            .await
            .unwrap_or(Err(EditError::Closed))
    }

    /// Store a device edit and publish it. Returns once it committed and was published.
    pub async fn apply_device_edit(
        &self,
        device: DeviceId,
        edit: DeviceEdit,
    ) -> Result<EditOutcome, EditError> {
        self.ask(|r| Command::DeviceEdit(device, edit, r))
            .await
            .unwrap_or(Err(EditError::Closed))
    }

    /// Re-read one device's row (after admission created it, or after it was deleted)
    /// and publish the result.
    pub async fn refresh_device(
        &self,
        device: &DeviceId,
    ) -> Result<Arc<SettingsSnapshot>, LoadError> {
        let device = device.clone();
        self.ask(|r| Command::Refresh(device, r))
            .await
            .unwrap_or(Err(LoadError::Closed))
    }

    /// Add a validated model to the registry. It is not selected anywhere.
    pub async fn add_wake_model(&self, model: NewModel) -> Result<AddOutcome, ModelError> {
        self.ask(|r| Command::AddModel(Box::new(model), r))
            .await
            .unwrap_or(Err(ModelError::Closed))
    }

    /// Delete a model, refused while the fleet or any device's effective settings
    /// select it.
    pub async fn delete_wake_model(
        &self,
        graph_sha256: Sha256,
    ) -> Result<Arc<SettingsSnapshot>, ModelError> {
        self.ask(|r| Command::DeleteModel(graph_sha256, r))
            .await
            .unwrap_or(Err(ModelError::Closed))
    }
}

/// One device's effective settings, waking only when they change.
#[derive(Debug)]
pub struct DeviceSettingsWatch {
    device: DeviceId,
    snapshots: watch::Receiver<Arc<SettingsSnapshot>>,
    current: Arc<DeviceSettings>,
}

impl DeviceSettingsWatch {
    /// The settings as of the last change seen.
    pub fn current(&self) -> Arc<DeviceSettings> {
        self.current.clone()
    }

    /// Wait for the device's settings to change and return them.
    pub async fn changed(&mut self) -> Result<Arc<DeviceSettings>, ServiceStopped> {
        loop {
            self.snapshots.changed().await.map_err(|_| ServiceStopped)?;
            let next = self.snapshots.borrow_and_update().effective(&self.device);
            if !Arc::ptr_eq(&next, &self.current) {
                self.current = next.clone();
                return Ok(next);
            }
        }
    }
}

/// The running settings service.
#[derive(Debug)]
pub struct SettingsService {
    handle: SettingsHandle,
    task: JoinHandle<()>,
}

impl SettingsService {
    /// Load the fleet document and every device row, publish the first snapshot and
    /// start the service task on the current Tokio runtime.
    pub async fn start(db: DbHandle, registry: WakeRegistry) -> Result<Self, LoadError> {
        let rows = db.read(ReadAllSettings).await.map_err(load_error)?;
        let fleet = StoredConfig::parse(rows.fleet.as_deref());
        let devices = rows
            .devices
            .into_iter()
            .map(|row| {
                (
                    row.device.clone(),
                    StoredDevice::from_row(row.config, row.config_sections, row.use_global_config),
                )
            })
            .collect();
        let mut state = State {
            db,
            fleet,
            devices,
            registry,
            generation: 0,
            previous: None,
        };
        let first = Arc::new(state.build());
        let (publisher, snapshots) = watch::channel(first.clone());
        state.previous = Some(first);
        let (commands, receiver) = mpsc::channel(64);
        let task = tokio::spawn(state.run(receiver, publisher));
        Ok(Self {
            handle: SettingsHandle {
                commands,
                snapshots,
            },
            task,
        })
    }

    /// A handle for callers.
    pub fn handle(&self) -> SettingsHandle {
        self.handle.clone()
    }

    /// Finish the change under way, stop, and report how the task ended. A panic in
    /// the task is a shared-service failure the caller treats as fatal (§5.5).
    pub async fn shutdown(self) -> Result<(), tokio::task::JoinError> {
        let _ = self.handle.commands.send(Command::Stop).await;
        self.task.await
    }
}

fn load_error(error: ServiceError<StoreError>) -> LoadError {
    match error {
        ServiceError::Closed => LoadError::Database("the database service is closed".to_owned()),
        other => LoadError::Database(other.to_string()),
    }
}

fn edit_db_error(error: ServiceError<StoreError>) -> EditError {
    match error {
        ServiceError::Failed(StoreError::UnknownDevice(device)) => EditError::UnknownDevice(device),
        other => EditError::Database(other.to_string()),
    }
}

#[derive(Clone, Debug)]
struct StoredDevice {
    config: StoredConfig,
    sections: StoredSections,
    sections_text: String,
    use_global_config: bool,
}

impl StoredDevice {
    fn from_row(config: String, sections_text: String, use_global_config: bool) -> Self {
        Self {
            config: StoredConfig::parse(Some(&config)),
            sections: StoredSections::parse(&sections_text, use_global_config),
            sections_text,
            use_global_config,
        }
    }

    fn issues(&self) -> Vec<StoredIssue> {
        self.config
            .issues()
            .iter()
            .chain(self.sections.issues())
            .cloned()
            .collect()
    }
}

struct State {
    db: DbHandle,
    fleet: StoredConfig,
    devices: HashMap<DeviceId, StoredDevice>,
    registry: WakeRegistry,
    generation: u64,
    previous: Option<Arc<SettingsSnapshot>>,
}

/// Keep the earlier `Arc` when the value is unchanged, so watchers of unchanged
/// settings are not woken.
fn reuse(previous: Option<&Arc<DeviceSettings>>, next: DeviceSettings) -> Arc<DeviceSettings> {
    match previous {
        Some(p) if **p == next => p.clone(),
        _ => Arc::new(next),
    }
}

impl State {
    fn build(&mut self) -> SettingsSnapshot {
        self.generation += 1;
        let previous = self.previous.as_deref();
        let fleet = reuse(previous.map(|p| &p.fleet), fleet_settings(&self.fleet));
        let devices = self
            .devices
            .iter()
            .map(|(id, stored)| {
                let settings =
                    effective_settings(&fleet, &stored.config, stored.sections.overridden());
                let earlier = previous
                    .and_then(|p| p.devices.get(id))
                    .map(|e| &e.settings);
                let entry = DeviceEntry {
                    settings: reuse(earlier, settings),
                    overridden: stored.sections.overridden().clone(),
                    issues: stored.issues(),
                };
                (id.clone(), entry)
            })
            .collect();
        SettingsSnapshot {
            generation: self.generation,
            fleet,
            fleet_issues: self.fleet.issues().to_vec(),
            devices,
            registry: self.registry.clone(),
        }
    }

    fn publish(
        &mut self,
        publisher: &watch::Sender<Arc<SettingsSnapshot>>,
    ) -> Arc<SettingsSnapshot> {
        let snapshot = Arc::new(self.build());
        self.previous = Some(snapshot.clone());
        publisher.send_replace(snapshot.clone());
        snapshot
    }

    fn current(&self) -> Arc<SettingsSnapshot> {
        self.previous.clone().expect("published at start")
    }

    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        publisher: watch::Sender<Arc<SettingsSnapshot>>,
    ) {
        while let Some(command) = commands.recv().await {
            match command {
                Command::FleetEdit(patch, reply) => {
                    let _ = reply.send(self.fleet_edit(patch, &publisher).await);
                }
                Command::DeviceEdit(device, edit, reply) => {
                    let _ = reply.send(self.device_edit(device, edit, &publisher).await);
                }
                Command::Refresh(device, reply) => {
                    let _ = reply.send(self.refresh(device, &publisher).await);
                }
                Command::AddModel(model, reply) => {
                    let _ = reply.send(self.add_model(*model, &publisher).await);
                }
                Command::DeleteModel(sha, reply) => {
                    let _ = reply.send(self.delete_model(sha, &publisher).await);
                }
                Command::Stop => break,
            }
        }
    }

    /// Check what an edit's values do to the wake settings of `settings`: a selected
    /// model must be usable, and no extra open rule may repeat the model's own.
    fn check_wake<'a>(
        &self,
        values: impl Iterator<Item = &'a SettingValue> + Clone,
        settings: &DeviceSettings,
    ) -> Result<(), EditError> {
        let selected = &settings.wakeword.wake_model.0;
        let model = self.registry.model(selected).filter(|m| m.is_usable());
        if values.clone().any(|v| v.key() == SettingKey::WakeModel) && model.is_none() {
            return Err(EditError::UnknownWakeModel(selected.clone()));
        }
        let touches_rules = values
            .clone()
            .any(|v| matches!(v.key(), SettingKey::WakeModel | SettingKey::WakeOpenRules));
        if touches_rules && let Some(model) = model {
            let t = model.thresholds();
            if let Some(rule) = settings
                .wakeword
                .wake_open_rules
                .rules()
                .iter()
                .find(|r| r.is_baseline(t.idle(), t.playback()))
            {
                return Err(EditError::OpenRuleIsBaseline { rule: *rule });
            }
        }
        Ok(())
    }

    fn outcome(&self, before: &SettingsSnapshot, after: Arc<SettingsSnapshot>) -> EditOutcome {
        let mut changed: Vec<(DeviceId, bool)> = after
            .devices
            .iter()
            .filter_map(|(id, entry)| {
                let earlier = before.effective(id);
                let differences = earlier.differences(&entry.settings);
                (!differences.is_empty()).then(|| {
                    (
                        id.clone(),
                        differences.iter().any(|k| k.needs_new_session()),
                    )
                })
            })
            .collect();
        changed.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        EditOutcome {
            new_session_needed: changed
                .iter()
                .filter(|c| c.1)
                .map(|c| c.0.clone())
                .collect(),
            changed_devices: changed.into_iter().map(|c| c.0).collect(),
            snapshot: after,
        }
    }

    async fn fleet_edit(
        &mut self,
        patch: SettingsPatch,
        publisher: &watch::Sender<Arc<SettingsSnapshot>>,
    ) -> Result<EditOutcome, EditError> {
        let text = self
            .fleet
            .with_changes(patch.values())
            .map_err(|_| EditError::StoredUnreadable)?;
        let before = self.current();
        let Some(text) = text else {
            return Ok(self.outcome(&before, before.clone()));
        };
        let fleet = StoredConfig::parse(Some(&text));
        self.check_wake(patch.values(), &fleet_settings(&fleet))?;
        self.db
            .write(WriteFleetConfig { text })
            .await
            .map_err(edit_db_error)?;
        self.fleet = fleet;
        let after = self.publish(publisher);
        Ok(self.outcome(&before, after))
    }

    async fn device_edit(
        &mut self,
        device: DeviceId,
        edit: DeviceEdit,
        publisher: &watch::Sender<Arc<SettingsSnapshot>>,
    ) -> Result<EditOutcome, EditError> {
        if !self.devices.contains_key(&device) {
            self.refresh(device.clone(), publisher)
                .await
                .map_err(|e| EditError::Database(e.to_string()))?;
        }
        let Some(stored) = self.devices.get(&device) else {
            return Err(EditError::UnknownDevice(device));
        };
        let before = self.current();
        let old_sections = stored.sections.overridden().clone();
        let sections = edit
            .sections
            .clone()
            .unwrap_or_else(|| old_sections.clone());
        let fleet = fleet_settings(&self.fleet);

        let mut store: Vec<SettingValue> = edit
            .values
            .values()
            .filter(|v| match v.key().scope() {
                Scope::State => true,
                Scope::Section(section) => sections.contains(&section),
            })
            .cloned()
            .collect();
        // A section switched to the device starts from the fleet's current values.
        for section in sections.difference(&old_sections) {
            for key in section.keys() {
                if edit.values.get(key).is_none() {
                    store.push(fleet.get(key));
                }
            }
        }

        let text = stored
            .config
            .with_changes(store.iter())
            .map_err(|_| EditError::StoredUnreadable)?;
        let config = match &text {
            Some(t) => StoredConfig::parse(Some(t)),
            None => stored.config.clone(),
        };
        if sections.contains(&Section::Wakeword) {
            let effective = effective_settings(&fleet, &config, &sections);
            let own = store.iter().filter(|v| {
                edit.values.get(v.key()) == Some(*v) && v.key().section() == Some(Section::Wakeword)
            });
            self.check_wake(own, &effective)?;
        }

        let use_global_config = sections.is_empty();
        let sections_changed =
            sections != old_sections || use_global_config != stored.use_global_config;
        if text.is_none() && !sections_changed {
            return Ok(self.outcome(&before, before.clone()));
        }
        let sections_text = if sections_changed {
            stored.sections.render(&sections)
        } else {
            stored.sections_text.clone()
        };
        self.db
            .write(WriteDeviceConfig {
                device: device.clone(),
                config: text,
                config_sections: sections_text.clone(),
                use_global_config,
            })
            .await
            .map_err(edit_db_error)?;
        self.devices.insert(
            device,
            StoredDevice {
                config,
                sections: StoredSections::parse(&sections_text, use_global_config),
                sections_text,
                use_global_config,
            },
        );
        let after = self.publish(publisher);
        Ok(self.outcome(&before, after))
    }

    async fn refresh(
        &mut self,
        device: DeviceId,
        publisher: &watch::Sender<Arc<SettingsSnapshot>>,
    ) -> Result<Arc<SettingsSnapshot>, LoadError> {
        let row = self
            .db
            .read(ReadDeviceSettings {
                device: device.clone(),
            })
            .await
            .map_err(load_error)?;
        let changed = match row {
            Some(row) => {
                let next =
                    StoredDevice::from_row(row.config, row.config_sections, row.use_global_config);
                let same = self.devices.get(&device).is_some_and(|d| {
                    d.config.text() == next.config.text()
                        && d.sections_text == next.sections_text
                        && d.use_global_config == next.use_global_config
                });
                if !same {
                    self.devices.insert(device, next);
                }
                !same
            }
            None => self.devices.remove(&device).is_some(),
        };
        Ok(if changed {
            self.publish(publisher)
        } else {
            self.current()
        })
    }

    async fn add_model(
        &mut self,
        model: NewModel,
        publisher: &watch::Sender<Arc<SettingsSnapshot>>,
    ) -> Result<AddOutcome, ModelError> {
        let registry = self.registry.clone();
        let (registry, outcome) = tokio::task::spawn_blocking(move || registry.added(model))
            .await
            .map_err(|_| ModelError::Interrupted)?
            .map_err(ModelError::Registry)?;
        if matches!(outcome, AddOutcome::Added(_)) {
            self.registry = registry;
            self.publish(publisher);
        }
        Ok(outcome)
    }

    async fn delete_model(
        &mut self,
        graph_sha256: Sha256,
        publisher: &watch::Sender<Arc<SettingsSnapshot>>,
    ) -> Result<Arc<SettingsSnapshot>, ModelError> {
        let users = self.current().selections(&graph_sha256);
        if !users.is_empty() {
            return Err(ModelError::InUse(users));
        }
        let registry = self.registry.clone();
        let registry = tokio::task::spawn_blocking(move || registry.without(&graph_sha256))
            .await
            .map_err(|_| ModelError::Interrupted)?
            .map_err(ModelError::Registry)?;
        self.registry = registry;
        Ok(self.publish(publisher))
    }
}
