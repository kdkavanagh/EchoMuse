//! Why a record command did not apply.

use std::fmt;

/// The failure of one record command. Any error rolls its whole transaction back.
#[derive(Debug)]
pub enum RecordError {
    /// SQLite refused a statement.
    Sqlite(rusqlite::Error),
    /// The operation key (a turn UUID, candidate id or near-miss UUID) already names a row of
    /// another device, so the command cannot be a repeat of that one.
    KeyConflict {
        /// The table.
        table: &'static str,
        /// The key's text.
        key: String,
    },
    /// The record breaks a rule of its table, named here.
    Invalid(&'static str),
    /// A stored value is not in the shape the column holds.
    BadStoredValue {
        /// The table.
        table: &'static str,
        /// The column.
        column: &'static str,
        /// The row.
        id: i64,
    },
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "sqlite: {e}"),
            Self::KeyConflict { table, key } => {
                write!(f, "{table}: key {key} already belongs to another device")
            }
            Self::Invalid(rule) => write!(f, "invalid record: {rule}"),
            Self::BadStoredValue { table, column, id } => {
                write!(f, "{table}.{column} of row {id} is not in its stored shape")
            }
        }
    }
}

impl std::error::Error for RecordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for RecordError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Why turn-crate traces could not be mapped onto a row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingError {
    /// The wake trace has no decision yet.
    Undecided,
    /// The wake trace was refused, but the row describes an accepted wake (or the reverse).
    OutcomeMismatch,
    /// The utterance committed, but no commit id was given (or one was given without a
    /// commit).
    CommitId,
}

impl fmt::Display for MappingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Undecided => "the wake candidate is not decided",
            Self::OutcomeMismatch => "the wake decision does not match the row",
            Self::CommitId => "a commit id is given exactly when the utterance committed",
        })
    }
}

impl std::error::Error for MappingError {}
