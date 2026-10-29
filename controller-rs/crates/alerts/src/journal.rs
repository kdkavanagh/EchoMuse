//! The alert tables of the database (§3.5.1; `crates/db/schema/v40.sql`): the operation
//! journal `alert_ops`, the per-speaker delivery position `alert_delivery`, and the
//! installed-script record `alert_scripts`.
//!
//! Every command runs on the database service's writer thread in its own transaction
//! and every query on a read-only connection, so no transaction waits on network I/O.
//!
//! # Row encodings
//!
//! The alert tables are empty in the real installations available to this rewrite, so
//! the JSON columns are specified here (the spec fixes only that the journal keeps each
//! request's canonical JSON and its digest):
//!
//! - `payload_json` is the canonical JSON of `{"plan": …, "request": …}`: the request as
//!   the caller made it, and the writes it resolved to when it was journaled (the event
//!   to create, or the targets of a group cancellation), so recovery repeats exactly the
//!   recorded steps.
//! - `payload_sha256` is the lowercase hex SHA-256 of the canonical JSON of the request
//!   alone. A repeat of an operation id with the same request is the same operation even
//!   when it arrives later (a relay retry); a different request under that id is
//!   `op_id_conflict`.
//! - `result_json` is the canonical JSON of the stored result once the operation is
//!   applied or rejected, and NULL while it is pending.
//! - `created_ms` and `applied_ms` are UTC milliseconds; `endpoint_id` is the device id.
//! - `alert_delivery.delivery_epoch` is a UUIDv4 string; `alert_scripts.sha256` is the
//!   lowercase hex SHA-256 of the canonical JSON of the script configuration written.
//!
//! A row whose `payload_json` is not in this shape (written by another controller) is
//! still read for the restore guard, which uses only the plain columns, and is otherwise
//! left untouched.

use std::fmt;

use echomuse_db::{ReadQuery, WriteCommand};
use echomuse_protocol::canonical::{canonical_json, canonical_sha256};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ids::OpId;

/// A journal failure; the transaction rolled back.
#[derive(Debug)]
pub enum JournalError {
    /// SQLite failed.
    Sql(rusqlite::Error),
    /// A value has no canonical JSON form (a NaN or infinite number).
    NotCanonical,
}

impl From<rusqlite::Error> for JournalError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(error) => write!(f, "alert journal: {error}"),
            Self::NotCanonical => f.write_str("alert journal: value has no canonical JSON"),
        }
    }
}

impl std::error::Error for JournalError {}

macro_rules! column_vocabulary {
    ($(#[$meta:meta])* pub enum $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum $name { $($(#[$vmeta])* $variant,)+ }

        impl $name {
            /// The column text.
            pub const fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text,)+ }
            }

            /// Parses the column text.
            pub fn from_column(text: &str) -> Option<Self> {
                match text { $($text => Some($name::$variant),)+ _ => None }
            }
        }
    };
}

column_vocabulary! {
    /// `alert_ops.action`.
    pub enum Action {
        /// Create a schedule.
        Create => "create",
        /// Edit a schedule.
        Update => "update",
        /// Handle one occurrence: dismissed.
        Dismiss => "dismiss",
        /// Handle one occurrence: snoozed into a child.
        Snooze => "snooze",
        /// Handle one occurrence: expired.
        Expire => "expire",
        /// Delete a schedule.
        Cancel => "cancel",
    }
}

impl Action {
    /// Whether an applied operation of this action is part of the restore guard.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Action::Dismiss | Action::Snooze | Action::Expire | Action::Cancel
        )
    }
}

column_vocabulary! {
    /// `alert_ops.source`.
    pub enum Source {
        /// A voice command.
        Voice => "voice",
        /// An LLM tool call.
        Llm => "llm",
        /// The device (its button, entity or executor).
        Device => "device",
        /// The dashboard.
        Dashboard => "dashboard",
        /// The alert service itself (missed occurrences).
        Engine => "engine",
    }
}

column_vocabulary! {
    /// `alert_ops.state`.
    pub enum State {
        /// Journaled, not yet confirmed.
        Pending => "pending",
        /// Confirmed on the calendar.
        Applied => "applied",
        /// Refused or never confirmed; nothing is stored for it.
        Rejected => "rejected",
    }
}

/// An operation to journal.
#[derive(Clone, Debug, PartialEq)]
pub struct NewOp {
    /// The operation id.
    pub op_id: OpId,
    /// The device id.
    pub endpoint_id: String,
    /// The speaker's calendar entity.
    pub calendar_entity: String,
    /// What it does.
    pub action: Action,
    /// The schedule it acts on.
    pub schedule_id: Uuid,
    /// The occurrence key; required for dismiss, snooze and expire.
    pub occurrence_key: Option<String>,
    /// Who asked.
    pub source: Source,
    /// The request as made; its canonical JSON is digested.
    pub request: Value,
    /// The writes it resolved to.
    pub plan: Value,
    /// UTC ms.
    pub created_ms: i64,
}

/// What `payload_json` holds.
#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// This controller's shape.
    Ours {
        /// The request.
        request: Value,
        /// The writes.
        plan: Value,
    },
    /// Another shape, kept as text.
    Foreign(String),
}

/// One `alert_ops` row.
#[derive(Clone, Debug, PartialEq)]
pub struct OpRow {
    /// The row's sequence, the order operations apply in.
    pub seq: i64,
    /// The operation id.
    pub op_id: OpId,
    /// The device id.
    pub endpoint_id: String,
    /// The calendar entity.
    pub calendar_entity: String,
    /// What it does; `None` for text this controller does not know.
    pub action: Option<Action>,
    /// The schedule it acts on; `None` when the column is not a UUID.
    pub schedule_id: Option<Uuid>,
    /// The occurrence key.
    pub occurrence_key: Option<String>,
    /// Who asked; `None` for text this controller does not know.
    pub source: Option<Source>,
    /// The payload.
    pub payload: Payload,
    /// The request digest.
    pub payload_sha256: String,
    /// The state; `None` for text this controller does not know.
    pub state: Option<State>,
    /// Progress through a multi-write operation.
    pub step: i64,
    /// The stored result; `None` while pending.
    pub result: Option<Value>,
    /// UTC ms.
    pub created_ms: i64,
    /// UTC ms of applying; `None` unless applied.
    pub applied_ms: Option<i64>,
}

impl OpRow {
    /// The request, for a row in this controller's shape.
    pub fn request(&self) -> Option<&Value> {
        match &self.payload {
            Payload::Ours { request, .. } => Some(request),
            Payload::Foreign(_) => None,
        }
    }

    /// The plan, for a row in this controller's shape.
    pub fn plan(&self) -> Option<&Value> {
        match &self.payload {
            Payload::Ours { plan, .. } => Some(plan),
            Payload::Foreign(_) => None,
        }
    }
}

/// The digest of a request: lowercase hex SHA-256 of its canonical JSON.
pub fn request_digest(request: &Value) -> Result<String, JournalError> {
    canonical_sha256(request)
        .map(|sha| sha.as_str().to_owned())
        .map_err(|_| JournalError::NotCanonical)
}

const ROW_COLUMNS: &str = "seq, op_id, endpoint_id, calendar_entity, action, schedule_id, \
    occurrence_key, source, payload_json, payload_sha256, state, step, result_json, \
    created_ms, applied_ms";

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OpRow> {
    let op_text: String = row.get(1)?;
    let payload_text: String = row.get(8)?;
    let payload = match serde_json::from_str::<Value>(&payload_text) {
        Ok(Value::Object(mut object))
            if object.len() == 2
                && object.contains_key("plan")
                && object.contains_key("request") =>
        {
            Payload::Ours {
                request: object.remove("request").unwrap_or(Value::Null),
                plan: object.remove("plan").unwrap_or(Value::Null),
            }
        }
        _ => Payload::Foreign(payload_text),
    };
    let action: String = row.get(4)?;
    let schedule: String = row.get(5)?;
    let source: String = row.get(7)?;
    let state: String = row.get(10)?;
    let result: Option<String> = row.get(12)?;
    Ok(OpRow {
        seq: row.get(0)?,
        // A non-UUID op_id cannot be one this controller wrote; the nil UUID marks it.
        op_id: OpId::from_journal(Uuid::parse_str(&op_text).unwrap_or(Uuid::nil())),
        endpoint_id: row.get(2)?,
        calendar_entity: row.get(3)?,
        action: Action::from_column(&action),
        schedule_id: Uuid::parse_str(&schedule).ok(),
        occurrence_key: row.get(6)?,
        source: Source::from_column(&source),
        payload,
        payload_sha256: row.get(9)?,
        state: State::from_column(&state),
        step: row.get(11)?,
        result: result.and_then(|text| serde_json::from_str(&text).ok()),
        created_ms: row.get(13)?,
        applied_ms: row.get(14)?,
    })
}

fn op_by_id(conn: &Connection, op_id: OpId) -> rusqlite::Result<Option<OpRow>> {
    conn.query_row(
        &format!("SELECT {ROW_COLUMNS} FROM alert_ops WHERE op_id = ?1"),
        params![op_id.to_string()],
        read_row,
    )
    .optional()
}

/// How journaling one operation went.
#[derive(Clone, Debug, PartialEq)]
pub enum Journaled {
    /// It is new and now pending.
    Inserted(OpRow),
    /// The same operation was journaled before; its row as it stands.
    Existing(OpRow),
    /// The id was journaled with a different request: `op_id_conflict`.
    Conflict(OpRow),
}

/// Journals operations in one transaction, in order. An id already present is not
/// written again; its row is returned with whether the request matches.
pub struct JournalOps(pub Vec<NewOp>);

impl WriteCommand for JournalOps {
    type Output = Vec<Journaled>;
    type Error = JournalError;

    fn apply(self, tx: &Transaction<'_>) -> Result<Self::Output, Self::Error> {
        let mut outcomes = Vec::with_capacity(self.0.len());
        for op in self.0 {
            let digest = request_digest(&op.request)?;
            if let Some(row) = op_by_id(tx, op.op_id)? {
                outcomes.push(if row.payload_sha256 == digest {
                    Journaled::Existing(row)
                } else {
                    Journaled::Conflict(row)
                });
                continue;
            }
            let payload = canonical_json(&json!({"plan": op.plan, "request": op.request}))
                .map_err(|_| JournalError::NotCanonical)?;
            tx.execute(
                "INSERT INTO alert_ops (op_id, endpoint_id, calendar_entity, action, schedule_id, \
                     occurrence_key, source, payload_json, payload_sha256, state, step, \
                     result_json, created_ms, applied_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', 0, NULL, ?10, NULL)",
                params![
                    op.op_id.to_string(),
                    op.endpoint_id,
                    op.calendar_entity,
                    op.action.as_str(),
                    op.schedule_id.to_string(),
                    op.occurrence_key,
                    op.source.as_str(),
                    payload,
                    digest,
                    op.created_ms,
                ],
            )?;
            let row = op_by_id(tx, op.op_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            outcomes.push(Journaled::Inserted(row));
        }
        Ok(outcomes)
    }
}

/// Records progress through a multi-write operation (a snooze is at step 1 once its child
/// exists). Only a pending row moves, and only forward.
pub struct AdvanceStep {
    /// The operation.
    pub op_id: OpId,
    /// The step reached.
    pub step: i64,
}

impl WriteCommand for AdvanceStep {
    type Output = ();
    type Error = JournalError;

    fn apply(self, tx: &Transaction<'_>) -> Result<(), JournalError> {
        tx.execute(
            "UPDATE alert_ops SET step = ?2 WHERE op_id = ?1 AND state = 'pending' AND step < ?2",
            params![self.op_id.to_string(), self.step],
        )?;
        Ok(())
    }
}

/// How a pending operation ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Settlement {
    /// Applied at UTC ms `at`, with its result.
    Applied {
        /// UTC ms.
        at: i64,
        /// The stored result.
        result: Value,
    },
    /// Rejected, with its result.
    Rejected {
        /// The stored result.
        result: Value,
    },
}

/// Ends a pending operation. A row that is no longer pending is left as it is; the
/// output is the row as it stands afterwards.
pub struct Settle {
    /// The operation.
    pub op_id: OpId,
    /// How it ended.
    pub settlement: Settlement,
}

impl WriteCommand for Settle {
    type Output = Option<OpRow>;
    type Error = JournalError;

    fn apply(self, tx: &Transaction<'_>) -> Result<Self::Output, JournalError> {
        let (state, at, result) = match self.settlement {
            Settlement::Applied { at, result } => (State::Applied, Some(at), result),
            Settlement::Rejected { result } => (State::Rejected, None, result),
        };
        let result = canonical_json(&result).map_err(|_| JournalError::NotCanonical)?;
        tx.execute(
            "UPDATE alert_ops SET state = ?2, applied_ms = ?3, result_json = ?4 \
             WHERE op_id = ?1 AND state = 'pending'",
            params![self.op_id.to_string(), state.as_str(), at, result],
        )?;
        Ok(op_by_id(tx, self.op_id)?)
    }
}

/// Every pending operation, in `seq` order.
pub struct PendingOps;

impl ReadQuery for PendingOps {
    type Output = Vec<OpRow>;
    type Error = JournalError;

    fn read(self, conn: &Connection) -> Result<Self::Output, JournalError> {
        let mut statement = conn.prepare(&format!(
            "SELECT {ROW_COLUMNS} FROM alert_ops WHERE state = 'pending' ORDER BY seq"
        ))?;
        let rows = statement
            .query_map([], read_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// The rows of these operation ids that exist.
pub struct OpsById(pub Vec<OpId>);

impl ReadQuery for OpsById {
    type Output = Vec<OpRow>;
    type Error = JournalError;

    fn read(self, conn: &Connection) -> Result<Self::Output, JournalError> {
        let mut rows = Vec::new();
        for id in self.0 {
            if let Some(row) = op_by_id(conn, id)? {
                rows.push(row);
            }
        }
        Ok(rows)
    }
}

/// One entry of the restore guard: an applied terminal operation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct GuardEntry {
    /// Dismiss, snooze, expire or cancel.
    pub action: Action,
    /// The schedule.
    pub schedule_id: Uuid,
    /// The occurrence key; `None` for a cancel.
    pub occurrence_key: Option<String>,
    /// The operation that applied.
    pub op_id: OpId,
}

/// The restore guard of one speaker: its applied terminal operations created at or
/// after `since_ms` (the guard keeps at least 90 days).
pub struct RestoreGuard {
    /// The device id.
    pub endpoint_id: String,
    /// UTC ms.
    pub since_ms: i64,
}

impl ReadQuery for RestoreGuard {
    type Output = Vec<GuardEntry>;
    type Error = JournalError;

    fn read(self, conn: &Connection) -> Result<Self::Output, JournalError> {
        let mut statement = conn.prepare(
            "SELECT action, schedule_id, occurrence_key, op_id FROM alert_ops \
             WHERE endpoint_id = ?1 AND state = 'applied' \
               AND action IN ('dismiss', 'snooze', 'expire', 'cancel') AND created_ms >= ?2 \
             ORDER BY seq",
        )?;
        let rows = statement.query_map(params![self.endpoint_id, self.since_ms], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut guard = Vec::new();
        for row in rows {
            let (action, schedule, key, op) = row?;
            let (Some(action), Ok(schedule_id), Ok(op)) = (
                Action::from_column(&action),
                Uuid::parse_str(&schedule),
                Uuid::parse_str(&op),
            ) else {
                continue;
            };
            guard.push(GuardEntry {
                action,
                schedule_id,
                occurrence_key: if action == Action::Cancel { None } else { key },
                op_id: OpId::from_journal(op),
            });
        }
        Ok(guard)
    }
}

// ── Delivery ─────────────────────────────────────────────────────────────────

/// One `alert_delivery` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryRow {
    /// The device id.
    pub endpoint_id: String,
    /// The Local Calendar's config entry.
    pub calendar_entry_id: String,
    /// The calendar entity.
    pub calendar_entity: String,
    /// The delivery epoch.
    pub delivery_epoch: String,
    /// The highest delivery sequence assigned.
    pub sequence: u64,
    /// The highest sequence the device durably acknowledged.
    pub acked_sequence: u64,
}

fn read_delivery(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeliveryRow> {
    Ok(DeliveryRow {
        endpoint_id: row.get(0)?,
        calendar_entry_id: row.get(1)?,
        calendar_entity: row.get(2)?,
        delivery_epoch: row.get(3)?,
        sequence: row.get::<_, i64>(4)?.max(0) as u64,
        acked_sequence: row.get::<_, i64>(5)?.max(0) as u64,
    })
}

const DELIVERY_COLUMNS: &str =
    "endpoint_id, calendar_entry_id, calendar_entity, delivery_epoch, sequence, acked_sequence";

/// The delivery row of one speaker.
pub struct LoadDelivery(pub String);

impl ReadQuery for LoadDelivery {
    type Output = Option<DeliveryRow>;
    type Error = JournalError;

    fn read(self, conn: &Connection) -> Result<Self::Output, JournalError> {
        Ok(conn
            .query_row(
                &format!("SELECT {DELIVERY_COLUMNS} FROM alert_delivery WHERE endpoint_id = ?1"),
                params![self.0],
                read_delivery,
            )
            .optional()?)
    }
}

/// Records which calendar a speaker uses. A new row starts delivery epoch `new_epoch` at
/// sequence 0; an existing row keeps its epoch and sequence.
pub struct BindCalendar {
    /// The device id.
    pub endpoint_id: String,
    /// The config entry.
    pub calendar_entry_id: String,
    /// The entity.
    pub calendar_entity: String,
    /// The epoch for a new row.
    pub new_epoch: String,
}

impl WriteCommand for BindCalendar {
    type Output = DeliveryRow;
    type Error = JournalError;

    fn apply(self, tx: &Transaction<'_>) -> Result<DeliveryRow, JournalError> {
        tx.execute(
            "INSERT INTO alert_delivery (endpoint_id, calendar_entry_id, calendar_entity, \
                 delivery_epoch, sequence, acked_sequence) VALUES (?1, ?2, ?3, ?4, 0, 0) \
             ON CONFLICT (endpoint_id) DO UPDATE SET calendar_entry_id = excluded.calendar_entry_id, \
                 calendar_entity = excluded.calendar_entity",
            params![
                self.endpoint_id,
                self.calendar_entry_id,
                self.calendar_entity,
                self.new_epoch
            ],
        )?;
        Ok(tx.query_row(
            &format!("SELECT {DELIVERY_COLUMNS} FROM alert_delivery WHERE endpoint_id = ?1"),
            params![self.endpoint_id],
            read_delivery,
        )?)
    }
}

/// Moves a speaker's delivery position: the assigned sequence and the acknowledged one
/// only rise, and the acknowledged one never passes the assigned one.
pub struct SetDeliveryPosition {
    /// The device id.
    pub endpoint_id: String,
    /// The highest sequence assigned.
    pub sequence: u64,
    /// The highest sequence acknowledged.
    pub acked_sequence: u64,
}

impl WriteCommand for SetDeliveryPosition {
    type Output = ();
    type Error = JournalError;

    fn apply(self, tx: &Transaction<'_>) -> Result<(), JournalError> {
        let sequence = i64::try_from(self.sequence).unwrap_or(i64::MAX);
        let acked = i64::try_from(self.acked_sequence).unwrap_or(i64::MAX);
        tx.execute(
            "UPDATE alert_delivery SET sequence = MAX(sequence, ?2), \
                 acked_sequence = MIN(MAX(acked_sequence, ?3), MAX(sequence, ?2)) \
             WHERE endpoint_id = ?1",
            params![self.endpoint_id, sequence, acked],
        )?;
        Ok(())
    }
}

// ── Scripts ──────────────────────────────────────────────────────────────────

/// One `alert_scripts` row: what the controller last wrote for a script.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptRecord {
    /// The script's object id.
    pub object_id: String,
    /// The digest of the configuration written.
    pub sha256: String,
    /// Its `echomuse_revision`.
    pub revision: i64,
}

/// Every installed-script record.
pub struct LoadScripts;

impl ReadQuery for LoadScripts {
    type Output = Vec<ScriptRecord>;
    type Error = JournalError;

    fn read(self, conn: &Connection) -> Result<Self::Output, JournalError> {
        let mut statement =
            conn.prepare("SELECT object_id, sha256, revision FROM alert_scripts")?;
        let rows = statement
            .query_map([], |row| {
                Ok(ScriptRecord {
                    object_id: row.get(0)?,
                    sha256: row.get(1)?,
                    revision: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// Records what was written for a script.
pub struct RecordScript(pub ScriptRecord);

impl WriteCommand for RecordScript {
    type Output = ();
    type Error = JournalError;

    fn apply(self, tx: &Transaction<'_>) -> Result<(), JournalError> {
        tx.execute(
            "INSERT INTO alert_scripts (object_id, sha256, revision) VALUES (?1, ?2, ?3) \
             ON CONFLICT (object_id) DO UPDATE SET sha256 = excluded.sha256, \
                 revision = excluded.revision",
            params![self.0.object_id, self.0.sha256, self.0.revision],
        )?;
        Ok(())
    }
}
