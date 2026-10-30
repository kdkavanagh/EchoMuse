//! The persisted-data contract (§3.5.1): the schema at `schema_version` 40, read from a
//! deployed database and kept in `schema/v40.sql`, plus the check that an opened database
//! has exactly that layout.

use std::{collections::BTreeMap, fmt};

use rusqlite::Connection;

/// The only `schema_version` this controller reads or writes.
pub const SCHEMA_VERSION: u32 = 40;

/// Every table and index of the contract, as SQLite stores them in `sqlite_master`.
pub const SCHEMA_SQL: &str = include_str!("../schema/v40.sql");

/// The `system_config` keys this crate reads or writes. Other keys in the table are kept
/// untouched.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SystemConfigKey {
    /// The layout version of the whole database, stored as decimal text.
    SchemaVersion,
    /// `strict` or `auto` (§4.10); it takes precedence over the `DEVICE_APPROVAL` setting.
    DeviceApproval,
    /// The ESPHome port the next device gets, stored as decimal text (§3.3.1).
    NextEsphomePort,
}

impl SystemConfigKey {
    /// The key exactly as stored.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SchemaVersion => "schema_version",
            Self::DeviceApproval => "device_approval",
            Self::NextEsphomePort => "next_esphome_port",
        }
    }
}

impl fmt::Display for SystemConfigKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The kind of a schema object in `sqlite_master`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ObjectKind {
    Table,
    Index,
    View,
    Trigger,
}

impl ObjectKind {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "table" => Some(Self::Table),
            "index" => Some(Self::Index),
            "view" => Some(Self::View),
            "trigger" => Some(Self::Trigger),
            _ => None,
        }
    }

    /// The kind as `sqlite_master.type` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Index => "index",
            Self::View => "view",
            Self::Trigger => "trigger",
        }
    }
}

/// One way an opened database differs from the contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaDifference {
    /// The contract has this object and the database does not.
    Missing { kind: ObjectKind, name: String },
    /// The database has an object the contract does not.
    Unexpected { kind: ObjectKind, name: String },
    /// A table's columns differ. `changed` names columns whose type, NOT NULL, default
    /// or primary-key position differ.
    Columns {
        table: String,
        missing: Vec<String>,
        unexpected: Vec<String>,
        changed: Vec<String>,
    },
    /// The columns match (or the object is no table), but the definition differs: a
    /// constraint, column order, or an index's columns.
    Definition { kind: ObjectKind, name: String },
}

impl fmt::Display for SchemaDifference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { kind, name } => write!(f, "{} {name} is missing", kind.as_str()),
            Self::Unexpected { kind, name } => {
                write!(f, "{} {name} is not part of the schema", kind.as_str())
            }
            Self::Columns {
                table,
                missing,
                unexpected,
                changed,
            } => {
                write!(f, "table {table}:")?;
                let groups = [
                    ("missing", missing),
                    ("unexpected", unexpected),
                    ("changed", changed),
                ];
                for (label, columns) in groups {
                    if !columns.is_empty() {
                        write!(f, " {label} columns {}", columns.join(", "))?;
                    }
                }
                Ok(())
            }
            Self::Definition { kind, name } => {
                write!(f, "{} {name} has a different definition", kind.as_str())
            }
        }
    }
}

struct SchemaObject {
    sql: String,
}

/// Tables, indexes, views and triggers, without SQLite's own `sqlite_*` objects
/// (`sqlite_sequence`, automatic indexes, `ANALYZE` statistics).
fn schema_objects(
    conn: &Connection,
) -> rusqlite::Result<BTreeMap<(ObjectKind, String), SchemaObject>> {
    let mut statement = conn.prepare(
        "SELECT type, name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\'",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut objects = BTreeMap::new();
    for row in rows {
        let (kind, name, sql) = row?;
        let Some(kind) = ObjectKind::parse(&kind) else {
            continue;
        };
        objects.insert(
            (kind, name),
            SchemaObject {
                sql: normalize_sql(sql.as_deref().unwrap_or_default()),
            },
        );
    }
    Ok(objects)
}

/// SQL text with every run of whitespace reduced to one space and no space beside a
/// parenthesis or comma, so that layout alone never counts as a difference.
fn normalize_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut space = false;
    for c in sql.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() && !matches!(c, '(' | ')' | ',') && !out.ends_with(['(', ',']) {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    out
}

#[derive(Eq, PartialEq)]
struct ColumnShape {
    declared_type: String,
    not_null: bool,
    default: Option<String>,
    primary_key_position: i64,
    hidden: i64,
}

fn columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<(String, ColumnShape)>> {
    let mut statement = conn.prepare(
        "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1)",
    )?;
    let rows = statement.query_map([table], |row| {
        Ok((
            row.get::<_, String>(0)?,
            ColumnShape {
                declared_type: row.get(1)?,
                not_null: row.get::<_, i64>(2)? != 0,
                default: row.get(3)?,
                primary_key_position: row.get(4)?,
                hidden: row.get(5)?,
            },
        ))
    })?;
    rows.collect()
}

fn column_difference(
    contract: &Connection,
    found: &Connection,
    table: &str,
) -> rusqlite::Result<Option<SchemaDifference>> {
    let expected = columns(contract, table)?;
    let actual = columns(found, table)?;
    let missing: Vec<String> = expected
        .iter()
        .filter(|(name, _)| !actual.iter().any(|(n, _)| n == name))
        .map(|(name, _)| name.clone())
        .collect();
    let unexpected: Vec<String> = actual
        .iter()
        .filter(|(name, _)| !expected.iter().any(|(n, _)| n == name))
        .map(|(name, _)| name.clone())
        .collect();
    let changed: Vec<String> = expected
        .iter()
        .filter(|(name, shape)| actual.iter().any(|(n, s)| n == name && s != shape))
        .map(|(name, _)| name.clone())
        .collect();
    if missing.is_empty() && unexpected.is_empty() && changed.is_empty() {
        return Ok(None);
    }
    Ok(Some(SchemaDifference::Columns {
        table: table.to_owned(),
        missing,
        unexpected,
        changed,
    }))
}

/// Every way the database's schema differs from the contract; empty when it matches.
pub(crate) fn schema_differences(found: &Connection) -> rusqlite::Result<Vec<SchemaDifference>> {
    let contract = Connection::open_in_memory()?;
    contract.execute_batch(SCHEMA_SQL)?;
    let expected = schema_objects(&contract)?;
    let actual = schema_objects(found)?;

    let mut differences = Vec::new();
    for ((kind, name), object) in &expected {
        match actual.get(&(*kind, name.clone())) {
            None => differences.push(SchemaDifference::Missing {
                kind: *kind,
                name: name.clone(),
            }),
            Some(found_object) if found_object.sql != object.sql => {
                let columns = match kind {
                    ObjectKind::Table => column_difference(&contract, found, name)?,
                    _ => None,
                };
                differences.push(columns.unwrap_or(SchemaDifference::Definition {
                    kind: *kind,
                    name: name.clone(),
                }));
            }
            Some(_) => {}
        }
    }
    for (kind, name) in actual.keys() {
        if !expected.contains_key(&(*kind, name.clone())) {
            differences.push(SchemaDifference::Unexpected {
                kind: *kind,
                name: name.clone(),
            });
        }
    }
    Ok(differences)
}
