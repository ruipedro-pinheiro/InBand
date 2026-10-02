//! @file opencode_cli.rs
//! @brief The `inband opencode` commands that the `OpenCode` plugin runs.
//!
//! @details `OpenCode` loads only JavaScript plugins. The plugin is thus a thin layer.
//! It gives the `sessionID` from `OpenCode` to these commands.
//! These commands sign and send each request.

use reqwest::Method;
use serde_json::Value;

use crate::client::{Client, ClientError};
use crate::hooks::{HOOK_TIMEOUT, TeamCommand, parse_team_command, run_team_command};
use crate::opencode_session;
use crate::protocol::identity_text;

/// @brief Gives the mailbox of a session.
///
/// @param session The `OpenCode` session id.
/// @return The mailbox name.
/// @throws String The session id is not valid.
pub fn mailbox(session: &str) -> Result<String, String> {
    opencode_session::mailbox(session).map_err(|_| "invalid OpenCode session id".to_owned())
}

/// @brief Gives the identity and the protocol of a session.
///
/// @details The plugin puts this text in the system prompt of the session.
/// The request also binds the mailbox to the session, so that wakes go to this session.
///
/// @param client The client of the daemon.
/// @param session The `OpenCode` session id.
/// @return The identity and protocol text.
/// @throws String The session id is not valid, or the daemon does not answer, or the daemon refuses the request.
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

/// @brief Runs `/lead x`, `/join x` or `/solo` for a session.
///
/// @param client The client of the daemon.
/// @param session The `OpenCode` session id.
/// @param command The command name: `lead`, `join` or `solo`.
/// @param arguments The arguments of the command, for example the team name.
/// @return The result of the command and the new protocol.
/// @throws String The usage of the command, or the refusal of the daemon.
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

/// @brief Calls one daemon tool for a session.
///
/// @param client The client of the daemon.
/// @param session The `OpenCode` session id.
/// @param name The tool name.
/// @param arguments The tool arguments.
/// @return The text of the result, and true when the tool failed.
/// @throws ClientError The daemon does not answer, or the daemon refuses the request.
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
