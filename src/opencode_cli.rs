//! The `inband opencode` commands that the `OpenCode` plugin runs.
//!
//! `OpenCode` loads only JavaScript plugins. The plugin thus stays a thin layer: it gives the
//! `sessionID` of the caller to these commands, which sign and send each request.

use reqwest::Method;
use serde_json::Value;

use crate::client::{Client, ClientError};
use crate::hooks::{HOOK_TIMEOUT, TeamCommand, parse_team_command, run_team_command};
use crate::opencode_session;
use crate::protocol::identity_text;

/// Returns the mailbox of a session.
///
/// # Errors
///
/// Returns an error when the session id is not valid.
pub fn mailbox(session: &str) -> Result<String, String> {
    opencode_session::mailbox(session).map_err(|_| "invalid OpenCode session id".to_owned())
}

/// Returns the identity and the protocol of a session, for its system prompt.
///
/// The request also binds the mailbox to the session, so that wakes go to this session.
///
/// # Errors
///
/// Returns an error when the session id is not valid, or when the daemon does not answer or refuses
/// the request.
pub async fn context(client: &Client, session: &str) -> Result<String, String> {
    let mailbox = mailbox(session)?;
    let path = format!("/claude/hook?agent={mailbox}&event=SessionStart");
    let reply = client
        .request(Method::GET, &path, None, Some(session), HOOK_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    Ok(reply["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .map_or_else(|| identity_text(&mailbox), str::to_owned))
}

/// Runs `/lead x`, `/join x` or `/solo` for a session, and returns what changed with the new
/// protocol.
///
/// # Errors
///
/// Returns the usage of the command, or the refusal of the daemon.
pub async fn team(
    client: &Client,
    session: &str,
    command: &str,
    arguments: &str,
) -> Result<String, String> {
    let mailbox = mailbox(session)?;
    let typed = format!("/{command} {arguments}");
    let command: TeamCommand = match parse_team_command(&typed) {
        Some(Ok(command)) => command,
        Some(Err(usage)) => return Err(usage),
        None => return Err(format!("unknown team command: {command}")),
    };
    run_team_command(client, &mailbox, session, &command)
        .await
        .map_err(|error| error.to_string())
}

/// Calls one daemon tool for a session. Returns the text of the result, and `true` when the tool
/// failed.
///
/// # Errors
///
/// Returns an error when the daemon does not answer or refuses the request.
pub async fn tool(
    client: &Client,
    session: &str,
    name: &str,
    arguments: &Value,
) -> Result<(String, bool), ClientError> {
    let result = client
        .call_tool(name, arguments, Some(session), |_| {})
        .await?;
    let text = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok((text, result["isError"].as_bool().unwrap_or(false)))
}
