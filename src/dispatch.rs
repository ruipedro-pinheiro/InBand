//! Wakes for the real clients: `codex queue` for Codex, and the HTTP API of the local server for
//! `OpenCode`.
//!
//! A wake prompt comes from the configuration. It never contains message content.

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::process::Command;

use crate::config::WakeTarget;
use crate::opencode_session;
use crate::wake::{WakeDispatch, WakeDisposition, WakeFuture, WakeInput, WakeResult};

const WAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DETAIL_CHARS: usize = 4096;

/// Wakes Codex through its CLI, and `OpenCode` through its local server.
pub struct RealWake {
    http: reqwest::Client,
    timeout: Duration,
}

impl RealWake {
    /// Creates the dispatcher with the default timeout.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be created.
    pub fn new() -> Result<Self, reqwest::Error> {
        Self::with_timeout(WAKE_TIMEOUT)
    }

    /// Creates the dispatcher with a given timeout.
    ///
    /// The HTTP client uses no system proxy and follows no redirect: the wake URLs point to this
    /// machine, and a proxy must never receive these requests.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be created.
    pub fn with_timeout(timeout: Duration) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()?;
        Ok(Self { http, timeout })
    }
}

impl WakeDispatch for RealWake {
    fn dispatch(&self, target: &WakeTarget, input: WakeInput) -> WakeFuture {
        match target {
            WakeTarget::Codex { command, .. } => {
                Box::pin(wake_codex(command.clone(), input, self.timeout))
            }
            WakeTarget::Opencode { base_url, .. } => {
                Box::pin(wake_opencode(self.http.clone(), base_url.clone(), input))
            }
        }
    }
}

/// Puts the mailbox name in the wake prompt: in place of `{mailbox}`, else on a last line.
#[must_use]
pub fn render_prompt(prompt: &str, mailbox: &str) -> String {
    if prompt.contains("{mailbox}") {
        prompt.replace("{mailbox}", mailbox)
    } else {
        format!("{prompt}\n\nMailbox: {mailbox}")
    }
}

fn clip(text: &str) -> String {
    text.trim().chars().take(MAX_DETAIL_CHARS).collect()
}

/// Runs `codex queue --thread <session> --message <prompt>`.
///
/// The command runs without a shell, so no value can add a shell command. A running Codex CLI reads
/// the prompt when its current turn ends. When the time ends, the command stops.
async fn wake_codex(command: String, input: WakeInput, timeout: Duration) -> WakeResult {
    let Some(session_id) = input.session_id else {
        return WakeResult::failed("no Codex session id to wake");
    };
    let mailbox = input.mailbox.unwrap_or(input.recipient);
    let prompt = render_prompt(&input.prompt, &mailbox);
    let child = Command::new(&command)
        .args(["queue", "--thread", &session_id, "--message", &prompt])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) => return WakeResult::failed(format!("cannot start {command}: {error}")),
    };
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => WakeResult::failed("codex queue failed: timeout"),
        Ok(Err(error)) => WakeResult::failed(format!("codex queue failed: {error}")),
        Ok(Ok(output)) if output.status.success() => WakeResult {
            disposition: WakeDisposition::Queued,
            detail: format!("queued wake for {mailbox}"),
        },
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let text = if stderr.trim().is_empty() {
                String::from_utf8_lossy(&output.stdout).into_owned()
            } else {
                stderr.into_owned()
            };
            let code = output
                .status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| code.to_string());
            WakeResult::failed(format!("codex queue failed ({code}): {}", clip(&text)))
        }
    }
}

/// One session of the `GET /session` answer of `OpenCode`.
#[derive(Debug, Deserialize)]
struct OpencodeSession {
    id: String,
    #[serde(rename = "parentID")]
    parent_id: Option<String>,
    time: Option<OpencodeTime>,
}

#[derive(Debug, Deserialize)]
struct OpencodeTime {
    updated: Option<f64>,
}

fn opencode_url(base_url: &str, segments: &[&str]) -> Result<url::Url, WakeResult> {
    let mut url = url::Url::parse(base_url)
        .map_err(|error| WakeResult::failed(format!("invalid OpenCode URL: {error}")))?;
    url.path_segments_mut()
        .map_err(|()| WakeResult::failed("invalid OpenCode URL"))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

/// Returns the root session that `OpenCode` updated last: the target of the fixed `opencode`
/// mailbox of v1 clients.
async fn most_recent_root_session(
    http: &reqwest::Client,
    base_url: &str,
) -> Result<String, WakeResult> {
    let url = opencode_url(base_url, &["session"])?;
    let response = http
        .get(url)
        .send()
        .await
        .map_err(|error| WakeResult::failed(format!("opencode unreachable: {error}")))?;
    if !response.status().is_success() {
        return Err(WakeResult::failed(format!(
            "GET /session -> {}",
            response.status()
        )));
    }
    let sessions: Vec<OpencodeSession> = response
        .json()
        .await
        .map_err(|error| WakeResult::failed(format!("invalid session list: {error}")))?;
    sessions
        .into_iter()
        .filter(|session| session.parent_id.is_none())
        .max_by(|a, b| {
            let updated =
                |s: &OpencodeSession| s.time.as_ref().and_then(|t| t.updated).unwrap_or(0.0);
            updated(a).total_cmp(&updated(b))
        })
        .map(|session| session.id)
        .ok_or_else(|| WakeResult::failed("no opencode session to wake"))
}

/// Starts a turn in one `OpenCode` session with `POST /session/<id>/prompt_async`.
///
/// The URL library removes `.` and `..` segments, so the session id is checked first: else an id
/// such as `..` could reach another endpoint.
async fn wake_opencode(http: reqwest::Client, base_url: String, input: WakeInput) -> WakeResult {
    let session_id = match input.session_id {
        Some(id) => id,
        None => match most_recent_root_session(&http, &base_url).await {
            Ok(id) => id,
            Err(failure) => return failure,
        },
    };
    if !opencode_session::is_valid_session_id(&session_id) {
        return WakeResult::failed(format!("invalid OpenCode session id {session_id:?}"));
    }
    let url = match opencode_url(&base_url, &["session", &session_id, "prompt_async"]) {
        Ok(url) => url,
        Err(failure) => return failure,
    };
    let mailbox = input.mailbox.unwrap_or(input.recipient);
    let body =
        json!({ "parts": [{ "type": "text", "text": render_prompt(&input.prompt, &mailbox) }] });
    match http.post(url).json(&body).send().await {
        Ok(response) if response.status().is_success() => WakeResult {
            disposition: WakeDisposition::Started,
            detail: format!("woke session {session_id}"),
        },
        Ok(response) => {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            WakeResult::failed(format!("POST prompt_async -> {status}: {}", clip(&text)))
        }
        Err(error) => WakeResult::failed(format!("wake failed: {error}")),
    }
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
