//! @file client.rs
//! @brief The client of the daemon API, for the hooks, the shims and the `OpenCode` plugin.
//!
//! @details Each request is signed with the token of one client.
//! A request for one session also signs that session.
//! The daemon thus knows which session acts, whatever the tool arguments say.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

use crate::auth::sign_request;
use crate::config::{EnvMap, normalize_loopback_http_base_url};
use crate::tokens::{client_token, load_token_env_file};

/// @brief The daemon URL when no configuration gives another port.
const DEFAULT_URL: &str = "http://127.0.0.1:7447";

/// @brief The errors of a request to the daemon.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("invalid INBAND_URL: {0}")]
    Url(String),
    #[error("cannot read the token file: {0}")]
    Tokens(String),
    #[error("the daemon did not answer: {0}")]
    Unreachable(#[from] reqwest::Error),
    #[error("the daemon refused the request ({status}): {message}")]
    Refused { status: StatusCode, message: String },
    #[error("the daemon sent an invalid answer: {0}")]
    Invalid(String),
}

/// @brief A connection to the daemon, for one client of the token file.
pub struct Client {
    base: String,
    id: String,
    token: Option<String>,
    http: reqwest::Client,
}

/// @brief Gives the URL of the daemon of this machine.
///
/// @details The port comes from the installed `config.json`, else it is 7447.
/// A machine that only runs agents has no `config.json`. It reaches the daemon through a forwarded port 7447.
fn local_daemon_url(env: &EnvMap) -> String {
    crate::daemon::default_directory(env)
        .and_then(|dir| std::fs::read_to_string(dir.join("config.json")).ok())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|config| config["port"].as_u64())
        .filter(|port| (1..=65_535).contains(port))
        .map_or_else(
            || DEFAULT_URL.to_owned(),
            |port| format!("http://127.0.0.1:{port}"),
        )
}

/// @brief Gives the current time in milliseconds.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

impl Client {
    /// @brief Makes the client from the environment and the token file.
    ///
    /// @details `INBAND_URL` selects the daemon, on this machine only. `INBAND_CLIENT_ID` replaces the client name.
    ///
    /// @param client_id The client name, for example `claude`.
    /// @param env The environment variables.
    /// @throws ClientError The URL is not valid, or the token file cannot be read.
    pub fn from_env(client_id: &str, mut env: EnvMap) -> Result<Self, ClientError> {
        load_token_env_file(&mut env).map_err(|error| ClientError::Tokens(error.to_string()))?;
        let raw = env
            .get("INBAND_URL")
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| local_daemon_url(&env));
        let base = normalize_loopback_http_base_url(&raw, &env)
            .map_err(|error| ClientError::Url(error.to_string()))?;
        let client_id = env
            .get("INBAND_CLIENT_ID")
            .filter(|id| !id.is_empty())
            .map_or(client_id, String::as_str)
            .to_owned();
        let token = client_token(&client_id, &env).map(str::to_owned);
        Self::new(&base, &client_id, token)
    }

    /// @brief Makes a client for the daemon at `base`.
    ///
    /// @details The HTTP client does not use a system proxy, and does not follow redirects.
    ///
    /// @param base The daemon URL.
    /// @param client_id The client name.
    /// @param token The token. `None` sends requests without a signature.
    /// @throws ClientError The HTTP client cannot be made.
    pub fn new(base: &str, client_id: &str, token: Option<String>) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            id: client_id.to_owned(),
            token,
            http,
        })
    }

    /// @brief Makes a request, and adds the signature headers when the client has a token.
    fn build(
        &self,
        method: &Method,
        path: &str,
        body: Option<&Value>,
        session: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let url = format!("{}{path}", self.base);
        let mut request = self.http.request(method.clone(), &url);
        if let Some(token) = &self.token {
            for (name, value) in sign_request(
                &self.id,
                token,
                (method.as_str(), &url, body),
                now_ms(),
                None,
                session,
            ) {
                request = request.header(name, value);
            }
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        request
    }

    /// @brief Sends one request and gives its JSON answer.
    ///
    /// @param method The HTTP method.
    /// @param path The path and the query.
    /// @param body The JSON body.
    /// @param session The session that the request acts for.
    /// @param timeout The maximum time of the request.
    /// @throws ClientError The daemon does not answer, refuses the request, or gives an answer that is not JSON.
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        let response = self
            .build(&method, path, body, session)
            .timeout(timeout)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(ClientError::Refused {
                status,
                message: error_message(&text),
            });
        }
        serde_json::from_str(&text).map_err(|error| ClientError::Invalid(error.to_string()))
    }

    /// @brief Calls one MCP tool of the daemon for a session.
    ///
    /// @details The request asks for progress notifications, so a long wait does not stop at 50 s.
    ///
    /// @param name The tool name.
    /// @param arguments The tool arguments.
    /// @param session The session that the call acts for.
    /// @param on_progress Receives each progress notification. The shim sends them on to its own client.
    /// @return The tool result.
    /// @throws ClientError The daemon does not answer, refuses the request, or gives no result.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: &Value,
        session: Option<&str>,
        mut on_progress: impl FnMut(&Value) + Send,
    ) -> Result<Value, ClientError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments, "_meta": { "progressToken": 1 } },
        });
        self.rpc(&body, session, &mut on_progress).await
    }

    /// @brief Sends one JSON-RPC request to `/mcp` and reads the answer.
    ///
    /// @details The answer is one JSON body, or a stream of events (SSE) with the progress notifications before the result.
    async fn rpc(
        &self,
        body: &Value,
        session: Option<&str>,
        on_progress: &mut (dyn FnMut(&Value) + Send),
    ) -> Result<Value, ClientError> {
        let mut response = self
            .build(&Method::POST, "/mcp", Some(body), session)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ClientError::Refused {
                status,
                message: error_message(&text),
            });
        }
        let is_stream = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        if !is_stream {
            let message: Value = serde_json::from_str(&response.text().await?)
                .map_err(|error| ClientError::Invalid(error.to_string()))?;
            return handle_rpc_message(&message, on_progress)?
                .ok_or_else(|| ClientError::Invalid("no JSON-RPC result".to_owned()));
        }
        let mut events = SseLines::default();
        while let Some(chunk) = response.chunk().await? {
            for message in events.push(&chunk) {
                if let Some(reply) = handle_rpc_message(&message, on_progress)? {
                    return Ok(reply);
                }
            }
        }
        Err(ClientError::Invalid("no JSON-RPC result".to_owned()))
    }
}

/// @brief Reads one JSON-RPC message of the answer.
///
/// @return The result, or `None` for a notification.
/// @throws ClientError The message is a JSON-RPC error.
fn handle_rpc_message(
    message: &Value,
    on_progress: &mut (dyn FnMut(&Value) + Send),
) -> Result<Option<Value>, ClientError> {
    if message.get("method").and_then(Value::as_str) == Some("notifications/progress") {
        on_progress(&message["params"]);
        return Ok(None);
    }
    if message.get("id").is_none() {
        return Ok(None);
    }
    if let Some(error) = message.get("error") {
        return Err(ClientError::Refused {
            status: StatusCode::OK,
            message: error["message"].as_str().unwrap_or("error").to_owned(),
        });
    }
    Ok(Some(message["result"].clone()))
}

/// @brief Gives the `error` text of an answer of the daemon, else the start of the raw text.
fn error_message(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| {
            let error = value.get("error")?;
            error
                .as_str()
                .or_else(|| error.get("message").and_then(Value::as_str))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| text.chars().take(200).collect())
}

/// @brief Cuts an SSE stream into the JSON-RPC messages of its `data:` lines.
#[derive(Default)]
struct SseLines {
    pending: Vec<u8>,
}

impl SseLines {
    /// @brief Adds one part of the stream, and gives the complete messages.
    ///
    /// @details A line can come in two parts. The incomplete line waits for the next part.
    fn push(&mut self, chunk: &[u8]) -> Vec<Value> {
        self.pending.extend_from_slice(chunk);
        let mut messages = Vec::new();
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(data) = line.trim_end().strip_prefix("data:")
                && let Ok(message) = serde_json::from_str(data.trim())
            {
                messages.push(message);
            }
        }
        messages
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
