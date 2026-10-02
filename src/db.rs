//! SQLite storage: the v1 schema, plus the session bindings and the audit log of v2.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("cannot prepare the database file {path}: {source}")]
    File { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  sender TEXT NOT NULL,
  recipient TEXT NOT NULL,
  content TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS deliveries (
  message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  recipient TEXT NOT NULL,
  read_at TEXT,
  PRIMARY KEY (message_id, recipient)
);
CREATE INDEX IF NOT EXISTS idx_deliveries_unread
  ON deliveries(recipient) WHERE read_at IS NULL;
CREATE TABLE IF NOT EXISTS agents (
  name TEXT PRIMARY KEY,
  first_seen TEXT NOT NULL,
  last_seen TEXT,
  online INTEGER NOT NULL DEFAULT 0,
  presence_at TEXT,
  wake_info TEXT
);
CREATE TABLE IF NOT EXISTS wakes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  recipient TEXT NOT NULL,
  created_at TEXT NOT NULL,
  ok INTEGER NOT NULL,
  detail TEXT
);
CREATE TABLE IF NOT EXISTS codex_sessions (
  mailbox TEXT PRIMARY KEY,
  family TEXT NOT NULL CHECK (family = 'codex'),
  session_id TEXT NOT NULL UNIQUE,
  display_label TEXT NOT NULL UNIQUE,
  cwd TEXT NOT NULL,
  lifecycle TEXT NOT NULL,
  registered_at TEXT NOT NULL,
  last_seen TEXT NOT NULL,
  FOREIGN KEY (mailbox) REFERENCES agents(name) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_codex_sessions_recent
  ON codex_sessions(family, last_seen DESC);
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
  mailbox TEXT PRIMARY KEY,
  session_key TEXT NOT NULL,
  bound_at TEXT NOT NULL,
  last_seen TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  created_at TEXT NOT NULL,
  client TEXT NOT NULL,
  sender TEXT NOT NULL,
  recipient TEXT NOT NULL,
  bytes INTEGER NOT NULL,
  content_sha256 TEXT NOT NULL,
  outcome TEXT NOT NULL,
  reason TEXT
);
CREATE TABLE IF NOT EXISTS members (
  mailbox TEXT PRIMARY KEY,
  team TEXT NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('lead', 'worker')),
  joined_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_members_one_lead
  ON members(team) WHERE role = 'lead';
";

/// The current time in the ISO 8601 format of the v1 daemon, for example `2026-10-01T22:07:59.392Z`.
#[must_use]
pub fn iso_now() -> String {
    iso(SystemTime::now())
}

#[must_use]
pub fn iso(time: SystemTime) -> String {
    humantime::format_rfc3339_millis(time).to_string()
}

fn private_file(path: &Path) -> Result<(), DbError> {
    let file_error = |source| DbError::File {
        path: path.to_owned(),
        source,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(file_error)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(file_error)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(file_error)
}

fn prepare(connection: &Connection) -> Result<(), DbError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.execute_batch(SCHEMA)?;
    let has_sender_role: bool = connection.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('messages') WHERE name = 'sender_role'",
        [],
        |row| row.get(0),
    )?;
    if !has_sender_role {
        connection.execute_batch("ALTER TABLE messages ADD COLUMN sender_role TEXT")?;
    }
    Ok(())
}

/// Opens or creates the database with mode 600 and applies the schema and migrations.
///
/// # Errors
/// Returns an error when the file cannot be created or the schema cannot be applied.
pub fn open(path: &Path) -> Result<Connection, DbError> {
    private_file(path)?;
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    prepare(&connection)?;
    for suffix in ["-wal", "-shm"] {
        let mut companion = path.as_os_str().to_owned();
        companion.push(suffix);
        let companion = PathBuf::from(companion);
        if companion.exists() {
            fs::set_permissions(&companion, fs::Permissions::from_mode(0o600)).map_err(
                |source| DbError::File {
                    path: companion.clone(),
                    source,
                },
            )?;
        }
    }
    Ok(connection)
}

/// An in-memory database with the full schema, for tests.
///
/// # Errors
/// Returns an error when the schema cannot be applied.
pub fn open_in_memory() -> Result<Connection, DbError> {
    let connection = Connection::open_in_memory()?;
    prepare(&connection)?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("inband-db-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_a_private_database_with_the_full_schema() {
        let dir = temp_dir("new");
        let path = dir.join("nested").join("bridge.db");
        let connection = open(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN
                 ('messages','deliveries','agents','wakes','codex_sessions','settings','sessions','audit','members')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 9);
        drop(connection);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migrates_a_v1_database_without_losing_messages() {
        let dir = temp_dir("v1");
        let path = dir.join("bridge.db");
        {
            let old = Connection::open(&path).unwrap();
            old.execute_batch(
                "CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, sender TEXT NOT NULL,
                   recipient TEXT NOT NULL, content TEXT NOT NULL, created_at TEXT NOT NULL);
                 INSERT INTO messages(sender, recipient, content, created_at)
                   VALUES ('claude-a-0001', 'claude-b-0002', 'old', '2026-01-01T00:00:00.000Z');",
            )
            .unwrap();
        }
        let connection = open(&path).unwrap();
        let (content, role): (String, Option<String>) = connection
            .query_row("SELECT content, sender_role FROM messages", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(content, "old");
        assert_eq!(role, None);
        drop(connection);
        // Opening twice must not fail on the existing column.
        open(&path).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn timestamps_use_the_v1_format() {
        let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_790_000_000_123);
        assert_eq!(iso(time), "2026-09-21T14:13:20.123Z");
    }
}
