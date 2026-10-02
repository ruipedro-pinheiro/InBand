//! The `inband hook claude` and `inband hook codex` commands, and the team commands of the user.
//!
//! The user types `/lead x`, `/join x` or `/solo` in Claude Code, and `$lead x` in Codex, which
//! refuses unknown slash commands. Only the `UserPromptSubmit` hook sees these commands, and only
//! the user writes a prompt: a model, or mail from another agent, cannot change a team.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Method;
use serde_json::{Value, json};

use crate::client::{Client, ClientError};
use crate::codex_session;
use crate::config::EnvMap;
use crate::protocol::identity_text;

/// The longest request of a hook: a hook must never hold a session back.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(2);
/// The minimum time between two "mail waits" reminders for one mailbox.
const MAILCHECK_SECONDS: u64 = 120;
const MAX_PROMPT_COMMAND: usize = 200;

/// A team command that the user typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamCommand {
    Lead(String),
    Join(String),
    Solo,
}

/// Returns the team command of a prompt: `None` for a usual prompt, an error text for a team
/// command with wrong arguments.
///
/// The command must be the whole prompt, on one line, and start with `/` or `$`.
#[must_use]
pub fn parse_team_command(prompt: &str) -> Option<Result<TeamCommand, String>> {
    let prompt = prompt.trim();
    if prompt.len() > MAX_PROMPT_COMMAND || prompt.contains('\n') {
        return None;
    }
    let rest = prompt
        .strip_prefix('/')
        .or_else(|| prompt.strip_prefix('$'))?;
    let mut words = rest.split_whitespace();
    let name = words.next()?;
    let arguments: Vec<&str> = words.collect();
    let command = match (name, arguments.as_slice()) {
        ("lead", [team]) => Ok(TeamCommand::Lead((*team).to_owned())),
        ("join", [team]) => Ok(TeamCommand::Join((*team).to_owned())),
        ("solo", []) => Ok(TeamCommand::Solo),
        ("lead" | "join", _) => Err(format!("usage: /{name} <team>")),
        ("solo", _) => Err("usage: /solo".to_owned()),
        _ => return None,
    };
    Some(command)
}

/// Runs a team command for a mailbox, and returns the text for the session: what changed, then the
/// new protocol.
///
/// # Errors
///
/// Returns an error when the daemon does not answer or refuses the command.
pub async fn run_team_command(
    client: &Client,
    mailbox: &str,
    session: &str,
    command: &TeamCommand,
) -> Result<String, ClientError> {
    let (path, body) = match command {
        TeamCommand::Lead(team) => ("/team/lead", json!({ "mailbox": mailbox, "team": team })),
        TeamCommand::Join(team) => ("/team/join", json!({ "mailbox": mailbox, "team": team })),
        TeamCommand::Solo => ("/team/leave", json!({ "mailbox": mailbox })),
    };
    let reply = client
        .request(Method::POST, path, Some(&body), Some(session), HOOK_TIMEOUT)
        .await?;
    Ok(format!(
        "{}\n\n{}",
        change_summary(&reply["change"]),
        reply["protocol"].as_str().unwrap_or_default()
    ))
}

fn change_summary(change: &Value) -> String {
    let mailbox = change["mailbox"].as_str().unwrap_or("this session");
    let team = change["team"].as_str().unwrap_or_default();
    match change["role"].as_str() {
        Some("lead") => {
            let mut text = format!(
                "The user ran an InBand team command: `{mailbox}` is now the lead of team `{team}`."
            );
            if let Some(previous) = change["replaced_lead"].as_str() {
                let _ = write!(text, " The previous lead `{previous}` is a worker now.");
            }
            text
        }
        Some("worker") => format!(
            "The user ran an InBand team command: `{mailbox}` joined team `{team}` as a worker."
        ),
        _ => format!(
            "The user ran an InBand team command: `{mailbox}` left its team. It is solo: InBand carries no mail to or from it."
        ),
    }
}

fn context(event: &str, text: &str) -> Value {
    json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": text } })
}

/// Returns the hook output for a typed team command, or `None` for a usual prompt.
///
/// A successful command adds its result to the context. A failed command blocks the prompt: the
/// user sees the error, and the model does not receive the command.
async fn team_command_output(
    client: &Client,
    mailbox: &str,
    session: &str,
    prompt: &str,
) -> Option<Value> {
    let command = match parse_team_command(prompt)? {
        Ok(command) => command,
        Err(usage) => return Some(json!({ "decision": "block", "reason": usage })),
    };
    Some(
        match run_team_command(client, mailbox, session, &command).await {
            Ok(text) => context("UserPromptSubmit", &text),
            Err(error) => json!({ "decision": "block", "reason": format!("inband: {error}") }),
        },
    )
}

/// Returns the mailbox of a Claude Code session: `claude-<directory>-<4 hex chars of the session>`.
///
/// This is the name of v1, so the v1 mail stays in the same mailbox.
#[must_use]
pub fn claude_mailbox(cwd: &str, session_id: &str) -> String {
    let base = Path::new(cwd.trim_end_matches('/'))
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("root")
        .to_lowercase();
    let slug: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "dir" } else { slug };
    let suffix: String = session_id
        .to_lowercase()
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(4)
        .collect();
    let suffix = if suffix.is_empty() {
        "0000".to_owned()
    } else {
        suffix
    };
    format!(
        "claude-{}-{suffix}",
        slug.chars().take(20).collect::<String>()
    )
}

fn text_field<'a>(payload: &'a Value, name: &str) -> &'a str {
    payload
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Runs one Claude Code hook, and returns its output, or `None` when it has nothing to say.
///
/// - `SessionStart` binds the mailbox, and returns the identity and the protocol.
/// - `UserPromptSubmit` runs a team command.
/// - `PostToolUse` tells a working session about its unread mail, at most every two minutes.
/// - `SessionEnd` marks the session offline.
pub async fn claude_hook(
    client: &Client,
    payload: &Value,
    state: &MailcheckState,
) -> Option<Value> {
    let session = text_field(payload, "session_id");
    if session.is_empty() {
        return None;
    }
    let mailbox = claude_mailbox(text_field(payload, "cwd"), session);
    match text_field(payload, "hook_event_name") {
        "SessionStart" => Some(claude_session_start(client, &mailbox, session).await),
        "PostToolUse" => {
            if !state.due(&mailbox) {
                return None;
            }
            let path = format!("/claude/hook?agent={mailbox}&event=PostToolUse");
            let reply = client
                .request(Method::GET, &path, None, Some(session), HOOK_TIMEOUT)
                .await
                .ok()?;
            reply.get("hookSpecificOutput").is_some().then_some(reply)
        }
        "SessionEnd" => {
            let body = json!({ "agent": mailbox, "online": false });
            let _ = client
                .request(
                    Method::POST,
                    "/presence",
                    Some(&body),
                    Some(session),
                    HOOK_TIMEOUT,
                )
                .await;
            None
        }
        "UserPromptSubmit" => {
            team_command_output(client, &mailbox, session, text_field(payload, "prompt")).await
        }
        _ => None,
    }
}

/// Runs the `SessionStart` hook. When the daemon does not answer, the output still gives the
/// mailbox, and tells that InBand is not available.
async fn claude_session_start(client: &Client, mailbox: &str, session: &str) -> Value {
    let path = format!("/claude/hook?agent={mailbox}&event=SessionStart");
    match client
        .request(Method::GET, &path, None, Some(session), HOOK_TIMEOUT)
        .await
    {
        Ok(reply) => {
            let body = json!({ "agent": mailbox, "online": true });
            let _ = client
                .request(
                    Method::POST,
                    "/presence",
                    Some(&body),
                    Some(session),
                    HOOK_TIMEOUT,
                )
                .await;
            reply
        }
        Err(error) => context(
            "SessionStart",
            &format!(
                "{} InBand is not available for this session: {error}",
                identity_text(mailbox)
            ),
        ),
    }
}

/// Runs one Codex hook, and returns its output, or `None` when it has nothing to say.
///
/// `SessionStart` and `Stop` go to the daemon unchanged. `UserPromptSubmit` runs a team command.
pub async fn codex_hook(client: &Client, payload: &Value) -> Option<Value> {
    let session = text_field(payload, "session_id");
    let mailbox = codex_session::canonical_mailbox(session).ok()?;
    match text_field(payload, "hook_event_name") {
        "UserPromptSubmit" => {
            team_command_output(client, &mailbox, session, text_field(payload, "prompt")).await
        }
        "SessionStart" | "Stop" => {
            match client
                .request(
                    Method::POST,
                    "/codex/hook",
                    Some(payload),
                    Some(session),
                    HOOK_TIMEOUT,
                )
                .await
            {
                Ok(reply) => Some(reply),
                Err(error) => {
                    eprintln!("inband: {error}");
                    None
                }
            }
        }
        _ => None,
    }
}

/// Limits the "mail waits" reminders to one for each mailbox every two minutes.
pub struct MailcheckState {
    directory: Option<PathBuf>,
}

impl MailcheckState {
    /// Keeps the reminder times in `$XDG_RUNTIME_DIR/inband`, else in `~/.cache/inband`.
    #[must_use]
    pub fn from_env(env: &EnvMap) -> Self {
        let non_empty = |name: &str| env.get(name).filter(|value| !value.is_empty());
        let directory = non_empty("XDG_RUNTIME_DIR")
            .map(|dir| Path::new(dir).join("inband"))
            .or_else(|| non_empty("HOME").map(|home| Path::new(home).join(".cache/inband")));
        Self { directory }
    }

    /// Returns a state without a limit, for the tests.
    #[must_use]
    pub fn always() -> Self {
        Self { directory: None }
    }

    /// Returns `true` when a reminder can go now, and keeps the time.
    fn due(&self, mailbox: &str) -> bool {
        let Some(directory) = &self.directory else {
            return true;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let stamp = directory.join(format!("mailcheck-{mailbox}"));
        let last = std::fs::read_to_string(&stamp)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok());
        if last.is_some_and(|last| now.saturating_sub(last) < MAILCHECK_SECONDS) {
            return false;
        }
        if create_private_dir(directory).is_ok() {
            let _ = std::fs::write(&stamp, format!("{now}\n"));
        }
        true
    }
}

fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
#[path = "hooks_tests.rs"]
mod tests;
