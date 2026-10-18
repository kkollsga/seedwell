//! Error types. Messages always name the file and, for validation, the field path.

use std::fmt;
use std::path::PathBuf;

/// One validation failure: where it is and why it is wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    /// Dotted path such as `transfers.alt.schedule.from` or `rule[2].goal.ratio`.
    pub path: String,
    /// Human-readable reason.
    pub reason: String,
}

impl FieldError {
    /// Build an error for `path`.
    pub fn new(path: impl Into<String>, reason: impl Into<String>) -> Self {
        FieldError { path: path.into(), reason: reason.into() }
    }
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

/// A non-fatal finding while loading a document (unknown key, suspicious value).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// Dotted path of the offending key.
    pub path: String,
    /// What is odd about it.
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Failure to load, validate or save one of the TOML documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocError {
    /// Reading or writing the file failed.
    Io {
        /// File involved.
        path: String,
        /// OS error text.
        message: String,
    },
    /// The file is larger than the hard cap (files on disk are untrusted).
    TooLarge {
        /// File involved.
        path: String,
        /// Its size.
        size: u64,
        /// The cap.
        max: u64,
    },
    /// Not valid TOML, or a value has the wrong type.
    Parse {
        /// Document name (`config.toml`, ...).
        file: &'static str,
        /// 1-based line, when known.
        line: Option<usize>,
        /// 1-based column, when known.
        column: Option<usize>,
        /// Parser message.
        message: String,
    },
    /// Valid TOML, but semantically invalid.
    Invalid {
        /// Document name.
        file: &'static str,
        /// Every problem found (not just the first).
        errors: Vec<FieldError>,
    },
    /// The file was written by a newer version of the app.
    Unsupported {
        /// Document name.
        file: &'static str,
        /// Version in the file.
        found: u32,
        /// Highest version this build understands.
        supported: u32,
    },
}

impl fmt::Display for DocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocError::Io { path, message } => write!(f, "{path}: {message}"),
            DocError::TooLarge { path, size, max } => {
                write!(f, "{path}: {size} bytes exceeds the {max} byte limit")
            }
            DocError::Parse { file, line, column, message } => {
                write!(f, "{file}")?;
                if let Some(l) = line {
                    write!(f, ":{l}")?;
                    if let Some(c) = column {
                        write!(f, ":{c}")?;
                    }
                }
                write!(f, ": {message}")
            }
            DocError::Invalid { file, errors } => {
                write!(f, "{file}: {} invalid value(s)", errors.len())?;
                for e in errors {
                    write!(f, "\n  - {e}")?;
                }
                Ok(())
            }
            DocError::Unsupported { file, found, supported } => write!(
                f,
                "{file}: version {found} was written by a newer Seedwell (this build understands up to {supported})"
            ),
        }
    }
}

impl std::error::Error for DocError {}

impl DocError {
    pub(crate) fn io(path: &std::path::Path, e: &std::io::Error) -> Self {
        DocError::Io { path: path.display().to_string(), message: e.to_string() }
    }
}

/// Failure in the database layer.
#[derive(Debug)]
pub enum StateError {
    /// A filesystem operation failed.
    Io {
        /// What we were doing.
        context: &'static str,
        /// The OS error.
        source: std::io::Error,
    },
    /// The embedded database reported an error.
    Db(Box<redb::Error>),
    /// The database was written by a newer schema; it was left untouched.
    NewerSchema {
        /// Version stored in the file.
        found: u32,
        /// Highest version this build understands.
        supported: u32,
        /// The file.
        path: PathBuf,
    },
    /// The registry has no migration step from this version.
    NoMigration {
        /// Version with no outgoing step.
        from: u32,
    },
    /// A migration step failed; the database is unchanged.
    Migration {
        /// Step name.
        name: &'static str,
        /// Failure text.
        reason: String,
        /// Backup taken before the attempt.
        backup: PathBuf,
    },
    /// The file is damaged and could not be recovered (only returned when moving it aside failed).
    Corrupt(String),
    /// A value exceeded its cap.
    TooLarge {
        /// What was too large.
        what: &'static str,
        /// Actual size.
        size: usize,
        /// Cap.
        max: usize,
    },
    /// The store has been closed (or its writer thread died).
    Closed,
    /// An argument was not acceptable (for example the `All` scope for a write).
    Invalid(&'static str),
    /// A deferred write failed; the batch is kept and retried.
    WriteFailed(String),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StateError::Io { context, source } => write!(f, "{context}: {source}"),
            StateError::Db(e) => write!(f, "database error: {e}"),
            StateError::NewerSchema { found, supported, path } => write!(
                f,
                "{} uses schema {found} but this Seedwell understands up to {supported}; \
                 the file was not modified. Update Seedwell to open it",
                path.display()
            ),
            StateError::NoMigration { from } => {
                write!(f, "no migration path from schema version {from}")
            }
            StateError::Migration { name, reason, backup } => write!(
                f,
                "migration `{name}` failed ({reason}); the database is unchanged, backup at {}",
                backup.display()
            ),
            StateError::Corrupt(m) => write!(f, "database is corrupt: {m}"),
            StateError::TooLarge { what, size, max } => {
                write!(f, "{what} is {size} bytes, the limit is {max}")
            }
            StateError::Closed => write!(f, "the state store is closed"),
            StateError::Invalid(m) => write!(f, "invalid argument: {m}"),
            StateError::WriteFailed(m) => write!(f, "state write failed: {m}"),
        }
    }
}

impl std::error::Error for StateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StateError::Io { source, .. } => Some(source),
            StateError::Db(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl<E: Into<redb::Error>> From<E> for StateError
where
    E: RedbErr,
{
    fn from(e: E) -> Self {
        // Boxed: `redb::Error` is 160 bytes and would bloat every `Result` in the crate.
        StateError::Db(Box::new(e.into()))
    }
}

/// Marker so the blanket conversion above only covers redb's own error types.
pub trait RedbErr {}
impl RedbErr for redb::DatabaseError {}
impl RedbErr for redb::TransactionError {}
impl RedbErr for redb::TableError {}
impl RedbErr for redb::StorageError {}
impl RedbErr for redb::CommitError {}
impl RedbErr for redb::Error {}

/// A binary record failed to decode. Records come from disk and are untrusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bad record: {}", self.0)
    }
}
impl std::error::Error for DecodeError {}
