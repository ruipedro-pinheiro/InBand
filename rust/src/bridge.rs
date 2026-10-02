//! The message bus: send, read, wait, subscribe, roles, presence, wakes and the security checks.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::auth::{AuthInfo, agent_matches_pattern};
use crate::codex_session::{self, CODEX_FAMILY, CodexSessionError};
use crate::config::{BridgeConfig, WakeTarget, is_agent_name};
use crate::db::{iso, iso_now};
use crate::protocol::Role;
use crate::sanitize::{contains_token, sanitize};
use crate::wake::{WakeDispatch, WakeInput, WakeResult};

const MAX_PENDING_WAITS_PER_AGENT: usize = 8;
const MAX_PENDING_SUBSCRIPTIONS_PER_TARGET: usize = 8;
/// Clients without a progress token time out at 60 s, so short waits stay under it.
const MAX_WAIT_SECONDS: u64 = 50;
const MAX_LONG_WAIT_SECONDS: u64 = 1800;
const MAX_SUBSCRIBE_SECONDS: u64 = 300;
const MAX_HISTORY: u32 = 500;
/// Messages one sender may send per minute.
const SEND_RATE_PER_MINUTE: usize = 30;
/// Unread messages a recipient may hold before new direct mail is refused.
const MAX_UNREAD_PER_RECIPIENT: i64 = 200;
const STALE_AFTER_SECONDS: u64 = 1800;

/// `name`, `first_seen` and `last_seen` of the agents table.
type AgentRow = (String, String, Option<String>);

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("invalid {field} \"{raw}\": expected 1-64 chars of [a-z0-9_-]")]
    InvalidName { field: &'static str, raw: String },
    #[error("\"{0}\" is a recipient-only alias, not an agent identity")]
    AliasIdentity(String),
    #[error("content is empty")]
    Empty,
    #[error("content is {size} bytes; max is {max}")]
    TooLarge { size: usize, max: usize },
    #[error("cannot send a message to yourself")]
    SelfSend,
    #[error("Codex mailbox \"{0}\" is not registered")]
    CodexNotRegistered(String),
    #[error("cannot send to \"codex\": no registered Codex session")]
    NoCodexSession,
    #[error("mailbox \"{0}\" belongs to another session; send from your own session")]
    BoundToOtherSession(String),
    #[error("this token cannot use {field} \"{agent}\"")]
    NotAuthorized { field: &'static str, agent: String },
    #[error("\"{0}\" needs a request signed by its own session")]
    SessionRequired(String),
    #[error("routing refused: {0}")]
    Routing(&'static str),
    #[error("the message contains a configured token; secrets must not travel in mail")]
    ContainsToken,
    #[error("\"{0}\" is sending too fast: at most {SEND_RATE_PER_MINUTE} messages per minute")]
    RateLimited(String),
    #[error("\"{0}\" already has {MAX_UNREAD_PER_RECIPIENT} unread messages")]
    RecipientFull(String),
    #[error("too many pending waits for \"{0}\"")]
    TooManyWaits(String),
    #[error("too many pending subscriptions for \"{0}\"")]
    TooManySubscriptions(String),
    #[error("refusing to clear: pass confirm=\"wipe\" to delete all messages")]
    ClearNotConfirmed,
    #[error(transparent)]
    Codex(#[from] CodexSessionError),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

/// Who calls the bridge: the authenticated client, and the session that signed the request.
#[derive(Debug, Clone)]
pub struct Caller {
    pub auth: AuthInfo,
    /// Set only when the request is signed for one session (the channel shim or a hook).
    pub session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageRow {
    pub id: i64,
    pub sender: String,
    pub recipient: String,
    pub content: String,
    pub created_at: String,
    pub sender_role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendResult {
    pub message_id: i64,
    pub sent_at: String,
    pub requested_to: String,
    pub resolved_to: String,
    pub delivered_to: Vec<String>,
    pub notify: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStatus {
    pub name: String,
    pub first_seen: String,
    pub last_seen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub role: &'static str,
    pub connected: String,
    pub idle_seconds: Option<u64>,
    pub waiting_now: bool,
    pub unread: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WakeRecord {
    pub recipient: String,
    pub created_at: String,
    pub ok: i64,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub daemon: &'static str,
    pub started_at: String,
    /// The team of the viewer. `None` for a solo viewer and for the admin view.
    pub team: Option<String>,
    /// The lead of that team.
    pub lead: Option<String>,
    pub agents: Vec<AgentStatus>,
    pub last_wakes: Vec<WakeRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct History {
    pub messages: Vec<MessageRow>,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TeamChange {
    pub mailbox: String,
    /// The new team. `None` after `leave`.
    pub team: Option<String>,
    pub role: &'static str,
    /// The team the mailbox was in before, when it changed.
    pub previous_team: Option<String>,
    /// The lead that `set_lead` turned into a worker of the same team.
    pub replaced_lead: Option<String>,
}

#[derive(Debug, Clone)]
struct Delivery {
    id: i64,
    sender: String,
    content: String,
    created_at: String,
    sender_role: String,
    recipients: Vec<String>,
}

#[derive(Debug, Default)]
struct Listeners {
    waits: HashMap<String, usize>,
    subscriptions: Vec<(u64, String, bool)>,
}

impl Listeners {
    fn subscription_matches(&self, recipient: &str) -> bool {
        self.subscriptions
            .iter()
            .any(|(_, prefix, exact)| target_matches(recipient, prefix, *exact))
    }
}

fn target_matches(recipient: &str, prefix: &str, exact: bool) -> bool {
    recipient == prefix
        || (!exact
            && recipient
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('-')))
}

struct Routed {
    id: i64,
    resolved_to: String,
    recipients: Vec<String>,
    sender_role: String,
}

struct RetryState {
    generation: u64,
    task: Option<tokio::task::JoinHandle<()>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock leaves plain data behind, so keep serving.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The message bus.
pub struct Bridge {
    db: Mutex<Connection>,
    config: BridgeConfig,
    secrets: Vec<String>,
    wake: Arc<dyn WakeDispatch>,
    events: broadcast::Sender<Arc<Delivery>>,
    listeners: Mutex<Listeners>,
    send_times: Mutex<HashMap<String, VecDeque<Instant>>>,
    retries: Mutex<HashMap<String, RetryState>>,
    next_id: AtomicU64,
    started_at: String,
}

/// Decrements a listener count when the waiting future ends, also when a client disconnects.
struct WaitGuard<'a> {
    bridge: &'a Bridge,
    mailbox: String,
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        let mut listeners = lock(&self.bridge.listeners);
        if let Some(count) = listeners.waits.get_mut(&self.mailbox) {
            *count -= 1;
            if *count == 0 {
                listeners.waits.remove(&self.mailbox);
            }
        }
    }
}

struct SubscriptionGuard<'a> {
    bridge: &'a Bridge,
    id: u64,
}

impl Drop for SubscriptionGuard<'_> {
    fn drop(&mut self) {
        lock(&self.bridge.listeners)
            .subscriptions
            .retain(|(id, _, _)| *id != self.id);
    }
}

fn message_rows(
    db: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Vec<MessageRow>> {
    let mut statement = db.prepare(sql)?;
    let rows = statement.query_map(params, |row| {
        Ok(MessageRow {
            id: row.get(0)?,
            sender: row.get(1)?,
            recipient: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
            sender_role: row.get(5)?,
        })
    })?;
    rows.collect()
}

const UNREAD_SQL: &str =
    "SELECT m.id, m.sender, m.recipient, m.content, m.created_at, m.sender_role
     FROM deliveries d JOIN messages m ON m.id = d.message_id
     WHERE d.recipient = ?1 AND d.read_at IS NULL ORDER BY m.id ASC";

impl Bridge {
    #[must_use]
    pub fn new(
        db: Connection,
        config: BridgeConfig,
        secrets: Vec<String>,
        wake: Arc<dyn WakeDispatch>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            db: Mutex::new(db),
            config,
            secrets,
            wake,
            events,
            listeners: Mutex::default(),
            send_times: Mutex::default(),
            retries: Mutex::default(),
            next_id: AtomicU64::new(1),
            started_at: iso_now(),
        })
    }

    /// # Errors
    /// Returns [`BridgeError::InvalidName`] for a name outside `[a-z0-9_-]{1,64}`.
    pub fn normalize_agent(raw: &str, field: &'static str) -> Result<String, BridgeError> {
        let name = raw.trim().to_ascii_lowercase();
        if is_agent_name(&name) {
            Ok(name)
        } else {
            Err(BridgeError::InvalidName {
                field,
                raw: raw.to_owned(),
            })
        }
    }

    fn require_concrete(name: &str) -> Result<(), BridgeError> {
        if name == CODEX_FAMILY || name == "all" {
            Err(BridgeError::AliasIdentity(name.to_owned()))
        } else {
            Ok(())
        }
    }

    fn require_registered_codex(db: &Connection, name: &str) -> Result<(), BridgeError> {
        if codex_session::is_canonical_mailbox(name)
            && codex_session::by_mailbox(db, name)?.is_none()
        {
            return Err(BridgeError::CodexNotRegistered(name.to_owned()));
        }
        Ok(())
    }

    fn touch_agent(db: &mut Connection, name: &str) -> Result<(), BridgeError> {
        Self::require_concrete(name)?;
        if codex_session::is_canonical_mailbox(name) {
            Self::require_registered_codex(db, name)?;
            codex_session::touch(db, name, None)?;
            return Ok(());
        }
        db.execute(
            "INSERT INTO agents(name, first_seen, last_seen) VALUES (?1, ?2, ?2)
             ON CONFLICT(name) DO UPDATE SET last_seen = ?2",
            params![name, iso_now()],
        )?;
        Ok(())
    }

    fn membership_of(db: &Connection, mailbox: &str) -> rusqlite::Result<Option<(String, Role)>> {
        db.query_row(
            "SELECT team, role FROM members WHERE mailbox = ?1",
            [mailbox],
            |row| {
                let role: String = row.get(1)?;
                let role = if role == "lead" {
                    Role::Lead
                } else {
                    Role::Worker
                };
                Ok((row.get(0)?, role))
            },
        )
        .optional()
    }

    fn lead_of_team(db: &Connection, team: &str) -> rusqlite::Result<Option<String>> {
        db.query_row(
            "SELECT mailbox FROM members WHERE team = ?1 AND role = 'lead'",
            [team],
            |row| row.get(0),
        )
        .optional()
    }

    /// The team and role of a mailbox, or `None` for a solo session.
    ///
    /// # Errors
    /// Returns SQLite errors.
    pub fn membership(&self, name: &str) -> Result<Option<(String, Role)>, BridgeError> {
        Ok(Self::membership_of(&lock(&self.db), name)?)
    }

    /// # Errors
    /// Returns SQLite errors.
    pub fn team_lead(&self, team: &str) -> Result<Option<String>, BridgeError> {
        Ok(Self::lead_of_team(&lock(&self.db), team)?)
    }

    /// # Errors
    /// Returns SQLite errors.
    pub fn role_of(&self, name: &str) -> Result<Role, BridgeError> {
        Ok(self.membership(name)?.map_or(Role::Solo, |(_, role)| role))
    }

    fn bound_session(db: &Connection, mailbox: &str) -> rusqlite::Result<Option<String>> {
        db.query_row(
            "SELECT session_key FROM sessions WHERE mailbox = ?1",
            [mailbox],
            |row| row.get(0),
        )
        .optional()
    }

    /// Binds a mailbox to the session that signs the request. Called from signed `SessionStart`
    /// hooks. A binding is never moved to another session: that would let any session take the
    /// mailbox of another one, the lead included.
    ///
    /// # Errors
    /// Returns an error for an invalid mailbox, a token that cannot use it, a request without a
    /// session, a mailbox bound to another session, or SQLite.
    pub fn bind_session(&self, caller: &Caller, mailbox_raw: &str) -> Result<(), BridgeError> {
        let mailbox = Self::normalize_agent(mailbox_raw, "mailbox")?;
        Self::require_concrete(&mailbox)?;
        Self::require_token_scope(caller, "mailbox", &mailbox)?;
        let session_key = caller
            .session
            .clone()
            .ok_or_else(|| BridgeError::SessionRequired(mailbox.clone()))?;
        let now = iso_now();
        let mut db = lock(&self.db);
        match Self::bound_session(&db, &mailbox)? {
            Some(bound) if bound != session_key => {
                return Err(BridgeError::BoundToOtherSession(mailbox));
            }
            _ => {}
        }
        Self::touch_agent(&mut db, &mailbox)?;
        db.execute(
            "INSERT INTO sessions(mailbox, session_key, bound_at, last_seen) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(mailbox) DO UPDATE SET last_seen = ?3",
            params![mailbox, session_key, now],
        )?;
        Ok(())
    }

    fn require_token_scope(
        caller: &Caller,
        field: &'static str,
        mailbox: &str,
    ) -> Result<(), BridgeError> {
        if caller.auth.admin
            || caller
                .auth
                .agents
                .iter()
                .any(|pattern| agent_matches_pattern(mailbox, pattern))
        {
            Ok(())
        } else {
            Err(BridgeError::NotAuthorized {
                field,
                agent: mailbox.to_owned(),
            })
        }
    }

    /// Checks that the caller may act as `mailbox`.
    ///
    /// Sessions of one family share a token, so the token alone does not say which session calls.
    /// Only a request signed for the session bound to the mailbox proves it. The one exception is a
    /// token whose patterns name this mailbox exactly: no other mailbox can use it.
    fn check_acting(
        db: &Connection,
        caller: &Caller,
        field: &'static str,
        mailbox: &str,
    ) -> Result<(), BridgeError> {
        if caller.auth.admin {
            return Ok(());
        }
        Self::require_token_scope(caller, field, mailbox)?;
        match Self::bound_session(db, mailbox)? {
            Some(bound) if caller.session.as_deref() == Some(bound.as_str()) => Ok(()),
            Some(_) => Err(BridgeError::BoundToOtherSession(mailbox.to_owned())),
            None if caller.auth.agents.iter().any(|pattern| pattern == mailbox) => Ok(()),
            None => Err(BridgeError::SessionRequired(mailbox.to_owned())),
        }
    }

    /// Star routing inside one team. A solo session neither sends nor receives.
    fn check_routing(
        db: &Connection,
        sender: Option<&(String, Role)>,
        to: &str,
    ) -> Result<(), BridgeError> {
        let Some((team, role)) = sender else {
            return Err(BridgeError::Routing(
                "the sender is not in a team; only the user adds a session to a team",
            ));
        };
        if to == "all" {
            return if *role == Role::Lead {
                Ok(())
            } else {
                Err(BridgeError::Routing("only the lead can write to all"))
            };
        }
        match Self::membership_of(db, to)? {
            Some((to_team, _)) if to_team == *team => {}
            _ => {
                return Err(BridgeError::Routing(
                    "the recipient is not in the sender's team",
                ));
            }
        }
        if *role == Role::Lead {
            return Ok(());
        }
        match Self::lead_of_team(db, team)? {
            Some(lead) if lead == to => Ok(()),
            Some(_) => Err(BridgeError::Routing("a worker can write only to the lead")),
            None => Err(BridgeError::Routing("the team has no lead")),
        }
    }

    fn check_rate(&self, sender: &str) -> Result<(), BridgeError> {
        let now = Instant::now();
        let mut times = lock(&self.send_times);
        let queue = times.entry(sender.to_owned()).or_default();
        while queue
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_mins(1))
        {
            queue.pop_front();
        }
        if queue.len() >= SEND_RATE_PER_MINUTE {
            return Err(BridgeError::RateLimited(sender.to_owned()));
        }
        queue.push_back(now);
        Ok(())
    }

    fn audit(
        &self,
        caller: &Caller,
        from: &str,
        to: &str,
        content: &str,
        outcome: &str,
        reason: Option<&str>,
    ) {
        let digest = hex::encode(Sha256::digest(content.as_bytes()));
        let bytes = i64::try_from(content.len()).unwrap_or(i64::MAX);
        let result = lock(&self.db).execute(
            "INSERT INTO audit(created_at, client, sender, recipient, bytes, content_sha256, outcome, reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![iso_now(), caller.auth.client_id, from, to, bytes, digest, outcome, reason],
        );
        if let Err(error) = result {
            eprintln!("inband: cannot write the audit log: {error}");
        }
    }

    fn refuse(
        &self,
        caller: &Caller,
        from: &str,
        to: &str,
        content: &str,
        error: BridgeError,
    ) -> BridgeError {
        self.audit(
            caller,
            from,
            to,
            content,
            "refused",
            Some(&error.to_string()),
        );
        error
    }

    /// Sends a message after the identity, routing, content and rate checks.
    ///
    /// # Errors
    /// Returns the first failed check. Refusals are written to the audit log.
    pub fn send(
        self: &Arc<Self>,
        caller: &Caller,
        from_raw: &str,
        to_raw: &str,
        content: &str,
    ) -> Result<SendResult, BridgeError> {
        let from = Self::normalize_agent(from_raw, "from")?;
        let requested_to = if to_raw.trim().eq_ignore_ascii_case("all") {
            "all".to_owned()
        } else {
            Self::normalize_agent(to_raw, "to")?
        };
        Self::require_concrete(&from)?;
        if content.is_empty() {
            return Err(BridgeError::Empty);
        }
        if content.len() > self.config.max_message_bytes {
            return Err(BridgeError::TooLarge {
                size: content.len(),
                max: self.config.max_message_bytes,
            });
        }
        if contains_token(content, self.secrets.iter().map(String::as_str)) {
            return Err(self.refuse(
                caller,
                &from,
                &requested_to,
                content,
                BridgeError::ContainsToken,
            ));
        }
        let cleaned = sanitize(content).content;
        if cleaned.trim().is_empty() {
            return Err(BridgeError::Empty);
        }

        self.check_rate(&from)
            .map_err(|error| self.refuse(caller, &from, &requested_to, content, error))?;

        let now = iso_now();
        // One transaction: a concurrent registration must not change the target between check and insert.
        let routed = {
            let mut db = lock(&self.db);
            Self::route_and_insert(&mut db, caller, &from, &requested_to, &cleaned, &now)
        };
        let Routed {
            id,
            resolved_to,
            recipients,
            sender_role,
        } = routed.map_err(|error| self.refuse(caller, &from, &requested_to, content, error))?;
        self.audit(caller, &from, &resolved_to, content, "accepted", None);

        let (channel, waiting): (Vec<bool>, Vec<bool>) = {
            let listeners = lock(&self.listeners);
            recipients
                .iter()
                .map(|r| {
                    (
                        listeners.subscription_matches(r),
                        listeners.waits.contains_key(r),
                    )
                })
                .unzip()
        };
        // No receiver means no listener: the send still succeeded.
        let _ = self.events.send(Arc::new(Delivery {
            id,
            sender: from,
            content: cleaned,
            created_at: now.clone(),
            sender_role,
            recipients: recipients.clone(),
        }));

        let mut notify = BTreeMap::new();
        let mut warnings = Vec::new();
        for (index, recipient) in recipients.iter().enumerate() {
            let state = if channel[index] {
                "pushed-to-channel".to_owned()
            } else if waiting[index] {
                "delivered-to-waiting-agent".to_owned()
            } else {
                self.maybe_wake(recipient)
            };
            notify.insert(recipient.clone(), state);
            if self.presence_of(recipient)? == "offline" {
                warnings.push(format!(
                    "\"{recipient}\" is DISCONNECTED (its session ended). The message is queued and will only be read if that session comes back; do not wait for a reply."
                ));
            }
        }
        Ok(SendResult {
            message_id: id,
            sent_at: now,
            requested_to,
            resolved_to,
            delivered_to: recipients,
            notify,
            warnings,
        })
    }

    /// Checks identity, target and routing, then stores the message, all in one transaction.
    fn route_and_insert(
        db: &mut Connection,
        caller: &Caller,
        from: &str,
        requested_to: &str,
        content: &str,
        now: &str,
    ) -> Result<Routed, BridgeError> {
        let tx = db.transaction()?;
        Self::require_registered_codex(&tx, from)?;
        Self::check_acting(&tx, caller, "from", from)?;
        let resolved_to = if requested_to == CODEX_FAMILY {
            codex_session::most_recent(&tx)?
                .ok_or(BridgeError::NoCodexSession)?
                .mailbox
        } else {
            Self::require_registered_codex(&tx, requested_to)?;
            requested_to.to_owned()
        };
        if resolved_to == from {
            return Err(BridgeError::SelfSend);
        }
        let membership = Self::membership_of(&tx, from)?;
        // The admin token belongs to the user, who is outside the teams.
        if !caller.auth.admin {
            Self::check_routing(&tx, membership.as_ref(), &resolved_to)?;
        }
        if resolved_to != "all" {
            let unread: i64 = tx.query_row(
                "SELECT COUNT(*) FROM deliveries WHERE recipient = ?1 AND read_at IS NULL",
                [&resolved_to],
                |row| row.get(0),
            )?;
            if unread >= MAX_UNREAD_PER_RECIPIENT {
                return Err(BridgeError::RecipientFull(resolved_to));
            }
        }
        let sender_role = membership.as_ref().map_or(Role::Solo, |(_, role)| *role);

        Self::touch_agent_tx(&tx, from)?;
        let recipients: Vec<String> = if resolved_to == "all" {
            // `all` is the sender's team. Only the admin, outside any team, reaches every agent.
            if let Some((team, _)) = &membership {
                let mut statement = tx.prepare(
                    "SELECT mailbox FROM members WHERE team = ?1 AND mailbox != ?2 ORDER BY mailbox",
                )?;
                let names = statement.query_map([team.as_str(), from], |row| row.get(0))?;
                names.collect::<Result<_, _>>()?
            } else {
                let mut statement =
                    tx.prepare("SELECT name FROM agents WHERE name != ?1 ORDER BY name")?;
                let names = statement.query_map([from], |row| row.get(0))?;
                names.collect::<Result<_, _>>()?
            }
        } else {
            tx.execute(
                "INSERT INTO agents(name, first_seen, last_seen) VALUES (?1, ?2, NULL) ON CONFLICT(name) DO NOTHING",
                params![resolved_to, now],
            )?;
            vec![resolved_to.clone()]
        };
        tx.execute(
            "INSERT INTO messages(sender, recipient, content, created_at, sender_role) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![from, resolved_to, content, now, sender_role.as_str()],
        )?;
        let id = tx.last_insert_rowid();
        for recipient in &recipients {
            tx.execute(
                "INSERT INTO deliveries(message_id, recipient, read_at) VALUES (?1, ?2, NULL)",
                params![id, recipient],
            )?;
        }
        tx.commit()?;
        Ok(Routed {
            id,
            resolved_to,
            recipients,
            sender_role: sender_role.as_str().to_owned(),
        })
    }

    fn touch_agent_tx(tx: &rusqlite::Transaction<'_>, name: &str) -> Result<(), BridgeError> {
        let now = iso_now();
        if codex_session::is_canonical_mailbox(name) {
            tx.execute(
                "UPDATE codex_sessions SET last_seen = ?1 WHERE mailbox = ?2",
                params![now, name],
            )?;
            tx.execute(
                "UPDATE agents SET last_seen = ?1 WHERE name = ?2",
                params![now, name],
            )?;
            return Ok(());
        }
        tx.execute(
            "INSERT INTO agents(name, first_seen, last_seen) VALUES (?1, ?2, ?2) ON CONFLICT(name) DO UPDATE SET last_seen = ?2",
            params![name, now],
        )?;
        Ok(())
    }

    /// Mailboxes join and leave teams only through these three calls. The daemon exposes them to
    /// signed requests from the user's own commands, never as MCP tools, so a model cannot call them.
    fn prepare_member(
        &self,
        caller: &Caller,
        mailbox_raw: &str,
    ) -> Result<(String, MutexGuard<'_, Connection>), BridgeError> {
        let mailbox = Self::normalize_agent(mailbox_raw, "mailbox")?;
        Self::require_concrete(&mailbox)?;
        let mut db = lock(&self.db);
        Self::require_registered_codex(&db, &mailbox)?;
        // A team change needs the signed session even for an exact token: the OpenCode model can run
        // its slash commands itself, so only the user's own CLI, with the admin token, moves it.
        if !caller.auth.admin && caller.session.is_none() {
            return Err(BridgeError::SessionRequired(mailbox));
        }
        Self::check_acting(&db, caller, "mailbox", &mailbox)?;
        Self::touch_agent(&mut db, &mailbox)?;
        Ok((mailbox, db))
    }

    /// Daemon notices about team changes. They pass the checks as the admin, since they come from
    /// the daemon itself, and a failed notice never undoes the change.
    fn notify(self: &Arc<Self>, from: &str, to: &str, content: &str) {
        let daemon = Caller {
            auth: AuthInfo::disabled(),
            session: None,
        };
        if let Err(error) = self.send(&daemon, from, to, content) {
            eprintln!("inband: could not notify {to}: {error}");
        }
    }

    /// Makes `mailbox_raw` the lead of `team_raw`, and creates the team when it does not exist.
    /// The previous lead of that team becomes a worker and gets a notice.
    ///
    /// # Errors
    /// Returns an error for an invalid name, a mailbox bound to another session, or SQLite.
    pub fn set_lead(
        self: &Arc<Self>,
        caller: &Caller,
        mailbox_raw: &str,
        team_raw: &str,
    ) -> Result<TeamChange, BridgeError> {
        let team = Self::normalize_agent(team_raw, "team")?;
        let (mailbox, mut db) = self.prepare_member(caller, mailbox_raw)?;
        let tx = db.transaction()?;
        let previous_team = Self::membership_of(&tx, &mailbox)?.map(|(team, _)| team);
        let replaced_lead = Self::lead_of_team(&tx, &team)?.filter(|lead| *lead != mailbox);
        if let Some(lead) = &replaced_lead {
            tx.execute(
                "UPDATE members SET role = 'worker' WHERE mailbox = ?1",
                [lead],
            )?;
        }
        tx.execute(
            "INSERT INTO members(mailbox, team, role, joined_at) VALUES (?1, ?2, 'lead', ?3)
             ON CONFLICT(mailbox) DO UPDATE SET team = ?2, role = 'lead', joined_at = ?3",
            params![mailbox, team, iso_now()],
        )?;
        tx.commit()?;
        drop(db);
        if let Some(lead) = &replaced_lead {
            self.notify(
                &mailbox,
                lead,
                &format!("{mailbox} is now the lead of team {team}. You are a worker of this team from now on."),
            );
        }
        Ok(TeamChange {
            mailbox,
            previous_team: previous_team.filter(|previous| *previous != team),
            team: Some(team),
            role: Role::Lead.as_str(),
            replaced_lead,
        })
    }

    /// Adds `mailbox_raw` to `team_raw` as a worker. The lead of the team gets a notice.
    ///
    /// # Errors
    /// Returns an error for an invalid name, a mailbox bound to another session, or SQLite.
    pub fn join(
        self: &Arc<Self>,
        caller: &Caller,
        mailbox_raw: &str,
        team_raw: &str,
    ) -> Result<TeamChange, BridgeError> {
        let team = Self::normalize_agent(team_raw, "team")?;
        let (mailbox, db) = self.prepare_member(caller, mailbox_raw)?;
        let previous_team = Self::membership_of(&db, &mailbox)?.map(|(team, _)| team);
        db.execute(
            "INSERT INTO members(mailbox, team, role, joined_at) VALUES (?1, ?2, 'worker', ?3)
             ON CONFLICT(mailbox) DO UPDATE SET team = ?2, role = 'worker', joined_at = ?3",
            params![mailbox, team, iso_now()],
        )?;
        let lead = Self::lead_of_team(&db, &team)?;
        drop(db);
        if let Some(lead) = lead {
            self.notify(
                &mailbox,
                &lead,
                &format!("{mailbox} joined team {team} as a worker."),
            );
        }
        Ok(TeamChange {
            mailbox,
            previous_team: previous_team.filter(|previous| *previous != team),
            team: Some(team),
            role: Role::Worker.as_str(),
            replaced_lead: None,
        })
    }

    /// Removes `mailbox_raw` from its team: the session becomes solo. The lead gets a notice.
    ///
    /// # Errors
    /// Returns an error for an invalid name, a mailbox bound to another session, or SQLite.
    pub fn leave(
        self: &Arc<Self>,
        caller: &Caller,
        mailbox_raw: &str,
    ) -> Result<TeamChange, BridgeError> {
        let (mailbox, db) = self.prepare_member(caller, mailbox_raw)?;
        let previous = Self::membership_of(&db, &mailbox)?;
        db.execute("DELETE FROM members WHERE mailbox = ?1", [&mailbox])?;
        let lead = match &previous {
            Some((team, Role::Worker)) => Self::lead_of_team(&db, team)?,
            _ => None,
        };
        drop(db);
        if let (Some(lead), Some((team, _))) = (lead, &previous) {
            self.notify(&mailbox, &lead, &format!("{mailbox} left team {team}."));
        }
        Ok(TeamChange {
            mailbox,
            previous_team: previous.map(|(team, _)| team),
            team: None,
            role: Role::Solo.as_str(),
            replaced_lead: None,
        })
    }

    /// Unread mail without marking it read.
    ///
    /// # Errors
    /// Returns an error for an invalid name, an unregistered Codex mailbox, or SQLite.
    pub fn peek_unread(&self, for_raw: &str) -> Result<Vec<MessageRow>, BridgeError> {
        let recipient = Self::normalize_agent(for_raw, "for")?;
        let mut db = lock(&self.db);
        Self::touch_agent(&mut db, &recipient)?;
        Ok(message_rows(&db, UNREAD_SQL, [&recipient])?)
    }

    /// Unread mail, marked read. The only call that consumes mail.
    ///
    /// # Errors
    /// Returns an error for an invalid name, an unregistered Codex mailbox, or SQLite.
    pub fn fetch_unread(&self, for_raw: &str) -> Result<Vec<MessageRow>, BridgeError> {
        let recipient = Self::normalize_agent(for_raw, "for")?;
        let rows = {
            let mut db = lock(&self.db);
            Self::touch_agent(&mut db, &recipient)?;
            let tx = db.transaction()?;
            let rows = message_rows(&tx, UNREAD_SQL, [&recipient])?;
            if !rows.is_empty() {
                tx.execute(
                    "UPDATE deliveries SET read_at = ?1 WHERE recipient = ?2 AND read_at IS NULL",
                    params![iso_now(), recipient],
                )?;
            }
            tx.commit()?;
            rows
        };
        if codex_session::is_canonical_mailbox(&recipient) && self.unread_count(&recipient)? == 0 {
            self.cancel_retry(&recipient);
        }
        Ok(rows)
    }

    fn unread_count(&self, recipient: &str) -> Result<i64, BridgeError> {
        Ok(lock(&self.db).query_row(
            "SELECT COUNT(*) FROM deliveries WHERE recipient = ?1 AND read_at IS NULL",
            [recipient],
            |row| row.get(0),
        )?)
    }

    /// Blocks until mail arrives or the timeout ends, then returns a preview of the unread mail.
    ///
    /// # Errors
    /// Returns an error for an invalid name, the codex alias, too many waits, or SQLite.
    pub async fn wait_for_messages(
        &self,
        for_raw: &str,
        timeout_seconds: u64,
        long_wait: bool,
    ) -> Result<Vec<MessageRow>, BridgeError> {
        let recipient = Self::normalize_agent(for_raw, "for")?;
        Self::require_concrete(&recipient)?;
        let cap = if long_wait {
            MAX_LONG_WAIT_SECONDS
        } else {
            MAX_WAIT_SECONDS
        };
        let timeout = Duration::from_secs(timeout_seconds.clamp(5, cap));
        let mut events = self.events.subscribe();
        let immediate = self.peek_unread(&recipient)?;
        if !immediate.is_empty() {
            return Ok(immediate);
        }
        let _guard = {
            let mut listeners = lock(&self.listeners);
            let count = listeners.waits.entry(recipient.clone()).or_default();
            if *count >= MAX_PENDING_WAITS_PER_AGENT {
                return Err(BridgeError::TooManyWaits(recipient));
            }
            *count += 1;
            WaitGuard {
                bridge: self,
                mailbox: recipient.clone(),
            }
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(delivery)) if !delivery.recipients.contains(&recipient) => {}
                // A matching delivery, a lagged receiver, a closed channel or the timeout: re-read.
                _ => break,
            }
        }
        self.peek_unread(&recipient)
    }

    /// Long poll for one exact mailbox, used by the channel shim.
    ///
    /// # Errors
    /// See [`Self::subscribe`].
    pub async fn subscribe_mailbox(
        &self,
        mailbox_raw: &str,
        timeout_seconds: u64,
        after_id: Option<i64>,
    ) -> Result<Vec<MessageRow>, BridgeError> {
        let mailbox = Self::normalize_agent(mailbox_raw, "mailbox")?;
        {
            let mut db = lock(&self.db);
            Self::touch_agent(&mut db, &mailbox)?;
        }
        self.subscribe(&mailbox, true, timeout_seconds, after_id)
            .await
    }

    /// Long poll for a whole `prefix-*` family.
    ///
    /// # Errors
    /// See [`Self::subscribe`].
    pub async fn subscribe_family(
        &self,
        prefix_raw: &str,
        timeout_seconds: u64,
        after_id: Option<i64>,
    ) -> Result<Vec<MessageRow>, BridgeError> {
        let prefix = Self::normalize_agent(prefix_raw, "prefix")?;
        self.subscribe(&prefix, false, timeout_seconds, after_id)
            .await
    }

    /// Returns unread mail after `after_id` at once, otherwise waits for the next delivery.
    /// Old shims send no cursor and only receive live mail.
    ///
    /// # Errors
    /// Returns an error for too many subscriptions or SQLite.
    async fn subscribe(
        &self,
        prefix: &str,
        exact: bool,
        timeout_seconds: u64,
        after_id: Option<i64>,
    ) -> Result<Vec<MessageRow>, BridgeError> {
        let mut events = self.events.subscribe();
        if let Some(after_id) = after_id {
            let queued = message_rows(
                &lock(&self.db),
                "SELECT m.id, m.sender, d.recipient, m.content, m.created_at, m.sender_role
                 FROM deliveries d JOIN messages m ON m.id = d.message_id
                 WHERE d.read_at IS NULL AND m.id > ?1
                   AND (d.recipient = ?2 OR (?3 = 0 AND substr(d.recipient, 1, length(?2) + 1) = ?2 || '-'))
                 ORDER BY m.id ASC, d.recipient ASC",
                params![after_id, prefix, i64::from(!exact)],
            )?;
            if !queued.is_empty() {
                return Ok(queued);
            }
        }
        let _guard = {
            let mut listeners = lock(&self.listeners);
            let same = listeners
                .subscriptions
                .iter()
                .filter(|(_, p, e)| p == prefix && *e == exact)
                .count();
            if same >= MAX_PENDING_SUBSCRIPTIONS_PER_TARGET {
                return Err(BridgeError::TooManySubscriptions(prefix.to_owned()));
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            listeners.subscriptions.push((id, prefix.to_owned(), exact));
            SubscriptionGuard { bridge: self, id }
        };
        let floor = after_id.unwrap_or(0);
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(timeout_seconds.clamp(1, MAX_SUBSCRIBE_SECONDS));
        loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(delivery)) => {
                    if delivery.id <= floor {
                        continue;
                    }
                    let rows: Vec<MessageRow> = delivery
                        .recipients
                        .iter()
                        .filter(|r| target_matches(r, prefix, exact))
                        .map(|recipient| MessageRow {
                            id: delivery.id,
                            sender: delivery.sender.clone(),
                            recipient: recipient.clone(),
                            content: delivery.content.clone(),
                            created_at: delivery.created_at.clone(),
                            sender_role: Some(delivery.sender_role.clone()),
                        })
                        .collect();
                    if !rows.is_empty() {
                        return Ok(rows);
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return Ok(Vec::new()),
            }
        }
    }

    /// Records the online or offline state that the hooks report.
    ///
    /// # Errors
    /// Returns an error for an invalid name, the codex alias, an unregistered Codex mailbox, or SQLite.
    pub fn set_presence(&self, name_raw: &str, online: bool) -> Result<(), BridgeError> {
        let name = Self::normalize_agent(name_raw, "agent")?;
        Self::require_concrete(&name)?;
        let now = iso_now();
        let mut db = lock(&self.db);
        if codex_session::is_canonical_mailbox(&name) {
            Self::require_registered_codex(&db, &name)?;
            codex_session::touch(&mut db, &name, None)?;
            db.execute(
                "UPDATE agents SET online = ?1, presence_at = ?2 WHERE name = ?3",
                params![i64::from(online), now, name],
            )?;
            return Ok(());
        }
        db.execute(
            "INSERT INTO agents(name, first_seen, last_seen, online, presence_at) VALUES (?1, ?2, ?2, ?3, ?2)
             ON CONFLICT(name) DO UPDATE SET online = ?3, presence_at = ?2, last_seen = ?2",
            params![name, now, i64::from(online)],
        )?;
        Ok(())
    }

    fn presence_of(&self, name: &str) -> Result<&'static str, BridgeError> {
        if lock(&self.listeners).waits.contains_key(name) {
            return Ok("online");
        }
        let row: Option<(i64, Option<String>)> = lock(&self.db)
            .query_row(
                "SELECT online, presence_at FROM agents WHERE name = ?1",
                [name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((online, Some(_))) => {
                if online == 0 {
                    "offline"
                } else {
                    "online"
                }
            }
            _ => "unknown",
        })
    }

    /// Past messages, oldest first, filtered to the visible mailboxes.
    ///
    /// # Errors
    /// Returns SQLite errors.
    pub fn history(
        &self,
        limit: u32,
        before_id: Option<i64>,
        visible: Option<&[String]>,
    ) -> Result<History, BridgeError> {
        let capped = i64::from(limit.clamp(1, MAX_HISTORY));
        let db = lock(&self.db);
        let rows = match before_id {
            Some(before) => message_rows(
                &db,
                "SELECT id, sender, recipient, content, created_at, sender_role FROM messages WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
                params![before, capped],
            )?,
            None => message_rows(
                &db,
                "SELECT id, sender, recipient, content, created_at, sender_role FROM messages ORDER BY id DESC LIMIT ?1",
                params![capped],
            )?,
        };
        let is_visible = |sender: &str, recipient: &str| {
            visible.is_none_or(|patterns| {
                patterns.iter().any(|p| {
                    agent_matches_pattern(sender, p) || agent_matches_pattern(recipient, p)
                })
            })
        };
        let mut messages: Vec<MessageRow> = rows
            .into_iter()
            .filter(|row| is_visible(&row.sender, &row.recipient))
            .collect();
        messages.reverse();
        let mut statement = db.prepare("SELECT sender, recipient FROM messages")?;
        let pairs = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut total = 0;
        for pair in pairs {
            let (sender, recipient) = pair?;
            if is_visible(&sender, &recipient) {
                total += 1;
            }
        }
        Ok(History { messages, total })
    }

    fn all_members(db: &Connection) -> rusqlite::Result<HashMap<String, (String, Role)>> {
        let mut statement = db.prepare("SELECT mailbox, team, role FROM members")?;
        let rows = statement.query_map([], |row| {
            let role: String = row.get(2)?;
            let role = if role == "lead" {
                Role::Lead
            } else {
                Role::Worker
            };
            Ok((row.get::<_, String>(0)?, (row.get::<_, String>(1)?, role)))
        })?;
        rows.collect()
    }

    /// Agents, presence, roles, unread counts and the last wakes.
    ///
    /// With a `viewer`, the list holds only the viewer's team, or only the viewer when it is solo.
    /// Without one (the admin view), it holds every agent.
    ///
    /// # Errors
    /// Returns SQLite errors.
    pub fn status(
        &self,
        viewer: Option<&str>,
        visible: Option<&[String]>,
        wake_visible: Option<&[String]>,
    ) -> Result<Status, BridgeError> {
        let (members, names) = {
            let db = lock(&self.db);
            let members = Self::all_members(&db)?;
            let mut statement =
                db.prepare("SELECT name, first_seen, last_seen FROM agents ORDER BY name")?;
            let rows =
                statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            let names: Vec<AgentRow> = rows.collect::<Result<_, _>>()?;
            (members, names)
        };
        let team = viewer
            .and_then(|v| members.get(v))
            .map(|(team, _)| team.clone());
        let lead = team.as_ref().and_then(|team| {
            members
                .iter()
                .find(|(_, (t, role))| t == team && *role == Role::Lead)
                .map(|(mailbox, _)| mailbox.clone())
        });
        let now = SystemTime::now();
        let mut agents = Vec::new();
        for (name, first_seen, last_seen) in names {
            let membership = members.get(&name);
            if let Some(viewer) = viewer {
                let same_team = team
                    .as_ref()
                    .is_some_and(|team| membership.is_some_and(|(t, _)| t == team));
                if name != viewer && !same_team {
                    continue;
                }
            }
            if visible
                .is_some_and(|patterns| !patterns.iter().any(|p| agent_matches_pattern(&name, p)))
            {
                continue;
            }
            let presence = self.presence_of(&name)?;
            let waiting_now = lock(&self.listeners).waits.contains_key(&name);
            let idle_seconds = last_seen
                .as_deref()
                .and_then(|seen| humantime::parse_rfc3339(seen).ok())
                .and_then(|seen| now.duration_since(seen).ok())
                .map(|idle| idle.as_secs());
            let connected = if presence == "online"
                && !waiting_now
                && idle_seconds.is_some_and(|s| s > STALE_AFTER_SECONDS)
            {
                "stale (online but idle >30min - possible crash)".to_owned()
            } else {
                presence.to_owned()
            };
            let (codex, unread) = {
                let db = lock(&self.db);
                let codex = codex_session::by_mailbox(&db, &name)?;
                let unread: i64 = db.query_row(
                    "SELECT COUNT(*) FROM deliveries WHERE recipient = ?1 AND read_at IS NULL",
                    [&name],
                    |row| row.get(0),
                )?;
                (codex, unread)
            };
            agents.push(AgentStatus {
                team: membership.map(|(team, _)| team.clone()),
                role: membership.map_or(Role::Solo, |(_, role)| *role).as_str(),
                display_label: codex.as_ref().map(|c| c.display_label.clone()),
                cwd: codex.as_ref().map(|c| c.cwd.clone()),
                lifecycle: codex.map(|c| c.lifecycle),
                name,
                first_seen,
                last_seen,
                connected,
                idle_seconds,
                waiting_now,
                unread,
            });
        }
        let last_wakes = self.last_wakes(viewer.is_some(), &agents, wake_visible)?;
        Ok(Status {
            daemon: "inband",
            started_at: self.started_at.clone(),
            team,
            lead,
            agents,
            last_wakes,
        })
    }

    /// The last five wakes. A team view keeps only the wakes of its listed agents.
    fn last_wakes(
        &self,
        team_view: bool,
        agents: &[AgentStatus],
        wake_visible: Option<&[String]>,
    ) -> Result<Vec<WakeRecord>, BridgeError> {
        let wakes = {
            let db = lock(&self.db);
            let mut statement = db.prepare(
                "SELECT recipient, created_at, ok, detail FROM wakes ORDER BY id DESC LIMIT 5",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(WakeRecord {
                    recipient: row.get(0)?,
                    created_at: row.get(1)?,
                    ok: row.get(2)?,
                    detail: row.get(3)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|wake| {
                    !team_view || agents.iter().any(|agent| agent.name == wake.recipient)
                })
                .filter(|wake| {
                    wake_visible.is_none_or(|patterns| {
                        patterns
                            .iter()
                            .any(|p| agent_matches_pattern(&wake.recipient, p))
                    })
                })
                .collect()
        };
        Ok(wakes)
    }

    /// Deletes all messages and deliveries. The audit log stays.
    ///
    /// # Errors
    /// Returns an error without `confirm == "wipe"`, or SQLite.
    pub fn clear(&self, confirm: &str) -> Result<usize, BridgeError> {
        if confirm != "wipe" {
            return Err(BridgeError::ClearNotConfirmed);
        }
        let db = lock(&self.db);
        let count: i64 = db.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
        db.execute_batch("DELETE FROM deliveries; DELETE FROM messages;")?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    fn wake_suppression(
        &self,
        recipient: &str,
        target: &WakeTarget,
    ) -> Result<Option<String>, BridgeError> {
        let common = target.common();
        let now = SystemTime::now();
        let hour_ago = iso(now - Duration::from_hours(1));
        let db = lock(&self.db);
        let last_hour: i64 = db.query_row(
            "SELECT COUNT(*) FROM wakes WHERE recipient = ?1 AND created_at > ?2",
            params![recipient, hour_ago],
            |row| row.get(0),
        )?;
        if last_hour >= i64::from(common.max_wakes_per_hour) {
            return Ok(Some(format!(
                "wake-suppressed: {last_hour} wakes in the last hour (cap {})",
                common.max_wakes_per_hour
            )));
        }
        let cutoff = iso(now - Duration::from_secs(u64::from(common.debounce_seconds)));
        let recent: i64 = db.query_row(
            "SELECT COUNT(*) FROM wakes WHERE recipient = ?1 AND created_at > ?2 AND ok = 1",
            params![recipient, cutoff],
            |row| row.get(0),
        )?;
        Ok((recent > 0).then(|| {
            format!(
                "wake-debounced (last wake < {}s ago)",
                common.debounce_seconds
            )
        }))
    }

    fn maybe_wake(self: &Arc<Self>, recipient: &str) -> String {
        let is_codex = codex_session::is_canonical_mailbox(recipient);
        let key = if is_codex { CODEX_FAMILY } else { recipient };
        let Some(target) = self.config.wake.get(key).cloned() else {
            return "no-wake-configured".to_owned();
        };
        match (&target, is_codex) {
            (WakeTarget::Codex { .. }, true) | (WakeTarget::Opencode { .. }, false) => {}
            (_, true) => return "wake-misconfigured: codex target required".to_owned(),
            (_, false) => return "wake-misconfigured: opencode target required".to_owned(),
        }
        match self.wake_suppression(recipient, &target) {
            Ok(Some(reason)) => return reason,
            Ok(None) => {}
            Err(error) => return format!("wake-failed: {error}"),
        }
        if is_codex {
            let generation = self.next_id.fetch_add(1, Ordering::Relaxed);
            {
                let mut retries = lock(&self.retries);
                if retries.contains_key(recipient) {
                    return "wake-retry-pending".to_owned();
                }
                retries.insert(
                    recipient.to_owned(),
                    RetryState {
                        generation,
                        task: None,
                    },
                );
            }
            let bridge = Arc::clone(self);
            let mailbox = recipient.to_owned();
            let handle =
                tokio::spawn(
                    async move { bridge.run_codex_wake(mailbox, target, generation).await },
                );
            if let Some(state) = lock(&self.retries)
                .get_mut(recipient)
                .filter(|s| s.generation == generation)
            {
                state.task = Some(handle);
            }
            return "wake-dispatched".to_owned();
        }
        let bridge = Arc::clone(self);
        let mailbox = recipient.to_owned();
        tokio::spawn(async move {
            let input = WakeInput {
                recipient: mailbox.clone(),
                session_id: None,
                mailbox: None,
                prompt: target.common().prompt.clone(),
            };
            let result = bridge.wake.dispatch(&target, input).await;
            bridge.record_wake(&mailbox, &result);
        });
        "wake-dispatched".to_owned()
    }

    fn retry_current(&self, mailbox: &str, generation: u64) -> bool {
        lock(&self.retries)
            .get(mailbox)
            .is_some_and(|state| state.generation == generation)
    }

    fn finish_retry(&self, mailbox: &str, generation: u64) {
        let mut retries = lock(&self.retries);
        if retries
            .get(mailbox)
            .is_some_and(|state| state.generation == generation)
        {
            retries.remove(mailbox);
        }
    }

    async fn run_codex_wake(self: Arc<Self>, mailbox: String, target: WakeTarget, generation: u64) {
        let WakeTarget::Codex {
            retry_delays_seconds,
            common,
            ..
        } = &target
        else {
            self.finish_retry(&mailbox, generation);
            return;
        };
        let mut next_delay = 0;
        loop {
            if !self.retry_current(&mailbox, generation) {
                return;
            }
            let pending = self.unread_count(&mailbox).unwrap_or(0);
            let suppressed = self.wake_suppression(&mailbox, &target).unwrap_or(None);
            let session = codex_session::by_mailbox(&lock(&self.db), &mailbox)
                .ok()
                .flatten();
            let (true, None, Some(session)) = (pending > 0, suppressed, session) else {
                self.finish_retry(&mailbox, generation);
                return;
            };
            let input = WakeInput {
                recipient: mailbox.clone(),
                session_id: Some(session.session_id),
                mailbox: Some(session.mailbox),
                prompt: common.prompt.clone(),
            };
            let result = self.wake.dispatch(&target, input).await;
            self.record_wake(&mailbox, &result);
            if !self.retry_current(&mailbox, generation) {
                return;
            }
            let done =
                result.disposition.is_success() || self.unread_count(&mailbox).unwrap_or(0) == 0;
            let Some(delay) = retry_delays_seconds.get(next_delay).filter(|_| !done) else {
                self.finish_retry(&mailbox, generation);
                return;
            };
            next_delay += 1;
            tokio::time::sleep(Duration::from_secs(u64::from(*delay))).await;
        }
    }

    fn cancel_retry(&self, mailbox: &str) {
        if let Some(state) = lock(&self.retries).remove(mailbox)
            && let Some(task) = state.task
        {
            task.abort();
        }
    }

    fn record_wake(&self, recipient: &str, result: &WakeResult) {
        let ok = result.disposition.is_success();
        let detail = format!("{}: {}", result.disposition.as_str(), result.detail);
        let write = lock(&self.db).execute(
            "INSERT INTO wakes(recipient, created_at, ok, detail) VALUES (?1, ?2, ?3, ?4)",
            params![recipient, iso_now(), i64::from(ok), detail],
        );
        if let Err(error) = write {
            eprintln!("inband: cannot record the wake of {recipient}: {error}");
        }
        eprintln!(
            "[wake] {recipient}: {} - {detail}",
            if ok { "OK" } else { "FAIL" }
        );
    }

    /// Wakes the Codex sessions that still have unread mail. Called once at startup.
    ///
    /// # Errors
    /// Returns SQLite errors.
    pub fn reconcile_codex_wakes(self: &Arc<Self>) -> Result<(), BridgeError> {
        let sessions = codex_session::list_with_unread(&lock(&self.db))?;
        for (session, unread) in sessions {
            if unread > 0 {
                self.maybe_wake(&session.mailbox);
            }
        }
        Ok(())
    }

    /// Registers a Codex session, for the Codex hook.
    ///
    /// # Errors
    /// Returns an error for an invalid session id or SQLite.
    pub fn register_codex(
        &self,
        session_id: &str,
        cwd: &str,
        lifecycle: &str,
    ) -> Result<codex_session::CodexSession, BridgeError> {
        Ok(codex_session::register(
            &mut lock(&self.db),
            session_id,
            cwd,
            lifecycle,
        )?)
    }

    /// # Errors
    /// Returns SQLite errors.
    pub fn touch_codex(&self, mailbox: &str, lifecycle: Option<&str>) -> Result<(), BridgeError> {
        Ok(codex_session::touch(
            &mut lock(&self.db),
            mailbox,
            lifecycle,
        )?)
    }

    /// # Errors
    /// Returns an error for an invalid session id or SQLite.
    pub fn codex_by_session_id(
        &self,
        session_id: &str,
    ) -> Result<Option<codex_session::CodexSession>, BridgeError> {
        Ok(codex_session::by_session_id(&lock(&self.db), session_id)?)
    }
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod tests;
