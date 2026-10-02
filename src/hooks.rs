//! @file hooks.rs
//! @brief The `inband hook claude` and `inband hook codex` commands, and the team commands of the user.
//!
//! @details The user types `/lead x`, `/join x` or `/solo` in Claude Code.
//! In Codex, the user types `$lead x`, because Codex refuses unknown slash commands.
//! Only the `UserPromptSubmit` hook sees these commands, and only the user writes a prompt.
//! A model, or mail from another agent, thus cannot change a team.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Method;
use serde_json::{Value, json};

use crate::client::{Client, ClientError};
use crate::codex_session;
use crate::config::EnvMap;
use crate::protocol::identity_text;

/// @brief The maximum time of a request of a hook. A hook must never stop a session for long.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(2);
/// @brief The minimum time between two "mail waits" reminders for one mailbox.
const MAILCHECK_SECONDS: u64 = 120;
/// @brief The maximum length of a prompt that can be a team command.
const MAX_PROMPT_COMMAND: usize = 200;

/// @brief A team command that the user typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamCommand {
    Lead(String),
    Join(String),
    Solo,
}

/// @brief Finds the team command of a prompt.
///
/// @details The command must be the whole prompt, on one line. It starts with `/` or `$`.
///
/// @param prompt The prompt of the user.
/// @return `None` for a usual prompt. An error text for a team command with wrong arguments.
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

/// @brief Runs a team command for a mailbox.
///
/// @param client The client of the daemon.
/// @param mailbox The mailbox of the session.
/// @param session The session id.
/// @param command The command.
/// @return The text for the session: what changed, then the new protocol.
/// @throws ClientError The daemon does not answer, or refuses the command.
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

/// @brief Writes one line that tells what a team command changed.
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

/// @brief Makes a hook output that adds text to the context of the session.
fn context(event: &str, text: &str) -> Value {
    json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": text } })
}

/// @brief Makes the hook output for a typed team command.
///
/// @details A successful command adds its result to the context.
/// A failed command blocks the prompt: the user sees the error, and the model does not receive the command.
///
/// @return `None` when the prompt is not a team command.
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

/// @brief Gives the mailbox of a Claude Code session: `claude-<directory>-<4 hex chars of the session id>`.
///
/// @details This is the name of v1, so the mail of v1 stays in the same mailbox.
///
/// @param cwd The working directory of the session.
/// @param session_id The session id.
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

/// @brief Gives a text field of the hook JSON, or an empty text.
fn text_field<'a>(payload: &'a Value, name: &str) -> &'a str {
    payload
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// @brief Runs one Claude Code hook.
///
/// @details The events are:
/// - `SessionStart`: binds the mailbox, and gives the identity and the protocol;
/// - `UserPromptSubmit`: runs a team command;
/// - `PostToolUse`: tells a session that works that it has unread mail;
/// - `SessionEnd`: marks the session offline.
///
/// @param client The client of the daemon.
/// @param payload The JSON that Claude Code gives to the hook.
/// @param state The time of the last reminder for each mailbox.
/// @return The hook output, or `None` when the hook has nothing to say.
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

/// @brief Runs the Claude Code `SessionStart` hook.
///
/// @details When the daemon does not answer, the hook still gives the mailbox name, and tells that InBand is not available.
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

/// @brief Runs one Codex hook.
///
/// @details `SessionStart` and `Stop` go to the daemon without change. `UserPromptSubmit` runs a team command.
///
/// @return The hook output, or `None` when the hook has nothing to say.
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

/// @brief Limits the "mail waits" reminders to one for each mailbox every two minutes.
pub struct MailcheckState {
    directory: Option<PathBuf>,
}

impl MailcheckState {
    /// @brief Keeps the times in `$XDG_RUNTIME_DIR/inband`, else in `~/.cache/inband`.
    #[must_use]
    pub fn from_env(env: &EnvMap) -> Self {
        let non_empty = |name: &str| env.get(name).filter(|value| !value.is_empty());
        let directory = non_empty("XDG_RUNTIME_DIR")
            .map(|dir| Path::new(dir).join("inband"))
            .or_else(|| non_empty("HOME").map(|home| Path::new(home).join(".cache/inband")));
        Self { directory }
    }

    /// @brief Gives a state without limit, for the tests.
    #[must_use]
    pub fn always() -> Self {
        Self { directory: None }
    }

    /// @brief Tells if a reminder can go now, and keeps the time when it can.
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

/// @brief Creates a directory that only its owner can open.
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
