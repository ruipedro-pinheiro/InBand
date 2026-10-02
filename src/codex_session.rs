//! The registry of Codex sessions.
//!
//! The mailbox of a Codex session is `codex-<session uuid>`, so its name tells which session owns
//! it. Each session also has a short label for humans, for example `codex-myrepo-019f6767`.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::db::iso_now;

/// The prefix of the Codex mailboxes. As a recipient, it means the latest Codex session.
pub const CODEX_FAMILY: &str = "codex";
const MAX_AGENT_NAME_LENGTH: usize = 64;
/// The lengths of the uuid part of a label, the shortest first. A longer one is used only when
/// another session has the shorter label.
const LABEL_SUFFIX_LENGTHS: [usize; 3] = [8, 12, 32];

#[derive(Debug, thiserror::Error)]
pub enum CodexSessionError {
    #[error("invalid Codex session UUID \"{0}\": expected canonical 8-4-4-4-12 hexadecimal form")]
    InvalidUuid(String),
    #[error("unable to create a unique display label for Codex session {0}")]
    NoLabel(String),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

/// One Codex session of the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSession {
    /// `codex-<uuid>`.
    pub mailbox: String,
    /// The uuid in lower case.
    pub session_id: String,
    /// The short name for humans, for example `codex-myrepo-019f6767`.
    pub display_label: String,
    /// The working directory of the session.
    pub cwd: String,
    /// `active` during a turn, `idle` after it.
    pub lifecycle: String,
    pub registered_at: String,
    pub last_seen: String,
}

impl CodexSession {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            mailbox: row.get("mailbox")?,
            session_id: row.get("session_id")?,
            display_label: row.get("display_label")?,
            cwd: row.get("cwd")?,
            lifecycle: row.get("lifecycle")?,
            registered_at: row.get("registered_at")?,
            last_seen: row.get("last_seen")?,
        })
    }
}

/// Returns `true` for an 8-4-4-4-12 hex uuid.
fn is_canonical_uuid(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Returns the session id in lower case.
///
/// # Errors
///
/// Returns an error when the id is not an 8-4-4-4-12 hex uuid.
pub fn normalize_session_id(session_id: &str) -> Result<String, CodexSessionError> {
    let normalized = session_id.to_ascii_lowercase();
    if is_canonical_uuid(&normalized) {
        Ok(normalized)
    } else {
        Err(CodexSessionError::InvalidUuid(session_id.to_owned()))
    }
}

/// Returns the mailbox of a session: `codex-<uuid>`.
///
/// # Errors
///
/// Returns an error when the id is not a valid uuid.
pub fn canonical_mailbox(session_id: &str) -> Result<String, CodexSessionError> {
    Ok(format!(
        "{CODEX_FAMILY}-{}",
        normalize_session_id(session_id)?
    ))
}

/// Returns `true` for a `codex-<uuid>` name.
#[must_use]
pub fn is_canonical_mailbox(name: &str) -> bool {
    name.strip_prefix("codex-").is_some_and(is_canonical_uuid)
}

/// Returns a directory path as lower case letters, digits and `-`, or `root` when nothing stays.
fn slugify_cwd(cwd: &str) -> String {
    let mut slug = String::new();
    for c in cwd.to_ascii_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "root".to_owned()
    } else {
        slug.to_owned()
    }
}

/// Returns the label of a session: `codex-<directory>-<first suffix_length chars of the uuid>`, 64
/// chars at most.
///
/// # Errors
///
/// Returns an error when the id is not a valid uuid.
pub fn display_label(
    cwd: &str,
    session_id: &str,
    suffix_length: usize,
) -> Result<String, CodexSessionError> {
    let compact: String = normalize_session_id(session_id)?
        .chars()
        .filter(|c| *c != '-')
        .collect();
    let suffix: String = compact.chars().take(suffix_length).collect();
    let max_slug = MAX_AGENT_NAME_LENGTH - CODEX_FAMILY.len() - suffix.len() - 2;
    let slug: String = slugify_cwd(cwd).chars().take(max_slug).collect();
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() { "root" } else { slug };
    Ok(format!("{CODEX_FAMILY}-{slug}-{suffix}"))
}

/// Adds a session to the registry, or updates its directory, state and last-seen time. A known
/// session keeps its label.
///
/// # Errors
///
/// Returns an error when the id is not valid, when other sessions have all the possible labels, or
/// when the database fails.
pub fn register(
    db: &mut Connection,
    session_id: &str,
    cwd: &str,
    lifecycle: &str,
) -> Result<CodexSession, CodexSessionError> {
    let session_id = normalize_session_id(session_id)?;
    let mailbox = canonical_mailbox(&session_id)?;
    let now = iso_now();
    let tx = db.transaction()?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT display_label FROM codex_sessions WHERE session_id = ?1",
            [&session_id],
            |row| row.get(0),
        )
        .optional()?;
    let label = match existing {
        Some(label) => label,
        None => available_label(&tx, cwd, &session_id)?,
    };
    tx.execute(
        "INSERT INTO agents(name, first_seen, last_seen) VALUES (?1, ?2, ?2)
         ON CONFLICT(name) DO UPDATE SET last_seen = excluded.last_seen",
        params![mailbox, now],
    )?;
    tx.execute(
        "INSERT INTO codex_sessions(mailbox, family, session_id, display_label, cwd, lifecycle, registered_at, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
         ON CONFLICT(session_id) DO UPDATE SET cwd = excluded.cwd, lifecycle = excluded.lifecycle,
           last_seen = excluded.last_seen",
        params![mailbox, CODEX_FAMILY, session_id, label, cwd, lifecycle, now],
    )?;
    let session = tx.query_row(
        "SELECT * FROM codex_sessions WHERE mailbox = ?1",
        [&mailbox],
        CodexSession::from_row,
    )?;
    tx.commit()?;
    Ok(session)
}

/// Returns the shortest label that no other session has.
fn available_label(
    db: &Connection,
    cwd: &str,
    session_id: &str,
) -> Result<String, CodexSessionError> {
    for length in LABEL_SUFFIX_LENGTHS {
        let candidate = display_label(cwd, session_id, length)?;
        let taken: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM codex_sessions WHERE display_label = ?1)",
            [&candidate],
            |row| row.get(0),
        )?;
        if !taken {
            return Ok(candidate);
        }
    }
    Err(CodexSessionError::NoLabel(session_id.to_owned()))
}

/// Returns the session of a mailbox, or `None`.
///
/// # Errors
///
/// Returns an error when the database fails.
pub fn by_mailbox(
    db: &Connection,
    mailbox: &str,
) -> Result<Option<CodexSession>, CodexSessionError> {
    Ok(db
        .query_row(
            "SELECT * FROM codex_sessions WHERE mailbox = ?1",
            [mailbox.to_ascii_lowercase()],
            CodexSession::from_row,
        )
        .optional()?)
}

/// Returns the session with this id, or `None`.
///
/// # Errors
///
/// Returns an error when the id is not valid, or when the database fails.
pub fn by_session_id(
    db: &Connection,
    session_id: &str,
) -> Result<Option<CodexSession>, CodexSessionError> {
    let session_id = normalize_session_id(session_id)?;
    Ok(db
        .query_row(
            "SELECT * FROM codex_sessions WHERE session_id = ?1",
            [session_id],
            CodexSession::from_row,
        )
        .optional()?)
}

/// Returns the session that the registry saw last: the target of the `codex` alias.
///
/// # Errors
///
/// Returns an error when the database fails.
pub fn most_recent(db: &Connection) -> Result<Option<CodexSession>, CodexSessionError> {
    Ok(db
        .query_row(
            "SELECT * FROM codex_sessions WHERE family = ?1
             ORDER BY last_seen DESC, registered_at DESC, mailbox ASC LIMIT 1",
            [CODEX_FAMILY],
            CodexSession::from_row,
        )
        .optional()?)
}

/// Updates the last-seen time of a session, and its state when given.
///
/// # Errors
///
/// Returns an error when the database fails.
pub fn touch(
    db: &mut Connection,
    mailbox: &str,
    lifecycle: Option<&str>,
) -> Result<(), CodexSessionError> {
    let mailbox = mailbox.to_ascii_lowercase();
    let now = iso_now();
    let tx = db.transaction()?;
    match lifecycle {
        Some(lifecycle) => tx.execute(
            "UPDATE codex_sessions SET last_seen = ?1, lifecycle = ?2 WHERE mailbox = ?3",
            params![now, lifecycle, mailbox],
        )?,
        None => tx.execute(
            "UPDATE codex_sessions SET last_seen = ?1 WHERE mailbox = ?2",
            params![now, mailbox],
        )?,
    };
    tx.execute(
        "UPDATE agents SET last_seen = ?1 WHERE name = ?2",
        params![now, mailbox],
    )?;
    tx.commit()?;
    Ok(())
}

/// Returns each Codex session with its count of unread messages, the most recent first.
///
/// # Errors
///
/// Returns an error when the database fails.
pub fn list_with_unread(db: &Connection) -> Result<Vec<(CodexSession, i64)>, CodexSessionError> {
    let mut statement = db.prepare(
        "SELECT cs.*, COUNT(d.message_id) AS unread FROM codex_sessions cs
         LEFT JOIN deliveries d ON d.recipient = cs.mailbox AND d.read_at IS NULL
         WHERE cs.family = ?1 GROUP BY cs.mailbox
         ORDER BY cs.last_seen DESC, cs.registered_at DESC, cs.mailbox ASC",
    )?;
    let rows = statement.query_map([CODEX_FAMILY], |row| {
        Ok((CodexSession::from_row(row)?, row.get("unread")?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    const SESSION: &str = "019f6767-789c-73b2-bc5c-ac8575f29efd";

    #[test]
    fn builds_the_mailbox_and_label() {
        assert_eq!(
            canonical_mailbox(SESSION).unwrap(),
            "codex-019f6767-789c-73b2-bc5c-ac8575f29efd"
        );
        assert_eq!(
            display_label("/home/dev/project", SESSION, 8).unwrap(),
            "codex-home-dev-project-019f6767"
        );
        assert!(is_canonical_mailbox(
            "codex-019f6767-789c-73b2-bc5c-ac8575f29efd"
        ));
        assert!(!is_canonical_mailbox("codex-api-1234"));
        let long = "/".to_owned() + &"x".repeat(200);
        assert!(display_label(&long, SESSION, 32).unwrap().len() <= MAX_AGENT_NAME_LENGTH);
    }

    #[test]
    fn registering_twice_updates_one_row() {
        let mut db = open_in_memory().unwrap();
        let first = register(&mut db, SESSION, "/home/dev/old", "starting").unwrap();
        db.execute(
            "UPDATE codex_sessions SET last_seen = '2000-01-01T00:00:00.000Z' WHERE mailbox = ?1",
            [&first.mailbox],
        )
        .unwrap();
        let second = register(&mut db, &SESSION.to_uppercase(), "/home/dev/new", "ready").unwrap();
        assert_eq!(second.mailbox, first.mailbox);
        assert_eq!(second.display_label, first.display_label);
        assert_eq!(second.cwd, "/home/dev/new");
        assert_eq!(second.lifecycle, "ready");
        assert_ne!(second.last_seen, "2000-01-01T00:00:00.000Z");
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM codex_sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn disambiguates_labels_that_share_a_prefix() {
        let mut db = open_in_memory().unwrap();
        let first = register(&mut db, SESSION, "/same/cwd", "ready").unwrap();
        let second = register(
            &mut db,
            "019f6767-abcd-73b2-bc5c-ac8575f29efd",
            "/same/cwd",
            "ready",
        )
        .unwrap();
        assert_ne!(second.mailbox, first.mailbox);
        assert_ne!(second.display_label, first.display_label);
    }

    #[test]
    fn rejects_non_canonical_ids() {
        let mut db = open_in_memory().unwrap();
        assert!(matches!(
            register(&mut db, "019f6767789c73b2bc5cac8575f29efd", "/x", "ready"),
            Err(CodexSessionError::InvalidUuid(_))
        ));
        assert!(matches!(
            register(
                &mut db,
                "019f6767-789c-73b2-bc5c-ac8575f29efg",
                "/x",
                "ready"
            ),
            Err(CodexSessionError::InvalidUuid(_))
        ));
    }

    #[test]
    fn most_recent_and_unread_listing() {
        let mut db = open_in_memory().unwrap();
        register(&mut db, SESSION, "/a", "ready").unwrap();
        let other = register(
            &mut db,
            "019f6768-789c-73b2-bc5c-ac8575f29efd",
            "/b",
            "ready",
        )
        .unwrap();
        db.execute(
            "UPDATE codex_sessions SET last_seen = '2000-01-01T00:00:00.000Z' WHERE mailbox != ?1",
            [&other.mailbox],
        )
        .unwrap();
        assert_eq!(most_recent(&db).unwrap().unwrap().mailbox, other.mailbox);
        touch(&mut db, &other.mailbox, Some("idle")).unwrap();
        assert_eq!(
            by_mailbox(&db, &other.mailbox).unwrap().unwrap().lifecycle,
            "idle"
        );
        let listed = list_with_unread(&db).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|(_, unread)| *unread == 0));
        assert!(by_session_id(&db, SESSION).unwrap().is_some());
    }
}
