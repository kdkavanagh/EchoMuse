//! A copied installation under a running controller (docs/rust-controller-evaluation.md
//! §3.5, §5.6 "Database and file preservation"). `tools/data-copy-check.sh` runs this test
//! inside an isolated network namespace, next to the controller it starts on a private copy
//! of a real data directory; it is `#[ignore]`d and does nothing without that script.
//!
//! `ECHOMUSE_DCC_MODE` picks what it does:
//!
//! - `drive`: takes a snapshot of every table of the untouched copy, starts the simulated
//!   Home Assistant and publishes its URL, then waits for the controller the script starts.
//!   It checks what the dashboard API reads from the real rows and files (devices, turns,
//!   near misses, traces, every recording byte for byte, archives, wake models, sounds,
//!   alerts, settings), then connects one simulated speaker with a copied device's id and
//!   token and plays it a real wake recording from the copy as a wake turn. Once the
//!   script has stopped the controller, every row the controller added or changed is
//!   compared with the stored rows of its table column by column: storage type, NULLs,
//!   JSON shape and numeric range. Rows deleted at startup or later are counted.
//! - `serve`: only the simulated Home Assistant, until the script writes `stop_ha`: for a
//!   restart of the controller and the rollback run of the previous image.
//!
//! The settings:
//!
//! - `ECHOMUSE_DCC_DATA`: the copied data directory (the database is `echomuse.db` in it);
//! - `ECHOMUSE_DCC_OUT`: where this test and the script leave each other files:
//!   `ha_token` and `ha_url` (written here, the simulated Home Assistant's), `driven`
//!   (here), `stopped` (by the script), `stop_ha` (by the script), `drive.json` and
//!   `report-drive.txt` (here);
//! - `ECHOMUSE_DCC_LOG`: the controller's log, which the script writes (`drive` only);
//! - `ECHOMUSE_DCC_SESSION`: a dashboard session the script inserted for the copy's
//!   administrator (`drive` only);
//! - `ECHOMUSE_DCC_API`, `ECHOMUSE_DCC_DEVICE_ADDR`: the controller's dashboard and device
//!   addresses, `127.0.0.1:8768` and `127.0.0.1:8767` when unset;
//! - `ECHOMUSE_DCC_TURN` (optional): `<device id>/<turn id>`, the stored wake turn whose
//!   utterance recording is played. Unset, the newest completed wake turn of an approved
//!   Echo Dot with a token, a transcript, a kept utterance and a decision trace is used
//!   (a Dot's `os_version` is `Fire OS 6.x.y.z`; the Echo Show's is `Fire OS 65xx.y`).
//!
//! Nothing secret is printed: device tokens, ESPHome keys, kiosk tokens, password hashes
//! and session tokens are reported only as set, unset or changed.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{OpenFlags, types::Value as Sql};

use super::*;

/// The copied device the simulated speaker dials as, and its stored token.
static REAL_TOKEN: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

/// The stored token of the copied device the speaker dials as; `None` for every other
/// device, which keeps its synthetic token.
pub(super) fn real_token(device: &str) -> Option<String> {
    REAL_TOKEN
        .get()
        .filter(|(id, _)| id == device)
        .map(|(_, token)| token.clone())
}

/// The schema contract's tables (crates/db/schema/v40.sql).
const TABLES: [&str; 14] = [
    "devices",
    "users",
    "sessions",
    "system_config",
    "turns",
    "near_misses",
    "device_logs",
    "device_metrics",
    "wake_counters",
    "wake_shadow",
    "wake_shadow_events",
    "alert_ops",
    "alert_delivery",
    "alert_scripts",
];

/// Columns whose values are never printed.
const SECRET: [(&str, &str); 5] = [
    ("devices", "token"),
    ("devices", "esphome_noise_psk"),
    ("devices", "kiosk_token"),
    ("users", "password_hash"),
    ("sessions", "token"),
];

/// How many of a table's newest stored rows a written row is compared with.
const REFERENCE_ROWS: usize = 300;
/// How many of the newest reference rows decide whether a column is still being written:
/// a NULL is reported when nine in ten of them have a value.
const RECENT_ROWS: usize = 50;

/// How long the controller may take to qualify and load the speech models.
const READY_WAIT: Duration = Duration::from_secs(400);

fn dcc_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is not set; run tools/data-copy-check.sh"))
}

fn dcc_env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn is_secret(table: &str, column: &str) -> bool {
    SECRET.contains(&(table, column))
}

// ── Reporting ────────────────────────────────────────────────────────────────────────

/// What the run found: every line is printed and kept for `report-drive.txt`; defects fail
/// the test at the end, after everything has been looked at.
#[derive(Default)]
struct Report {
    lines: Vec<String>,
    defects: Vec<String>,
}

impl Report {
    fn line(&mut self, text: impl Into<String>) {
        let text = text.into();
        eprintln!("{text}");
        self.lines.push(text);
    }

    fn defect(&mut self, text: impl Into<String>) {
        let text = format!("DEFECT: {}", text.into());
        eprintln!("{text}");
        self.lines.push(text.clone());
        self.defects.push(text);
    }

    fn check(&mut self, ok: bool, text: impl Into<String>) {
        if ok {
            self.line(format!("ok: {}", text.into()));
        } else {
            self.defect(text);
        }
    }
}

// ── Snapshots ────────────────────────────────────────────────────────────────────────

/// One stored row: its rowid (for "newest") and its values by column.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    rowid: i64,
    values: BTreeMap<String, Sql>,
}

impl Row {
    fn get(&self, column: &str) -> &Sql {
        self.values.get(column).unwrap_or(&Sql::Null)
    }

    fn text(&self, column: &str) -> Option<&str> {
        match self.get(column) {
            Sql::Text(text) => Some(text),
            _ => None,
        }
    }

    fn int(&self, column: &str) -> Option<i64> {
        match self.get(column) {
            Sql::Integer(n) => Some(*n),
            _ => None,
        }
    }
}

/// A table's rows by primary key (by rowid where it has none).
#[derive(Clone, Default)]
struct Table {
    columns: Vec<String>,
    rows: BTreeMap<String, Row>,
}

#[derive(Clone, Default)]
struct Snapshot {
    tables: BTreeMap<&'static str, Table>,
}

fn key_text(value: &Sql) -> String {
    match value {
        Sql::Null => "NULL".to_owned(),
        Sql::Integer(n) => format!("{n:020}"),
        Sql::Real(x) => x.to_string(),
        Sql::Text(text) => text.clone(),
        Sql::Blob(bytes) => format!("blob:{}", bytes.len()),
    }
}

fn open_read_only(db: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap_or_else(|e| panic!("{}: {e}", db.display()));
    conn.busy_timeout(Duration::from_secs(10)).unwrap();
    conn
}

fn snapshot(db: &Path) -> Snapshot {
    let conn = open_read_only(db);
    let mut snapshot = Snapshot::default();
    for table in TABLES {
        let mut info = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let mut keyed: Vec<(i64, String)> = Vec::new();
        let columns: Vec<String> = info
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .map(|(name, pk)| {
                if pk > 0 {
                    keyed.push((pk, name.clone()));
                }
                name
            })
            .collect();
        keyed.sort();
        let key: Vec<String> = keyed.into_iter().map(|(_, name)| name).collect();
        let mut statement = conn
            .prepare(&format!("SELECT rowid, * FROM {table}"))
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                let rowid: i64 = row.get(0)?;
                let mut values = BTreeMap::new();
                for (n, column) in columns.iter().enumerate() {
                    values.insert(column.clone(), row.get::<_, Sql>(n + 1)?);
                }
                Ok(Row { rowid, values })
            })
            .unwrap()
            .map(Result::unwrap)
            .map(|row| {
                let id = if key.is_empty() {
                    key_text(&Sql::Integer(row.rowid))
                } else {
                    key.iter()
                        .map(|k| key_text(row.get(k)))
                        .collect::<Vec<_>>()
                        .join("\u{1f}")
                };
                (id, row)
            })
            .collect();
        snapshot.tables.insert(table, Table { columns, rows });
    }
    snapshot
}

fn counts(snapshot: &Snapshot) -> BTreeMap<&'static str, usize> {
    snapshot
        .tables
        .iter()
        .map(|(table, rows)| (*table, rows.rows.len()))
        .collect()
}

/// Rows of `before` that `after` no longer has, by table.
fn removed(before: &Snapshot, after: &Snapshot) -> BTreeMap<&'static str, usize> {
    before
        .tables
        .iter()
        .map(|(table, rows)| {
            let later = &after.tables[table].rows;
            (
                *table,
                rows.rows.keys().filter(|k| !later.contains_key(*k)).count(),
            )
        })
        .filter(|(_, n)| *n > 0)
        .collect()
}

fn report_counts(report: &mut Report, title: &str, before: &Snapshot, after: &Snapshot) {
    report.line(format!("── row counts {title} ──"));
    let a = counts(before);
    let b = counts(after);
    for table in TABLES {
        report.line(format!("  {table:<20} {:>7} → {:>7}", a[table], b[table]));
    }
    let gone = removed(before, after);
    if gone.is_empty() {
        report.line("  no stored row was removed");
    }
    for (table, n) in gone {
        report.defect(format!("{n} stored rows of {table} were removed {title}"));
    }
}

// ── Shapes ───────────────────────────────────────────────────────────────────────────

fn storage(value: &Sql) -> &'static str {
    match value {
        Sql::Null => "null",
        Sql::Integer(_) => "integer",
        Sql::Real(_) => "real",
        Sql::Text(_) => "text",
        Sql::Blob(_) => "blob",
    }
}

/// A text value's JSON, when it is a JSON object or array.
fn as_json(value: &Sql) -> Option<Value> {
    match value {
        Sql::Text(text) if text.starts_with('{') || text.starts_with('[') => {
            serde_json::from_str(text).ok()
        }
        _ => None,
    }
}

/// Every path of a JSON value with the type of what it holds (`.a.b[]:int`).
fn json_paths(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                out.insert(format!("{prefix}:{{}}"));
            }
            for (key, item) in map {
                json_paths(item, &format!("{prefix}.{key}"), out);
            }
        }
        Value::Array(items) => {
            if items.is_empty() {
                out.insert(format!("{prefix}:[]"));
            }
            for item in items {
                json_paths(item, &format!("{prefix}[]"), out);
            }
        }
        Value::Null => {
            out.insert(format!("{prefix}:null"));
        }
        Value::Bool(_) => {
            out.insert(format!("{prefix}:bool"));
        }
        Value::Number(n) => {
            let kind = if n.is_i64() || n.is_u64() {
                "int"
            } else {
                "float"
            };
            out.insert(format!("{prefix}:{kind}"));
        }
        Value::String(_) => {
            out.insert(format!("{prefix}:string"));
        }
    }
}

/// What a column holds across reference rows.
#[derive(Default)]
struct Profile {
    rows: usize,
    storage: BTreeMap<&'static str, usize>,
    text: usize,
    json: usize,
    /// Paths found in any reference row's JSON.
    paths: BTreeSet<String>,
    /// Paths every reference row's JSON has.
    common: Option<BTreeSet<String>>,
    min: Option<f64>,
    max: Option<f64>,
    /// Of the newest [`RECENT_ROWS`] reference rows: how many, and how many hold a value.
    recent: usize,
    recent_values: usize,
}

impl Profile {
    fn of(rows: &[&Row], column: &str) -> Self {
        let mut profile = Self::default();
        for row in rows {
            let value = row.get(column);
            profile.rows += 1;
            if profile.recent < RECENT_ROWS {
                profile.recent += 1;
                if !matches!(value, Sql::Null) {
                    profile.recent_values += 1;
                }
            }
            *profile.storage.entry(storage(value)).or_default() += 1;
            let number = match value {
                Sql::Integer(n) => Some(*n as f64),
                Sql::Real(x) => Some(*x),
                _ => None,
            };
            if let Some(x) = number {
                profile.min = Some(profile.min.map_or(x, |m| m.min(x)));
                profile.max = Some(profile.max.map_or(x, |m| m.max(x)));
            }
            if matches!(value, Sql::Text(_)) {
                profile.text += 1;
            }
            if let Some(json) = as_json(value) {
                profile.json += 1;
                let mut paths = BTreeSet::new();
                json_paths(&json, "", &mut paths);
                profile.paths.extend(paths.iter().cloned());
                profile.common = Some(match profile.common.take() {
                    None => paths,
                    Some(common) => common.intersection(&paths).cloned().collect(),
                });
            }
        }
        profile
    }
}

/// A value for the report: secrets withheld, long text cut.
fn shown(table: &str, column: &str, value: &Sql) -> String {
    if is_secret(table, column) {
        return match value {
            Sql::Null => "NULL".to_owned(),
            _ => "(set)".to_owned(),
        };
    }
    match value {
        Sql::Null => "NULL".to_owned(),
        Sql::Integer(n) => n.to_string(),
        Sql::Real(x) => format!("{x}"),
        Sql::Text(text) if as_json(value).is_some() => format!("JSON, {} chars", text.len()),
        Sql::Text(text) if text.chars().count() > 80 => {
            format!("{:?}…", text.chars().take(80).collect::<String>())
        }
        Sql::Text(text) => format!("{text:?}"),
        Sql::Blob(bytes) => format!("blob of {} bytes", bytes.len()),
    }
}

/// The stored rows a written row of `table` is compared with: the newest of the table's
/// rows, for turns those with the same trigger and terminal reason.
fn references<'a>(table: &str, stored: &'a Table, row: &Row) -> Vec<&'a Row> {
    let mut rows: Vec<&Row> = stored
        .rows
        .values()
        .filter(|r| {
            table != "turns"
                || (r.get("trigger_type") == row.get("trigger_type")
                    && r.get("terminal_reason") == row.get("terminal_reason"))
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.rowid));
    rows.truncate(REFERENCE_ROWS);
    rows
}

/// Compares one written value with the column's stored values.
fn compare_value(
    report: &mut Report,
    table: &str,
    column: &str,
    value: &Sql,
    profile: &Profile,
) -> bool {
    let place = format!("{table}.{column}");
    let mut clean = true;
    let kind = storage(value);
    let stored_kinds: Vec<&str> = profile.storage.keys().copied().collect();
    let non_null_kinds: Vec<&str> = stored_kinds
        .iter()
        .copied()
        .filter(|k| *k != "null")
        .collect();
    if profile.rows == 0 {
        report.line(format!(
            "  note: {place} = {}: no stored row to compare with",
            shown(table, column, value)
        ));
        return true;
    }
    if kind == "null" {
        if !profile.storage.contains_key("null") {
            report.line(format!(
                "  note: {place} is NULL; each of {} stored rows has a value ({})",
                profile.rows,
                non_null_kinds.join("/")
            ));
            clean = false;
        } else if profile.recent_values * 10 >= profile.recent * 9 {
            report.line(format!(
                "  note: {place} is NULL; {} of the newest {} stored rows have a value ({})",
                profile.recent_values,
                profile.recent,
                non_null_kinds.join("/")
            ));
            clean = false;
        }
        return clean;
    }
    if non_null_kinds.is_empty() {
        report.line(format!(
            "  note: {place} = {}; it is NULL in all {} stored rows",
            shown(table, column, value),
            profile.rows
        ));
        return false;
    }
    if !non_null_kinds.contains(&kind) {
        report.defect(format!(
            "{place} is stored as {kind} ({}); stored rows hold {}",
            shown(table, column, value),
            stored_kinds.join("/")
        ));
        return false;
    }
    let json = as_json(value);
    if profile.json > 0 && profile.json == profile.text && json.is_none() {
        report.defect(format!(
            "{place} is not JSON ({}); every stored value is",
            shown(table, column, value)
        ));
        return false;
    }
    if profile.json == 0 && profile.text > 0 && json.is_some() {
        report.defect(format!(
            "{place} is JSON; no stored value is ({})",
            shown(table, column, value)
        ));
        return false;
    }
    if let Some(json) = json {
        let mut paths = BTreeSet::new();
        json_paths(&json, "", &mut paths);
        let unknown: Vec<&String> = paths.difference(&profile.paths).collect();
        if !unknown.is_empty() {
            report.line(format!(
                "  note: {place}: {} JSON paths no stored row has: {}",
                unknown.len(),
                abbreviate(&unknown)
            ));
            clean = false;
        }
        if let Some(common) = &profile.common {
            let missing: Vec<&String> = common.difference(&paths).collect();
            if !missing.is_empty() {
                report.line(format!(
                    "  note: {place}: lacks {} JSON paths every stored row has: {}",
                    missing.len(),
                    abbreviate(&missing)
                ));
                clean = false;
            }
        }
    }
    let number = match value {
        Sql::Integer(n) => Some(*n as f64),
        Sql::Real(x) => Some(*x),
        _ => None,
    };
    if let (Some(x), Some(min), Some(max)) = (number, profile.min, profile.max)
        && (x < min || x > max)
    {
        report.line(format!(
            "  note: {place} = {x} is outside the stored range {min} to {max}"
        ));
        clean = false;
    }
    clean
}

fn abbreviate(paths: &[&String]) -> String {
    let shown: Vec<&str> = paths.iter().take(12).map(|p| p.as_str()).collect();
    if paths.len() > shown.len() {
        format!("{} …", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

/// Compares every row the controller added or changed with the stored rows.
fn compare_written(report: &mut Report, before: &Snapshot, after: &Snapshot) {
    report.line("── rows the controller wrote, column by column ──");
    for table in TABLES {
        let stored = &before.tables[table];
        let now = &after.tables[table];
        let added: Vec<(&String, &Row)> = now
            .rows
            .iter()
            .filter(|(k, _)| !stored.rows.contains_key(*k))
            .collect();
        let changed: Vec<(&String, &Row, &Row)> = now
            .rows
            .iter()
            .filter_map(|(k, row)| {
                let old = stored.rows.get(k)?;
                (old.values != row.values).then_some((k, old, row))
            })
            .collect();
        if added.is_empty() && changed.is_empty() {
            continue;
        }
        report.line(format!(
            "{table}: {} added, {} changed",
            added.len(),
            changed.len()
        ));
        for (key, row) in &added {
            let shown_key = if table == "sessions" {
                "(session)"
            } else {
                key.as_str()
            };
            report.line(format!("  added {shown_key} (rowid {})", row.rowid));
            let refs = references(table, stored, row);
            let mut clean = 0;
            for column in &now.columns {
                let value = row.get(column);
                let profile = Profile::of(&refs, column);
                report.line(format!(
                    "    {column} = {} [{}; stored: {}]",
                    shown(table, column, value),
                    storage(value),
                    profile
                        .storage
                        .iter()
                        .map(|(k, n)| format!("{k} {n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                if compare_value(report, table, column, value, &profile) {
                    clean += 1;
                }
            }
            report.line(format!(
                "    {clean} of {} columns match the stored rows' shapes",
                now.columns.len()
            ));
        }
        for (key, old, row) in &changed {
            let shown_key = if table == "sessions" {
                "(session)"
            } else {
                key.as_str()
            };
            report.line(format!("  changed {shown_key}"));
            let refs = references(table, stored, row);
            for column in &now.columns {
                let (was, is) = (old.get(column), row.get(column));
                if was == is {
                    continue;
                }
                report.line(format!(
                    "    {column}: {} → {}",
                    shown(table, column, was),
                    shown(table, column, is)
                ));
                let profile = Profile::of(&refs, column);
                compare_value(report, table, column, is, &profile);
            }
        }
    }
}

// ── The dashboard API ────────────────────────────────────────────────────────────────

struct Api {
    client: reqwest::Client,
    base: String,
    session: String,
}

impl Api {
    fn new(address: &str, session: String) -> Self {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap();
        Self {
            client,
            base: format!("http://{address}"),
            session,
        }
    }

    async fn get(&self, path: &str) -> (u16, Bytes) {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.session)
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {path}: {e}"));
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .unwrap_or_else(|e| panic!("GET {path}: {e}"));
        (status, body)
    }

    async fn json(&self, report: &mut Report, path: &str) -> Option<Value> {
        let (status, body) = self.get(path).await;
        if status != 200 {
            report.defect(format!(
                "GET {path} answered {status}: {}",
                String::from_utf8_lossy(&body[..body.len().min(300)])
            ));
            return None;
        }
        match serde_json::from_slice(&body) {
            Ok(value) => Some(value),
            Err(e) => {
                report.defect(format!("GET {path} is not JSON: {e}"));
                None
            }
        }
    }
}

/// Tallies of one recording route over every row.
#[derive(Default)]
struct Served {
    identical: usize,
    absent: usize,
    wrong: Vec<String>,
}

impl Served {
    /// One answer: `expected` is the file the row says is kept, if it is on disk.
    fn tally(&mut self, path: &str, status: u16, body: &[u8], expected: Option<Vec<u8>>) {
        match (status, expected) {
            (200, Some(bytes)) if bytes == body => self.identical += 1,
            (404, None) => self.absent += 1,
            (status, expected) => self.wrong.push(format!(
                "{path}: {status} with {} bytes; the file {}",
                body.len(),
                match expected {
                    Some(bytes) => format!("has {} bytes", bytes.len()),
                    None => "is not kept".to_owned(),
                }
            )),
        }
    }

    fn report(&self, report: &mut Report, what: &str) {
        report.line(format!(
            "  {what}: {} served byte for byte, {} answered 404 (not kept)",
            self.identical, self.absent
        ));
        for wrong in self.wrong.iter().take(10) {
            report.defect(format!("{what}: {wrong}"));
        }
        if self.wrong.len() > 10 {
            report.defect(format!(
                "{what}: {} more wrong answers",
                self.wrong.len() - 10
            ));
        }
    }
}

fn read_kept(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

/// The `.wav` files under `dir`, at any depth.
fn wav_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| {
                    let path = entry.path();
                    if path.is_dir() {
                        wav_files(&path)
                    } else {
                        usize::from(path.extension().is_some_and(|e| e == "wav"))
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

/// The number of entries a ZIP archive's end record names.
fn zip_entries(bytes: &[u8]) -> Option<u16> {
    let end = bytes.windows(4).rposition(|w| w == b"PK\x05\x06")?;
    let field = bytes.get(end + 10..end + 12)?;
    Some(u16::from_le_bytes([field[0], field[1]]))
}

async fn check_api(report: &mut Report, api: &Api, data: &Path, stored: &Snapshot) {
    report.line("── the dashboard API over the stored rows and files ──");
    let devices = &stored.tables["devices"];
    let turns = &stored.tables["turns"];
    let near_misses = &stored.tables["near_misses"];

    if let Some(me) = api.json(report, "/api/auth/me").await {
        report.check(
            me["role"] == "admin",
            format!("the inserted session signs in as role {}", me["role"]),
        );
    }
    if let Some(list) = api.json(report, "/api/devices").await {
        let listed: BTreeSet<String> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["device_id"].as_str().map(str::to_owned))
            .collect();
        let rows: BTreeSet<String> = devices.rows.keys().cloned().collect();
        report.check(
            listed == rows,
            format!(
                "/api/devices lists the {} stored devices ({} listed)",
                rows.len(),
                listed.len()
            ),
        );
        for device in list.as_array().into_iter().flatten() {
            report.line(format!(
                "  {} {:?}: approved {}, firmware {}, os {}, esphome_port {}, config_sections {}",
                device["device_id"].as_str().unwrap_or("?"),
                device["label"].as_str().unwrap_or(""),
                device["approved"],
                device["firmware_ver"],
                device["os_version"],
                device["esphome_port"],
                device["config_sections"]
            ));
        }
    }
    for path in [
        "/api/global/config",
        "/api/system/config",
        "/api/system/status",
        "/api/ha/status",
        "/api/firmware",
        "/api/clip_windows",
    ] {
        if api.json(report, path).await.is_some() {
            report.line(format!("ok: GET {path}"));
        }
    }
    for path in ["/", "/dashboard"] {
        let (status, body) = api.get(path).await;
        report.check(
            status == 200 && !body.is_empty(),
            format!("GET {path} answered {status} with {} bytes", body.len()),
        );
    }

    // Each device's own routes.
    for (device, row) in &devices.rows {
        for suffix in [
            "",
            "/config",
            "/logs",
            "/alerts",
            "/wake_shadow",
            "/response_latency",
        ] {
            api.json(report, &format!("/api/devices/{device}{suffix}"))
                .await;
        }
        let stored_turns = turns
            .rows
            .values()
            .filter(|t| t.text("device_id") == Some(device))
            .count();
        let stored_near = near_misses
            .rows
            .values()
            .filter(|t| t.text("device_id") == Some(device))
            .count();
        let listed_turns = api
            .json(report, &format!("/api/devices/{device}/turns?limit=1000"))
            .await
            .and_then(|v| v.as_array().map(Vec::len));
        let listed_near = api
            .json(
                report,
                &format!("/api/devices/{device}/near_misses?limit=1000"),
            )
            .await
            .and_then(|v| v.as_array().map(Vec::len));
        report.check(
            listed_turns == Some(stored_turns.min(1000))
                && listed_near == Some(stored_near.min(1000)),
            format!(
                "{device} ({}): {stored_turns} turns and {stored_near} near misses stored, \
                 {listed_turns:?} and {listed_near:?} listed",
                shown("devices", "label", row.get("label"))
            ),
        );
        let (status, body) = api
            .get(&format!("/api/devices/{device}/wakeclips.zip"))
            .await;
        report.line(format!(
            "  {device}/wakeclips.zip: {status}, {} bytes, {:?} entries; {} WAV files in wakes/{device}",
            body.len(),
            zip_entries(&body),
            wav_files(&data.join("wakes").join(device))
        ));
    }

    // Every device's lists, and every turn object's JSON columns arriving parsed.
    if let Some(list) = api.json(report, "/api/turns?limit=1000").await {
        let list = list.as_array().cloned().unwrap_or_default();
        report.check(
            list.len() == turns.rows.len().min(1000),
            format!(
                "/api/turns lists {} of {} stored turns",
                list.len(),
                turns.rows.len()
            ),
        );
        let unparsed = list
            .iter()
            .filter(|t| {
                ["afe_evidence", "stt_fix", "wake_windows"]
                    .iter()
                    .any(|k| t[*k].is_string())
            })
            .count();
        report.check(
            unparsed == 0,
            format!("{unparsed} turn objects carry a JSON column as unparsed text"),
        );
    }
    if let Some(list) = api.json(report, "/api/near_misses?limit=1000").await {
        let list = list.as_array().cloned().unwrap_or_default();
        report.check(
            list.len() == near_misses.rows.len().min(1000),
            format!(
                "/api/near_misses lists {} of {} stored near misses",
                list.len(),
                near_misses.rows.len()
            ),
        );
        let mut words: BTreeMap<String, usize> = BTreeMap::new();
        for near in &list {
            *words.entry(near["recording"].to_string()).or_default() += 1;
        }
        report.line(format!("  near-miss recording words: {words:?}"));
    }

    // Every turn's trace and recordings, every near miss's recording.
    let mut traces = Served::default();
    let mut wakes = Served::default();
    let mut utterances = Served::default();
    let mut recordings = Served::default();
    let mut server_errors = Vec::new();
    for turn in turns.rows.values() {
        let device = turn.text("device_id").unwrap_or_default();
        let id = turn.int("id").unwrap_or_default();
        let base = format!("/api/devices/{device}/turns/{id}");
        let (status, body) = api.get(&format!("{base}/trace")).await;
        let stored_trace = as_json(turn.get("decision_trace"));
        let same = serde_json::from_slice::<Value>(&body).ok() == stored_trace;
        match (status, stored_trace.is_some()) {
            (200, true) if same => traces.identical += 1,
            (404, false) => traces.absent += 1,
            _ => traces.wrong.push(format!("{base}/trace: {status}")),
        }
        let (status, _) = api.get(&format!("{base}/afe")).await;
        if status >= 500 {
            server_errors.push(format!("{base}/afe: {status}"));
        }
        let wake_file = turn
            .text("wake_file")
            .and_then(|_| read_kept(&data.join("wakes").join(device).join(format!("{id}.wav"))));
        let (status, body) = api.get(&format!("{base}/wake")).await;
        wakes.tally(&format!("{base}/wake"), status, &body, wake_file);
        let audio_file = turn
            .text("audio_file")
            .and_then(|_| read_kept(&data.join("recordings").join(format!("{device}_{id}.wav"))));
        let (status, body) = api.get(&format!("{base}/audio")).await;
        utterances.tally(&format!("{base}/audio"), status, &body, audio_file);
        let recording = read_kept(
            &data
                .join("turn_recordings")
                .join(format!("{device}_{id}.wav")),
        );
        let (status, body) = api.get(&format!("{base}/recording")).await;
        recordings.tally(&format!("{base}/recording"), status, &body, recording);
    }
    let mut near_recordings = Served::default();
    for near in near_misses.rows.values() {
        let device = near.text("device_id").unwrap_or_default();
        let id = near.int("id").unwrap_or_default();
        let path = format!("/api/devices/{device}/near_misses/{id}/recording");
        let kept = (near.text("recording") == Some("saved"))
            .then(|| {
                read_kept(
                    &data
                        .join("nearmiss_recordings")
                        .join(format!("{device}_{id}.wav")),
                )
            })
            .flatten();
        let (status, body) = api.get(&path).await;
        near_recordings.tally(&path, status, &body, kept);
        let (status, _) = api
            .get(&format!("/api/devices/{device}/near_misses/{id}/afe"))
            .await;
        if status >= 500 {
            server_errors.push(format!("near miss {id} afe: {status}"));
        }
    }
    report.line("recordings and traces of every stored row:");
    traces.report(report, "decision traces");
    wakes.report(report, "wake clips");
    utterances.report(report, "utterances");
    recordings.report(report, "turn recordings");
    near_recordings.report(report, "near-miss recordings");
    report.check(
        server_errors.is_empty(),
        format!("AFE series of every row answer without a server error {server_errors:?}"),
    );
    for (path, folder) in [
        ("/api/near_misses.zip?min=0&max=1", "nearmiss_recordings"),
        ("/api/wakeclips.zip?min=0&max=1", "wakes"),
    ] {
        let (status, body) = api.get(path).await;
        report.line(format!(
            "  {path}: {status}, {} bytes, {:?} entries ({folder}/ holds {} WAV files)",
            body.len(),
            zip_entries(&body),
            wav_files(&data.join(folder))
        ));
    }

    // The wake-model registry and the sounds, against their files.
    if let Some(models) = api.json(report, "/api/wake_models").await {
        let active = models["active"].as_str().unwrap_or_default().to_owned();
        let listed = models["models"].as_array().map(Vec::len).unwrap_or(0);
        report.check(
            !active.is_empty()
                && data
                    .join("oww_models")
                    .join(format!("{active}.onnx"))
                    .is_file(),
            format!(
                "/api/wake_models: {listed} models, the active graph {active} is in oww_models/"
            ),
        );
    }
    if let Some(sounds) = api.json(report, "/api/sounds").await {
        let ids: Vec<String> = sounds["sounds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["id"].as_str().map(str::to_owned))
            .collect();
        let on_disk: Vec<String> = std::fs::read_dir(data.join("sounds"))
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .filter_map(|e| {
                        e.file_name()
                            .to_str()
                            .and_then(|n| n.strip_suffix(".alert.json"))
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default();
        report.check(
            on_disk.iter().all(|s| ids.contains(s)),
            format!(
                "/api/sounds lists {ids:?} (default {}, unresolved {}); exported sounds on disk: {on_disk:?}",
                sounds["default_id"], sounds["unresolved"]
            ),
        );
    }
}

// ── Driving ──────────────────────────────────────────────────────────────────────────

/// The stored wake turn whose recording the speaker plays.
struct Source {
    device: String,
    token: String,
    turn: i64,
    transcript: String,
    recording: PathBuf,
    /// Where the candidate opened, in samples from the recording's start (its support
    /// starts at the recording's first sample).
    open: u64,
}

fn pick_source(data: &Path, stored: &Snapshot) -> Source {
    let devices = &stored.tables["devices"];
    let chosen = std::env::var("ECHOMUSE_DCC_TURN").ok().map(|spec| {
        let (device, turn) = spec
            .split_once('/')
            .expect("ECHOMUSE_DCC_TURN is <device id>/<turn id>");
        (device.to_owned(), turn.parse::<i64>().expect("a turn id"))
    });
    let mut turns: Vec<&Row> = stored.tables["turns"].rows.values().collect();
    turns.sort_by_key(|t| std::cmp::Reverse(t.rowid));
    for turn in turns {
        let device = turn.text("device_id").unwrap_or_default().to_owned();
        let id = turn.int("id").unwrap_or_default();
        if let Some((want_device, want_turn)) = &chosen
            && (want_device != &device || *want_turn != id)
        {
            continue;
        }
        let Some(row) = devices.rows.get(&device) else {
            continue;
        };
        let dot = row
            .text("os_version")
            .is_some_and(|v| v.starts_with("Fire OS 6."));
        let token = row.text("token").map(str::to_owned);
        let usable = turn.text("trigger_type") == Some("wake")
            && turn.text("terminal_reason") == Some("completed")
            && turn.text("audio_file").is_some()
            && turn.text("stt_text").is_some_and(|t| !t.is_empty())
            && row.int("approved") == Some(1);
        if chosen.is_none() && !(usable && dot) {
            continue;
        }
        let Some(token) = token else {
            panic!("{device} has no stored token to dial with");
        };
        let recording = data.join("recordings").join(format!("{device}_{id}.wav"));
        let trace = as_json(turn.get("decision_trace"));
        let window = trace.as_ref().and_then(|t| {
            Some((
                t["utterance"]["start"].as_u64()?,
                t["utterance"]["trigger"].as_u64()?,
            ))
        });
        let Some((start, trigger)) = window.filter(|(s, t)| t > s) else {
            assert!(
                chosen.is_none(),
                "turn {id} has no utterance start and trigger in its trace"
            );
            continue;
        };
        if !recording.is_file() {
            assert!(chosen.is_none(), "{} is not kept", recording.display());
            continue;
        }
        return Source {
            device,
            token,
            turn: id,
            transcript: turn.text("stt_text").unwrap_or_default().to_owned(),
            recording,
            open: trigger - start,
        };
    }
    panic!("no stored wake turn fits; name one with ECHOMUSE_DCC_TURN");
}

async fn wait_for_file(path: &Path, limit: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Waits until the controller's log says the speech models are ready.
async fn wait_for_speech(log: &Path) {
    let deadline = tokio::time::Instant::now() + READY_WAIT;
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if text.contains("speech models are ready") {
            return;
        }
        assert!(
            !text.contains("speech is unavailable"),
            "the controller reports speech unavailable; see {}",
            log.display()
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "speech was not ready within {READY_WAIT:?}; see {}",
            log.display()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn start_ha(stored: &Snapshot, out: &Path) -> SimHa {
    let ids: Vec<&str> = stored.tables["devices"]
        .rows
        .keys()
        .map(String::as_str)
        .collect();
    let ha = SimHa::start(&ids).await;
    let url = format!("http://{}", ha.address);
    // The token first: the script reads both once `ha_url` appears.
    std::fs::write(out.join("ha_token"), HA_TOKEN).unwrap();
    std::fs::write(out.join("ha_url.part"), &url).unwrap();
    std::fs::rename(out.join("ha_url.part"), out.join("ha_url")).unwrap();
    eprintln!("simulated Home Assistant at {url}");
    ha
}

async fn drive(data: PathBuf, out: PathBuf) {
    let mut report = Report::default();
    // reqwest is built without a default TLS provider; the controller installs ring too.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let db = data.join("echomuse.db");
    let before = snapshot(&db);
    let source = pick_source(&data, &before);
    REAL_TOKEN
        .set((source.device.clone(), source.token.clone()))
        .expect("set once");
    report.line(format!(
        "source: stored wake turn {} of {} ({} samples to the trigger)",
        source.turn, source.device, source.open
    ));
    let ha = start_ha(&before, &out).await;

    wait_for_speech(Path::new(&dcc_env("ECHOMUSE_DCC_LOG"))).await;
    let api = Api::new(
        &dcc_env_or("ECHOMUSE_DCC_API", "127.0.0.1:8768"),
        dcc_env("ECHOMUSE_DCC_SESSION"),
    );
    within("the dashboard API", async {
        while api
            .client
            .get(format!("{}/", api.base))
            .send()
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    let started = snapshot(&db);
    report_counts(&mut report, "at startup", &before, &started);
    check_api(&mut report, &api, &data, &before).await;

    // The wake turn: quiet, the recording with the candidate where the device opened it,
    // then quiet for the endpoint.
    report.line("── a wake turn from the copied device ──");
    // The speaker keeps its device id for the process's life; the static holds it.
    let device: &'static str = &REAL_TOKEN.get().expect("set above").0;
    let address: SocketAddr = dcc_env_or("ECHOMUSE_DCC_DEVICE_ADDR", "127.0.0.1:8767")
        .parse()
        .unwrap();
    let recording = read_wav(&source.recording);
    let open = source.open.min(recording.len() as u64);
    let last_turn = before.tables["turns"]
        .rows
        .values()
        .filter_map(|t| t.int("id"))
        .max()
        .unwrap_or(0);
    // The copied Dot's firmware serves the native AFE stream (its stored turns carry AFE
    // series), which the simulated speaker does too: the turn gets its chart and its
    // turn recording.
    let speaker = Speaker::connect(address, device, &["afe_metadata_v1"]).await;
    report.line(format!(
        "{device} attached with its stored token; session.ready granted capture"
    ));
    ha.hears(&source.transcript);
    ha.answers(IntentPlan::answer("Done."));
    speaker.say(silence(3_000));
    let base = speaker.position();
    speaker.say(recording[..open as usize].to_vec());
    speaker.push(Item::Candidate {
        id: "data-copy".to_owned(),
        support_start: base,
        open: base + open,
        producing_sound: false,
    });
    speaker.say(recording[open as usize..].to_vec());
    speaker.say(silence(4_000));
    let ack = speaker.candidate_ack("data-copy").await;
    report.check(ack == "accepted", format!("the candidate was {ack}"));
    let new_turn = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            let conn = open_read_only(&db);
            let id: Option<i64> = conn
                .query_row(
                    "SELECT max(id) FROM turns WHERE device_id = ?1 AND id > ?2",
                    rusqlite::params![device, last_turn],
                    |row| row.get(0),
                )
                .unwrap();
            if let Some(id) = id {
                break id;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no turn row was written"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    // Late writes to the row (the correction record) land within the turn's few seconds.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let stt = ha.state().runs("stt").len();
    let intents = intent_texts(&ha);
    report.line(format!(
        "turn {new_turn} written; Home Assistant ran {stt} STT runs and heard {intents:?}"
    ));
    let (terminal, close) = {
        let conn = open_read_only(&db);
        conn.query_row(
            "SELECT terminal_reason, decision_trace FROM turns WHERE id = ?1",
            [new_turn],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .unwrap()
    };
    // A turn that did not complete says nothing about the rows a normal turn writes. The
    // speech jobs have deadlines, so a machine too busy to run them in time ends it.
    let close = close
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|t| {
            t["utterance"]["endpoint"]
                .as_array()?
                .iter()
                .find(|e| e["event"] == "close")
                .cloned()
        });
    report.check(
        terminal.as_deref() == Some("completed") && stt == 1,
        format!("the turn ended {terminal:?} (endpoint close: {close:?})"),
    );
    for (suffix, folder) in [
        ("wake", "wakes"),
        ("audio", "recordings"),
        ("recording", "turn_recordings"),
    ] {
        let (status, body) = api
            .get(&format!("/api/devices/{device}/turns/{new_turn}/{suffix}"))
            .await;
        let header = (body.len() >= 44 && &body[..4] == b"RIFF").then(|| {
            (
                u16::from_le_bytes([body[22], body[23]]),
                u32::from_le_bytes([body[24], body[25], body[26], body[27]]),
            )
        });
        report.line(format!(
            "  new turn's {suffix} ({folder}/): {status}, {} bytes, WAV (channels, rate) {header:?}",
            body.len()
        ));
    }
    // What dashboard.jsx reads from a wake turn's trace: PauseTranscripts reads
    // `utterance.pauses`, `utterance.stable_prefix` and the commit event's `source`.
    if let Some(trace) = api
        .json(
            &mut report,
            &format!("/api/devices/{device}/turns/{new_turn}/trace"),
        )
        .await
    {
        let utterance = &trace["utterance"];
        let commit_source = utterance["endpoint"]
            .as_array()
            .and_then(|events| events.iter().find(|e| e["event"] == "commit"))
            .map(|commit| commit["source"].clone());
        report.check(
            utterance["pauses"].is_array()
                && utterance["stable_prefix"].is_string()
                && commit_source.as_ref().is_some_and(Value::is_string),
            format!(
                "the new turn's trace has what the dashboard reads: {} pauses, stable_prefix {}, \
                 commit source {commit_source:?}",
                utterance["pauses"].as_array().map_or(0, Vec::len),
                utterance["stable_prefix"]
            ),
        );
    }
    std::fs::write(
        out.join("drive.json"),
        json!({"device": device, "turn": new_turn, "source_turn": source.turn}).to_string(),
    )
    .unwrap();
    drop(speaker);
    tokio::time::sleep(Duration::from_secs(2)).await;
    std::fs::write(out.join("driven"), b"").unwrap();

    // The script stops the controller; then everything it wrote is on disk.
    assert!(
        wait_for_file(&out.join("stopped"), Duration::from_secs(300)).await,
        "the script did not stop the controller"
    );
    let after = snapshot(&db);
    report_counts(&mut report, "over the whole run", &before, &after);
    compare_written(&mut report, &before, &after);
    drop(ha);

    let defects = report.defects.len();
    report.line(format!("{defects} defects"));
    std::fs::write(out.join("report-drive.txt"), report.lines.join("\n") + "\n").unwrap();
    assert!(report.defects.is_empty(), "{:#?}", report.defects);
}

async fn serve(data: PathBuf, out: PathBuf) {
    let stored = snapshot(&data.join("echomuse.db"));
    let _ha = start_ha(&stored, &out).await;
    wait_for_file(&out.join("stop_ha"), Duration::from_secs(7_200)).await;
}

/// See the module documentation; run by tools/data-copy-check.sh.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs inside tools/data-copy-check.sh's isolated namespace on a copied installation"]
async fn a_copied_installation_under_the_controller() {
    let data = PathBuf::from(dcc_env("ECHOMUSE_DCC_DATA"));
    let out = PathBuf::from(dcc_env("ECHOMUSE_DCC_OUT"));
    match dcc_env("ECHOMUSE_DCC_MODE").as_str() {
        "drive" => drive(data, out).await,
        "serve" => serve(data, out).await,
        other => panic!("ECHOMUSE_DCC_MODE is drive or serve, not {other}"),
    }
}
