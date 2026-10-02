//! The message bus: stores, routes and delivers the mail, and does all the security checks.
//!
//! Each operation receives a [`Caller`]: the authenticated client, and the session that signed the
//! request. The bus then checks three things:
//!
//! - identity: a caller acts only for the mailbox of its own session;
//! - routing: a worker writes only to its lead, and no mail leaves a team;
//! - content: the bus cleans the text, refuses tokens, and limits the rate and the size.
//!
//! The HTTP and MCP layers only translate requests: all the rules are here.

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
use crate::opencode_session;
use crate::protocol::Role;
use crate::sanitize::{contains_token, sanitize};
use crate::wake::{WakeDispatch, WakeInput, WakeResult};

const MAX_PENDING_WAITS_PER_AGENT: usize = 8;
const MAX_PENDING_SUBSCRIPTIONS_PER_TARGET: usize = 8;
/// The longest wait for a client without a progress token. Such a client stops a request after 60
/// s.
const MAX_WAIT_SECONDS: u64 = 50;
const MAX_LONG_WAIT_SECONDS: u64 = 1800;
const MAX_SUBSCRIBE_SECONDS: u64 = 300;
const MAX_HISTORY: u32 = 500;
/// The maximum number of messages that one sender sends in one minute.
const SEND_RATE_PER_MINUTE: usize = 30;
/// The number of unread messages after which a mailbox refuses new mail.
const MAX_UNREAD_PER_RECIPIENT: i64 = 200;
/// The idle time after which `ping` shows an online agent as stale.
const STALE_AFTER_SECONDS: u64 = 1800;
/// The SQL condition for the mail that the viewer `?1` sent or received. A NULL `?1` selects all
/// messages.
const VIEWER_MAIL_SQL: &str = "(?1 IS NULL OR m.sender = ?1
     OR EXISTS (SELECT 1 FROM deliveries d WHERE d.message_id = m.id AND d.recipient = ?1))";

/// The `name`, `first_seen` and `last_seen` columns of the `agents` table.
type AgentRow = (String, String, Option<String>);

/// The reasons to refuse an operation. The text goes back to the agent.
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
    #[error("pass your own mailbox: only the admin token sees every agent")]
    ViewerRequired,
    #[error("this needs the admin token")]
    AdminRequired,
    #[error("refusing to clear: pass confirm=\"wipe\" to delete all messages")]
    ClearNotConfirmed,
    #[error(transparent)]
    Codex(#[from] CodexSessionError),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

/// The caller of an operation: the authenticated client, and its session.
#[derive(Debug, Clone)]
pub struct Caller {
    pub auth: AuthInfo,
    /// Set only when the request is signed for one session (the channel shim or a hook).
    pub session: Option<String>,
}

/// One message, as an agent receives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageRow {
    pub id: i64,
    pub sender: String,
    pub recipient: String,
    pub content: String,
    pub created_at: String,
    /// The role of the sender when it sent the message. The daemon sets it, never the sender.
    pub sender_role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendResult {
    pub message_id: i64,
    pub sent_at: String,
    /// The recipient in the request, for example `codex` or `all`.
    pub requested_to: String,
    /// The recipient after the alias: a mailbox, or `all`.
    pub resolved_to: String,
    /// The mailboxes that received the message.
    pub delivered_to: Vec<String>,
    /// For each recipient: how the daemon told it, for example `pushed-to-channel` or
    /// `wake-dispatched`.
    pub notify: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// One agent, as `ping` shows it.
///
/// Each agent has all the fields, null when empty: the rows then have one form, and `ping` shows
/// them as one TOON table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStatus {
    pub name: String,
    pub first_seen: String,
    pub last_seen: Option<String>,
    /// The short name of a Codex session.
    pub display_label: Option<String>,
    pub cwd: Option<String>,
    /// `active` or `idle`, for a Codex session.
    pub lifecycle: Option<String>,
    pub team: Option<String>,
    pub role: &'static str,
    /// `online`, `offline`, `unknown`, or `stale` for an online agent without activity for 30
    /// minutes.
    pub connected: String,
    /// The time since the last activity.
    pub idle_seconds: Option<u64>,
    /// True when the agent waits for mail now.
    pub waiting_now: bool,
    pub unread: i64,
}

/// One wake attempt, as `ping` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WakeRecord {
    pub recipient: String,
    pub created_at: String,
    /// 1 when the wake reached the client, else 0.
    pub ok: i64,
    pub detail: Option<String>,
}

/// The result of `ping`.
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
    /// The number of messages that the caller can see, in all pages.
    pub total: usize,
}

/// The result of a team command.
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

/// The position of a mailbox: the input of its protocol text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionContext {
    pub mailbox: String,
    pub role: Role,
    pub team: Option<String>,
    pub lead: Option<String>,
}

impl SessionContext {
    #[must_use]
    pub fn protocol(&self) -> String {
        crate::protocol::protocol_text(
            self.role,
            &self.mailbox,
            self.team.as_deref(),
            self.lead.as_deref(),
        )
    }
}

/// A new message, as the bus gives it to the waits and the long polls.
#[derive(Debug, Clone)]
struct Delivery {
    id: i64,
    sender: String,
    content: String,
    created_at: String,
    sender_role: String,
    /// The mailboxes that received the message.
    recipients: Vec<String>,
}

/// The waits and the long polls in progress.
#[derive(Debug, Default)]
struct Listeners {
    /// The number of waits for each mailbox.
    waits: HashMap<String, usize>,
    /// The long polls: an id, the mailbox or family prefix, and true for one exact mailbox.
    subscriptions: Vec<(u64, String, bool)>,
}

impl Listeners {
    fn subscription_matches(&self, recipient: &str) -> bool {
        self.subscriptions
            .iter()
            .any(|(_, prefix, exact)| target_matches(recipient, prefix, *exact))
    }
}

/// Returns `true` when `recipient` is `prefix`, or, for a family poll (`exact` false), a mailbox of
/// the family `prefix-*`.
fn target_matches(recipient: &str, prefix: &str, exact: bool) -> bool {
    recipient == prefix
        || (!exact
            && recipient
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('-')))
}

/// A stored message and its recipients.
struct Routed {
    id: i64,
    resolved_to: String,
    recipients: Vec<String>,
    sender_role: String,
}

/// The Codex wake in progress for one mailbox.
///
/// Each wake gets a new generation. A task whose generation is old stops.
struct RetryState {
    generation: u64,
    /// The task that sends the wakes, to stop it.
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Locks a mutex, also after a panic of another thread: the data stays valid, so the daemon
/// continues.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The message bus.
pub struct Bridge {
    db: Mutex<Connection>,
    config: BridgeConfig,
    /// The tokens of all clients. A message that contains one is refused.
    secrets: Vec<String>,
    wake: Arc<dyn WakeDispatch>,
    /// Each new message goes to the waits and the long polls through this channel.
    events: broadcast::Sender<Arc<Delivery>>,
    listeners: Mutex<Listeners>,
    /// The send times of the last minute, for each sender.
    send_times: Mutex<HashMap<String, VecDeque<Instant>>>,
    /// The Codex wake in progress, for each mailbox.
    retries: Mutex<HashMap<String, RetryState>>,
    /// The source of the ids of the long polls and of the wake generations.
    next_id: AtomicU64,
    started_at: String,
}

/// Decreases the wait count of a mailbox when a wait ends, also when the client disconnects.
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

/// Removes a long poll from the list when it ends.
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
    /// Creates the bus. `secrets` are the tokens of all clients: a message that contains one is
    /// refused.
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

    /// Returns a mailbox name in lower case.
    ///
    /// # Errors
    ///
    /// Returns [`BridgeError::InvalidName`] for a name that is not 1 to 64 chars of `[a-z0-9_-]`.
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

    /// Refuses an alias (`codex` or `all`) as the identity of a sender.
    fn require_concrete(name: &str) -> Result<(), BridgeError> {
        if name == CODEX_FAMILY || name == "all" {
            Err(BridgeError::AliasIdentity(name.to_owned()))
        } else {
            Ok(())
        }
    }

    /// Refuses a `codex-<uuid>` mailbox that no Codex hook registered.
    fn require_registered_codex(db: &Connection, name: &str) -> Result<(), BridgeError> {
        if codex_session::is_canonical_mailbox(name)
            && codex_session::by_mailbox(db, name)?.is_none()
        {
            return Err(BridgeError::CodexNotRegistered(name.to_owned()));
        }
        Ok(())
    }

    /// Adds a mailbox to the agents, or updates its last-seen time.
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

    /// Returns the team and the role of a mailbox, or `None` for a solo mailbox.
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

    /// Returns the team and the role of a mailbox, or `None` for a solo mailbox.
    ///
    /// # Errors
    ///
    /// Returns an error when the database fails.
    pub fn membership(&self, name: &str) -> Result<Option<(String, Role)>, BridgeError> {
        Ok(Self::membership_of(&lock(&self.db), name)?)
    }

    /// Returns the lead of a team, or `None` when it has none.
    ///
    /// # Errors
    ///
    /// Returns an error when the database fails.
    pub fn team_lead(&self, team: &str) -> Result<Option<String>, BridgeError> {
        Ok(Self::lead_of_team(&lock(&self.db), team)?)
    }

    /// Returns the role of a mailbox. A mailbox in no team is solo.
    ///
    /// # Errors
    ///
    /// Returns an error when the database fails.
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

    /// Binds a mailbox to the session that signed the request. The signed `SessionStart` hook calls
    /// it.
    ///
    /// A binding never moves to another session. Else a session could take the mailbox of another
    /// session, the lead included.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is not valid, the token cannot use the mailbox, the request
    /// has no session, the mailbox has another session, or the database fails.
    pub fn bind_session(&self, caller: &Caller, mailbox_raw: &str) -> Result<(), BridgeError> {
        let mailbox = Self::normalize_agent(mailbox_raw, "mailbox")?;
        Self::require_concrete(&mailbox)?;
        Self::require_token_scope(caller, "mailbox", &mailbox)?;
        let session_key = caller
            .session
            .clone()
            .ok_or_else(|| BridgeError::SessionRequired(mailbox.clone()))?;
        if Self::session_owned(&mailbox) {
            Self::require_owner_session(caller, &mailbox)?;
        }
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

    /// Returns `true` when the name of a mailbox tells its session: `codex-<session uuid>`, or
    /// `opencode-<digest of the session id>`.
    fn session_owned(mailbox: &str) -> bool {
        codex_session::is_canonical_mailbox(mailbox)
            || opencode_session::is_session_mailbox(mailbox)
    }

    fn session_owns(session: &str, mailbox: &str) -> bool {
        match mailbox.strip_prefix("codex-") {
            Some(uuid) if codex_session::is_canonical_mailbox(mailbox) => {
                session.eq_ignore_ascii_case(uuid)
            }
            _ => opencode_session::owns(session, mailbox),
        }
    }

    /// Makes sure that a request for a session-owned mailbox comes from that session.
    ///
    /// Codex writes the session in the `_meta` of each tool call, and `OpenCode` gives it to the
    /// tools of the InBand plugin. The model cannot write either value.
    fn require_owner_session(caller: &Caller, mailbox: &str) -> Result<(), BridgeError> {
        match caller.session.as_deref() {
            Some(session) if Self::session_owns(session, mailbox) => Ok(()),
            Some(_) => Err(BridgeError::BoundToOtherSession(mailbox.to_owned())),
            None => Err(BridgeError::SessionRequired(mailbox.to_owned())),
        }
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

    /// Makes sure that the caller can act for a mailbox.
    ///
    /// All the sessions of a client share its token, so the token does not tell which session
    /// calls. Only a request signed for the session of the mailbox proves it. One exception: a
    /// token whose only pattern is this exact mailbox, because no other mailbox can use it.
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
        if Self::session_owned(mailbox) {
            return Self::require_owner_session(caller, mailbox);
        }
        match Self::bound_session(db, mailbox)? {
            Some(bound) if caller.session.as_deref() == Some(bound.as_str()) => Ok(()),
            Some(_) => Err(BridgeError::BoundToOtherSession(mailbox.to_owned())),
            None if caller.auth.agents.iter().any(|pattern| pattern == mailbox) => Ok(()),
            None => Err(BridgeError::SessionRequired(mailbox.to_owned())),
        }
    }

    /// Applies the routing rules of the teams: a solo session cannot send, only the lead sends to
    /// `all`, the recipient must be in the team of the sender, and a worker sends only to its lead.
    ///
    /// `sender` is the team and the role of the sender, `None` for a solo sender.
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

    /// Adds a line to the audit log. The log keeps the size and the SHA-256 digest of the content,
    /// not the content.
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

    /// Adds a refusal to the audit log, and returns the error.
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

    /// Sends a message from `from_raw` to `to_raw`.
    ///
    /// The recipient is a mailbox, `codex` (the latest Codex session) or `all` (the team of the
    /// lead). The checks come in this order: the names, the content, the rate, then the identity
    /// and the routing. The bus then stores the message, tells the waits and the long polls, and
    /// wakes an idle recipient.
    ///
    /// # Errors
    ///
    /// Returns the first failed check. The audit log keeps the refusals for a token in the content,
    /// the rate, the identity and the routing.
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

    /// Checks the identity, the recipient and the routing, then stores the message, in one
    /// transaction: a registration at the same time cannot change the recipient between the check
    /// and the insert.
    ///
    /// The admin token belongs to the user, who is in no team, so the routing rules do not apply to
    /// it.
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

    /// Checks a request that changes the team of a mailbox, and returns the mailbox with the locked
    /// database.
    ///
    /// Only the team commands of the user call this function, and no MCP tool does, so a model
    /// cannot change a team. A team change needs a signed session, also for a token whose only
    /// pattern is this mailbox. The admin token of the user is the only exception.
    fn prepare_member(
        &self,
        caller: &Caller,
        mailbox_raw: &str,
    ) -> Result<(String, MutexGuard<'_, Connection>), BridgeError> {
        let mailbox = Self::normalize_agent(mailbox_raw, "mailbox")?;
        Self::require_concrete(&mailbox)?;
        let mut db = lock(&self.db);
        Self::require_registered_codex(&db, &mailbox)?;
        if !caller.auth.admin && caller.session.is_none() {
            return Err(BridgeError::SessionRequired(mailbox));
        }
        Self::check_acting(&db, caller, "mailbox", &mailbox)?;
        Self::touch_agent(&mut db, &mailbox)?;
        Ok((mailbox, db))
    }

    /// Sends a notice of the daemon about a team change.
    ///
    /// The notice passes the checks as the admin, because it comes from the daemon. A failed notice
    /// does not undo the change.
    fn notify(self: &Arc<Self>, from: &str, to: &str, content: &str) {
        let daemon = Caller {
            auth: AuthInfo::disabled(),
            session: None,
        };
        if let Err(error) = self.send(&daemon, from, to, content) {
            eprintln!("inband: could not notify {to}: {error}");
        }
    }

    /// Makes a mailbox the lead of a team, and creates the team when it does not exist.
    ///
    /// The previous lead of the team becomes a worker, and receives a notice.
    ///
    /// # Errors
    ///
    /// Returns an error when a name is not valid, the request does not come from the session of the
    /// mailbox, or the database fails.
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

    /// Adds a mailbox to a team as a worker. The lead of the team receives a notice.
    ///
    /// # Errors
    ///
    /// Returns an error when a name is not valid, the request does not come from the session of the
    /// mailbox, or the database fails.
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

    /// Removes a mailbox from its team: the session becomes solo. The lead receives a notice.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is not valid, the request does not come from the session of
    /// the mailbox, or the database fails.
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

    /// Checks the mailbox that the caller reads or acts for, updates its last-seen time, and
    /// returns its name in lower case.
    fn acting_mailbox(
        &self,
        caller: &Caller,
        raw: &str,
        field: &'static str,
    ) -> Result<String, BridgeError> {
        let mailbox = Self::normalize_agent(raw, field)?;
        Self::require_concrete(&mailbox)?;
        let mut db = lock(&self.db);
        Self::require_registered_codex(&db, &mailbox)?;
        Self::check_acting(&db, caller, field, &mailbox)?;
        Self::touch_agent(&mut db, &mailbox)?;
        Ok(mailbox)
    }

    fn unread_of(&self, recipient: &str) -> Result<Vec<MessageRow>, BridgeError> {
        Ok(message_rows(&lock(&self.db), UNREAD_SQL, [recipient])?)
    }

    /// Returns the role, the team and the lead of a mailbox, for its protocol text.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is not valid, the caller cannot act for the mailbox, or the
    /// database fails.
    pub fn session_context(
        &self,
        caller: &Caller,
        mailbox_raw: &str,
    ) -> Result<SessionContext, BridgeError> {
        let mailbox = self.acting_mailbox(caller, mailbox_raw, "agent")?;
        let db = lock(&self.db);
        let membership = Self::membership_of(&db, &mailbox)?;
        let lead = match &membership {
            Some((team, _)) => Self::lead_of_team(&db, team)?,
            None => None,
        };
        let (team, role) = membership.map_or((None, Role::Solo), |(team, role)| (Some(team), role));
        Ok(SessionContext {
            mailbox,
            role,
            team,
            lead,
        })
    }

    #[must_use]
    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    /// Returns the unread messages of a mailbox, and keeps them unread.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is not valid, the caller cannot act for the mailbox, or the
    /// database fails.
    pub fn peek_unread(
        &self,
        caller: &Caller,
        for_raw: &str,
    ) -> Result<Vec<MessageRow>, BridgeError> {
        let recipient = self.acting_mailbox(caller, for_raw, "for")?;
        self.unread_of(&recipient)
    }

    /// Returns the unread messages of a mailbox, and marks them as read.
    ///
    /// This is the only operation that marks mail as read: when the answer is lost, the mail stays
    /// unread.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is not valid, the caller cannot act for the mailbox, or the
    /// database fails.
    pub fn fetch_unread(
        &self,
        caller: &Caller,
        for_raw: &str,
    ) -> Result<Vec<MessageRow>, BridgeError> {
        let recipient = self.acting_mailbox(caller, for_raw, "for")?;
        let rows = {
            let mut db = lock(&self.db);
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

    /// Waits for mail, then returns the unread messages without marking them as read.
    ///
    /// The wait ends when mail for the mailbox arrives or when the time ends. It also ends when the
    /// bus lost events or stopped; in all cases, the function reads the unread mail again. Without
    /// `long_wait` (a client without progress notifications), the wait stops after 50 s.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is not valid, the caller cannot act for the mailbox, the
    /// mailbox has too many waits, or the database fails.
    pub async fn wait_for_messages(
        &self,
        caller: &Caller,
        for_raw: &str,
        timeout_seconds: u64,
        long_wait: bool,
    ) -> Result<Vec<MessageRow>, BridgeError> {
        let recipient = self.acting_mailbox(caller, for_raw, "for")?;
        let cap = if long_wait {
            MAX_LONG_WAIT_SECONDS
        } else {
            MAX_WAIT_SECONDS
        };
        let timeout = Duration::from_secs(timeout_seconds.clamp(5, cap));
        let mut events = self.events.subscribe();
        let immediate = self.unread_of(&recipient)?;
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
                _ => break,
            }
        }
        self.unread_of(&recipient)
    }

    /// Long poll for one mailbox: the shim of the session uses it.
    ///
    /// # Errors
    ///
    /// Returns an error when the caller cannot act for the mailbox, when there are too many long
    /// polls for it, or when the database fails.
    pub async fn subscribe_mailbox(
        &self,
        caller: &Caller,
        mailbox_raw: &str,
        timeout_seconds: u64,
        after_id: Option<i64>,
    ) -> Result<Vec<MessageRow>, BridgeError> {
        let mailbox = self.acting_mailbox(caller, mailbox_raw, "mailbox")?;
        self.subscribe(&mailbox, true, timeout_seconds, after_id)
            .await
    }

    /// Long poll for a whole family, for example all `claude-*`.
    ///
    /// It reads the mail of many sessions, so only the admin token can use it.
    ///
    /// # Errors
    ///
    /// Returns an error when the caller is not the admin, when there are too many long polls for
    /// the family, or when the database fails.
    pub async fn subscribe_family(
        &self,
        caller: &Caller,
        prefix_raw: &str,
        timeout_seconds: u64,
        after_id: Option<i64>,
    ) -> Result<Vec<MessageRow>, BridgeError> {
        let prefix = Self::normalize_agent(prefix_raw, "prefix")?;
        if !caller.auth.admin {
            return Err(BridgeError::NotAuthorized {
                field: "prefix",
                agent: prefix,
            });
        }
        self.subscribe(&prefix, false, timeout_seconds, after_id)
            .await
    }

    /// Returns at once the unread mail after `after_id`, or else waits for the next message.
    ///
    /// An old shim sends no `after_id`: it receives only the new mail.
    ///
    /// # Errors
    ///
    /// Returns an error when there are too many long polls for this target, or when the database
    /// fails.
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

    /// Keeps the online or offline state that the hooks of a session send.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is not valid, the caller cannot act for the mailbox, or the
    /// database fails.
    pub fn set_presence(
        &self,
        caller: &Caller,
        name_raw: &str,
        online: bool,
    ) -> Result<(), BridgeError> {
        let name = self.acting_mailbox(caller, name_raw, "agent")?;
        lock(&self.db).execute(
            "UPDATE agents SET online = ?1, presence_at = ?2 WHERE name = ?3",
            params![i64::from(online), iso_now(), name],
        )?;
        Ok(())
    }

    /// Returns `online`, `offline` or `unknown`. A mailbox with a wait in progress is online.
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

    /// Returns past messages, the oldest first.
    ///
    /// A session sees its own mail: what it sent and what it received. Each message of a team goes
    /// between the lead and one member, so a lead sees its whole team. Only the admin token,
    /// without a viewer, sees all messages. `before_id` gives the previous page.
    ///
    /// # Errors
    ///
    /// Returns an error when the caller is not the admin and gives no viewer, the caller cannot act
    /// for the viewer, or the database fails.
    pub fn history(
        &self,
        caller: &Caller,
        viewer: Option<&str>,
        limit: u32,
        before_id: Option<i64>,
    ) -> Result<History, BridgeError> {
        let viewer = match viewer {
            Some(raw) => Some(self.acting_mailbox(caller, raw, "for")?),
            None if caller.auth.admin => None,
            None => return Err(BridgeError::ViewerRequired),
        };
        let capped = i64::from(limit.clamp(1, MAX_HISTORY));
        let before = before_id.unwrap_or(i64::MAX);
        let db = lock(&self.db);
        let mut messages = message_rows(
            &db,
            &format!(
                "SELECT m.id, m.sender, m.recipient, m.content, m.created_at, m.sender_role
                 FROM messages m WHERE m.id < ?2 AND {VIEWER_MAIL_SQL} ORDER BY m.id DESC LIMIT ?3"
            ),
            params![viewer, before, capped],
        )?;
        messages.reverse();
        let total: i64 = db.query_row(
            &format!("SELECT COUNT(*) FROM messages m WHERE {VIEWER_MAIL_SQL}"),
            params![viewer],
            |row| row.get(0),
        )?;
        Ok(History {
            messages,
            total: usize::try_from(total).unwrap_or(0),
        })
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

    /// Returns the agents, their presence, roles and unread mail, and the last wakes.
    ///
    /// With a viewer, the list contains only the team of the viewer, or only the viewer when it is
    /// solo. Only the admin token can omit the viewer, and then sees all agents.
    ///
    /// # Errors
    ///
    /// Returns an error when the caller is not the admin and gives no viewer, the caller cannot act
    /// for the viewer, or the database fails.
    pub fn status(&self, caller: &Caller, viewer_raw: Option<&str>) -> Result<Status, BridgeError> {
        let viewer = match viewer_raw {
            Some(raw) => Some(self.acting_mailbox(caller, raw, "from")?),
            None if caller.auth.admin => None,
            None => return Err(BridgeError::ViewerRequired),
        };
        let viewer = viewer.as_deref();
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
        let last_wakes = self.last_wakes(viewer.is_some(), &agents)?;
        Ok(Status {
            daemon: "inband",
            started_at: self.started_at.clone(),
            team,
            lead,
            agents,
            last_wakes,
        })
    }

    /// Returns the last five wakes. A team view keeps only the wakes of the agents in its list.
    fn last_wakes(
        &self,
        team_view: bool,
        agents: &[AgentStatus],
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
                .collect()
        };
        Ok(wakes)
    }

    /// Deletes all messages and deliveries, and returns the number of deleted messages. The audit
    /// log stays.
    ///
    /// # Errors
    ///
    /// Returns an error when the caller is not the admin, `confirm` is not `wipe`, or the database
    /// fails.
    pub fn clear(&self, caller: &Caller, confirm: &str) -> Result<usize, BridgeError> {
        if !caller.auth.admin {
            return Err(BridgeError::AdminRequired);
        }
        if confirm != "wipe" {
            return Err(BridgeError::ClearNotConfirmed);
        }
        let db = lock(&self.db);
        let count: i64 = db.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
        db.execute_batch("DELETE FROM deliveries; DELETE FROM messages;")?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Returns why a wake must wait, or `None`: the hourly limit, or a successful wake within the
    /// debounce time.
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

    /// Wakes the recipient of a message when the configuration has a wake target for it, and
    /// returns what the bus did, for the send result.
    ///
    /// A Codex mailbox wakes its session through `codex queue`, with retries. An `OpenCode` session
    /// mailbox wakes its own session. The fixed `opencode` mailbox of v1 clients wakes the most
    /// recent session.
    fn maybe_wake(self: &Arc<Self>, recipient: &str) -> String {
        let is_codex = codex_session::is_canonical_mailbox(recipient);
        let is_opencode_session = opencode_session::is_session_mailbox(recipient);
        let key = if is_codex {
            CODEX_FAMILY
        } else if is_opencode_session {
            opencode_session::OPENCODE_FAMILY
        } else {
            recipient
        };
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
        let session_id = if is_opencode_session {
            match Self::bound_session(&lock(&self.db), recipient) {
                Ok(Some(session)) => Some(session),
                Ok(None) => {
                    return "wake-failed: no OpenCode session is bound to this mailbox".to_owned();
                }
                Err(error) => return format!("wake-failed: {error}"),
            }
        } else {
            None
        };
        let bridge = Arc::clone(self);
        let mailbox = recipient.to_owned();
        tokio::spawn(async move {
            let input = WakeInput {
                recipient: mailbox.clone(),
                session_id,
                mailbox: Some(mailbox.clone()),
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

    /// Wakes a Codex session, and tries again after each delay of the configuration.
    ///
    /// The task stops when a wake succeeds, the mail is read, a wake must wait, or a newer task
    /// starts.
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

    /// Stops the Codex wake task of a mailbox whose mail is all read.
    fn cancel_retry(&self, mailbox: &str) {
        if let Some(state) = lock(&self.retries).remove(mailbox)
            && let Some(task) = state.task
        {
            task.abort();
        }
    }

    /// Adds a wake attempt to the database and to the log.
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

    /// Wakes the Codex sessions that have unread mail. The daemon calls it once, at start.
    ///
    /// # Errors
    ///
    /// Returns an error when the database fails.
    pub fn reconcile_codex_wakes(self: &Arc<Self>) -> Result<(), BridgeError> {
        let sessions = codex_session::list_with_unread(&lock(&self.db))?;
        for (session, unread) in sessions {
            if unread > 0 {
                self.maybe_wake(&session.mailbox);
            }
        }
        Ok(())
    }

    /// Registers a Codex session, for the Codex hook of that session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session id is not valid, the request is signed for another
    /// session, or the database fails.
    pub fn register_codex(
        &self,
        caller: &Caller,
        session_id: &str,
        cwd: &str,
        lifecycle: &str,
    ) -> Result<codex_session::CodexSession, BridgeError> {
        let mailbox = codex_session::canonical_mailbox(session_id)?;
        if !caller.auth.admin {
            Self::require_token_scope(caller, "mailbox", &mailbox)?;
            Self::require_owner_session(caller, &mailbox)?;
        }
        Ok(codex_session::register(
            &mut lock(&self.db),
            session_id,
            cwd,
            lifecycle,
        )?)
    }

    /// Keeps the state of a Codex session, for the Codex hook of that session.
    ///
    /// # Errors
    ///
    /// Returns an error when the caller cannot act for the mailbox, or the database fails.
    pub fn touch_codex(
        &self,
        caller: &Caller,
        mailbox_raw: &str,
        lifecycle: Option<&str>,
    ) -> Result<(), BridgeError> {
        let mailbox = self.acting_mailbox(caller, mailbox_raw, "mailbox")?;
        Ok(codex_session::touch(
            &mut lock(&self.db),
            &mailbox,
            lifecycle,
        )?)
    }

    /// Returns the registered Codex session with this id, or `None`.
    ///
    /// # Errors
    ///
    /// Returns an error when the session id is not valid, or the database fails.
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
