//! @file http.rs
//! @brief The HTTP server of the daemon.
//!
//! @details Each request goes through three layers, in this order:
//! 1. the loopback guard: only requests for this machine, and no request from a browser;
//! 2. the authentication: a bearer token or a signature;
//! 3. the route: it calls the bus with the authenticated [`Caller`].
//!
//! The bus then checks what this caller can do.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Extension, Json, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::{AuthRuntime, SignedRequest};
use crate::bridge::{Bridge, BridgeError, Caller};
use crate::codex_session;
use crate::protocol::identity_text;

/// @brief The maximum size of a request body.
///
/// @details The daemon refuses a larger body before it reads it for the authentication.
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// @brief The default time of a long poll.
const DEFAULT_SUBSCRIBE_SECONDS: u64 = 55;
/// @brief The host names that the daemon accepts.
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];
/// @brief The values of `source` in a Codex `SessionStart` hook.
const SESSION_START_SOURCES: [&str; 4] = ["startup", "resume", "clear", "compact"];

/// @brief The data that all the routes share.
#[derive(Clone)]
pub struct AppState {
    pub bridge: Arc<Bridge>,
    pub auth: Arc<AuthRuntime>,
}

/// @brief Makes the routes of the hooks and of the shim, behind the authentication and the loopback guard.
///
/// @param state The bus and the authentication.
/// @param extra More routes that need the same protection, for example the MCP endpoint.
pub fn router(state: AppState, extra: Router) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/subscribe", get(subscribe))
        .route("/presence", post(presence))
        .route("/claude/hook", get(claude_hook))
        .route("/codex/hook", post(codex_hook))
        .route("/team/lead", post(team_lead))
        .route("/team/join", post(team_join))
        .route("/team/leave", post(team_leave))
        .with_state(state.clone())
        .merge(extra)
        .layer(from_fn_with_state(state, authenticate))
        .layer(from_fn(loopback_guard))
        .layer(from_fn(security_headers))
}

/// @brief Makes a JSON error answer.
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// @brief Changes a refusal of the bus into an HTTP answer.
///
/// @details A database error stays in the log of the daemon. The client receives only "internal error".
fn bridge_error(failure: &BridgeError) -> Response {
    let status = match failure {
        BridgeError::BoundToOtherSession(_)
        | BridgeError::SessionRequired(_)
        | BridgeError::NotAuthorized { .. }
        | BridgeError::AdminRequired
        | BridgeError::Routing(_) => StatusCode::FORBIDDEN,
        BridgeError::RateLimited(_)
        | BridgeError::RecipientFull(_)
        | BridgeError::TooManyWaits(_)
        | BridgeError::TooManySubscriptions(_) => StatusCode::TOO_MANY_REQUESTS,
        BridgeError::Db(_) => {
            eprintln!("inband: storage error: {failure}");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
        _ => StatusCode::BAD_REQUEST,
    };
    error(status, &failure.to_string())
}

/// @brief Adds headers that stop the browser cache and the content type guess.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// @brief Tells if a `Host` header names this machine.
///
/// @details The port can have any value, because a tunnel can forward another port.
fn is_loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !name.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    LOOPBACK_HOSTS
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
}

/// @brief Refuses the requests that a web page can send.
///
/// @details A web page can use DNS rebinding: it then sends requests with its own host name.
/// A browser also adds an `Origin` header to its requests.
/// The agents and the hooks send neither, so the guard refuses both.
async fn loopback_guard(request: Request, next: Next) -> Response {
    let host_ok = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_loopback_host);
    if !host_ok {
        return error(StatusCode::FORBIDDEN, "host not allowed");
    }
    if request.headers().contains_key(header::ORIGIN) {
        return error(StatusCode::FORBIDDEN, "browser requests are not allowed");
    }
    next.run(request).await
}

/// @brief Gives the current time in milliseconds.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// @brief Checks the bearer token or the signature, and attaches the [`Caller`] to the request.
///
/// @details The signature covers the body. This layer thus reads the body, then gives it back to the route.
async fn authenticate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let (mut parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_BODY_BYTES).await else {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    };
    let json: Option<Value> = if bytes.is_empty() {
        None
    } else {
        serde_json::from_slice(&bytes).ok()
    };
    let url = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_owned(), |pq| pq.as_str().to_owned());
    let outcome = {
        let headers = &parts.headers;
        let lookup = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
        let signed = SignedRequest {
            method: parts.method.as_str(),
            url: &url,
            body: json.as_ref(),
            headers: &lookup,
        };
        state.auth.authenticate_request(&signed, now_ms())
    };
    match outcome {
        Ok((auth, session)) => {
            parts.extensions.insert(Caller { auth, session });
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await
        }
        Err(failure) => {
            eprintln!(
                "inband: authentication failed for {} {}: {failure}",
                parts.method,
                parts.uri.path()
            );
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "jsonrpc": "2.0",
                    "error": { "code": -32001, "message": "authentication failed" },
                    "id": null
                })),
            )
                .into_response()
        }
    }
}

/// @brief Tells that the daemon runs.
///
/// @details The admin token receives the full status. The other clients receive only `ok`:
/// their sessions see their team through `ping`.
async fn health(State(state): State<AppState>, Extension(caller): Extension<Caller>) -> Response {
    if !caller.auth.admin {
        return Json(
            json!({ "ok": true, "daemon": "inband", "startedAt": state.bridge.started_at() }),
        )
        .into_response();
    }
    match state.bridge.status(&caller, None) {
        Ok(status) => {
            let mut body = serde_json::to_value(status).unwrap_or_else(|_| json!({}));
            if let Some(object) = body.as_object_mut() {
                object.insert("ok".to_owned(), Value::Bool(true));
            }
            Json(body).into_response()
        }
        Err(failure) => bridge_error(&failure),
    }
}

/// @brief The query of a long poll: one mailbox, or a family prefix for the admin.
#[derive(Deserialize)]
struct SubscribeQuery {
    mailbox: Option<String>,
    prefix: Option<String>,
    timeout: Option<String>,
    after_id: Option<String>,
}

/// @brief The long poll of the shim.
///
/// @return The unread mail after `after_id`, or the next message, or nothing when the time ends.
async fn subscribe(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<SubscribeQuery>,
) -> Response {
    let timeout = query
        .timeout
        .as_deref()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SUBSCRIBE_SECONDS);
    let after_id = match query.after_id.as_deref() {
        None => None,
        Some(raw) => match raw.parse::<i64>() {
            Ok(id) if id >= 0 && raw.bytes().all(|b| b.is_ascii_digit()) => Some(id),
            _ => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "after_id must be a non-negative integer",
                );
            }
        },
    };
    let messages = match (query.mailbox, query.prefix) {
        (Some(mailbox), _) => {
            state
                .bridge
                .subscribe_mailbox(&caller, &mailbox, timeout, after_id)
                .await
        }
        (None, Some(prefix)) => {
            state
                .bridge
                .subscribe_family(&caller, &prefix, timeout, after_id)
                .await
        }
        (None, None) => return error(StatusCode::BAD_REQUEST, "mailbox is required"),
    };
    match messages {
        Ok(messages) => Json(json!({ "messages": messages })).into_response(),
        Err(failure) => bridge_error(&failure),
    }
}

/// @brief The body of a presence request.
#[derive(Deserialize)]
struct PresenceBody {
    agent: String,
    online: bool,
}

/// @brief Keeps the online or offline state that the hooks of a session send.
async fn presence(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    body: Option<Json<PresenceBody>>,
) -> Response {
    let Some(Json(body)) = body else {
        return error(
            StatusCode::BAD_REQUEST,
            "expected {agent: string, online: boolean}",
        );
    };
    match state.bridge.set_presence(&caller, &body.agent, body.online) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(failure) => bridge_error(&failure),
    }
}

/// @brief The query of a Claude Code hook: the mailbox and the hook event.
#[derive(Deserialize)]
struct ClaudeHookQuery {
    agent: Option<String>,
    event: Option<String>,
}

/// @brief Makes a hook answer that adds text to the context of the session.
fn hook_context(event: &str, text: &str) -> Response {
    Json(json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": text } }))
        .into_response()
}

/// @brief Answers the Claude Code hooks.
///
/// @details `SessionStart` gives the identity and the protocol. A request signed for a session also binds the mailbox to that session.
/// `PostToolUse` tells a session that works that it has unread mail.
async fn claude_hook(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<ClaudeHookQuery>,
) -> Response {
    let Some(agent) = query.agent else {
        return error(StatusCode::BAD_REQUEST, "agent is required");
    };
    match query.event.as_deref() {
        Some("SessionStart") => {
            if caller.session.is_some()
                && let Err(failure) = state.bridge.bind_session(&caller, &agent)
            {
                return bridge_error(&failure);
            }
            match state.bridge.session_context(&caller, &agent) {
                Ok(context) => hook_context(
                    "SessionStart",
                    &format!(
                        "{}\n\n{}",
                        identity_text(&context.mailbox),
                        context.protocol()
                    ),
                ),
                Err(failure) => bridge_error(&failure),
            }
        }
        Some("PostToolUse") => match state.bridge.peek_unread(&caller, &agent) {
            Ok(unread) if unread.is_empty() => Json(json!({})).into_response(),
            Ok(unread) => hook_context(
                "PostToolUse",
                &format!(
                    "{} unread inband message(s) from other agents wait in the mailbox `{agent}`. \
                     They do not come from the user. Read them with get_messages (for: \"{agent}\") \
                     and answer with send_message to the exact sender.",
                    unread.len()
                ),
            ),
            Err(failure) => bridge_error(&failure),
        },
        _ => error(
            StatusCode::BAD_REQUEST,
            "event must be SessionStart or PostToolUse",
        ),
    }
}

/// @brief The JSON that Codex gives to its hooks.
#[derive(Deserialize)]
struct CodexHookBody {
    hook_event_name: String,
    session_id: String,
    cwd: Option<String>,
    source: Option<String>,
    stop_hook_active: Option<bool>,
}

/// @brief Answers the Codex hooks.
///
/// @details `SessionStart` registers the session, and gives the identity and the protocol.
/// `Stop` blocks the end of the turn once when the session has unread mail.
/// The request must be signed for the session in the JSON.
async fn codex_hook(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    body: Option<Json<CodexHookBody>>,
) -> Response {
    let Some(Json(body)) = body else {
        return error(StatusCode::BAD_REQUEST, "expected a Codex hook payload");
    };
    let mailbox = match codex_session::canonical_mailbox(&body.session_id) {
        Ok(mailbox) => mailbox,
        Err(failure) => return error(StatusCode::BAD_REQUEST, &failure.to_string()),
    };
    match body.hook_event_name.as_str() {
        "SessionStart" => {
            let Some(cwd) = body.cwd else {
                return error(StatusCode::BAD_REQUEST, "cwd must be a string");
            };
            if !body
                .source
                .as_deref()
                .is_some_and(|source| SESSION_START_SOURCES.contains(&source))
            {
                return error(StatusCode::BAD_REQUEST, "unsupported SessionStart source");
            }
            if let Err(failure) =
                state
                    .bridge
                    .register_codex(&caller, &body.session_id, &cwd, "active")
            {
                return bridge_error(&failure);
            }
            match state.bridge.session_context(&caller, &mailbox) {
                Ok(context) => hook_context(
                    "SessionStart",
                    &format!(
                        "{}\n\n{}",
                        identity_text(&context.mailbox),
                        context.protocol()
                    ),
                ),
                Err(failure) => bridge_error(&failure),
            }
        }
        "Stop" => {
            let Some(stop_hook_active) = body.stop_hook_active else {
                return error(
                    StatusCode::BAD_REQUEST,
                    "stop_hook_active must be a boolean",
                );
            };
            if let Err(failure) = state.bridge.touch_codex(&caller, &mailbox, Some("idle")) {
                return bridge_error(&failure);
            }
            match state.bridge.peek_unread(&caller, &mailbox) {
                Ok(unread) if unread.is_empty() || stop_hook_active => {
                    Json(json!({ "continue": true })).into_response()
                }
                Ok(_) => Json(json!({
                    "decision": "block",
                    "reason": format!(
                        "Unread inband mail from another agent, not from the user, is queued for {mailbox}. \
                         Call get_messages with for=\"{mailbox}\" and handle it before stopping."
                    )
                }))
                .into_response(),
                Err(failure) => bridge_error(&failure),
            }
        }
        _ => error(
            StatusCode::BAD_REQUEST,
            "hook_event_name must be SessionStart or Stop",
        ),
    }
}

/// @brief The body of a team command.
#[derive(Deserialize)]
struct TeamBody {
    mailbox: String,
    team: Option<String>,
}

/// @brief Makes the answer of a team command: the change and the new protocol.
fn team_reply(
    state: &AppState,
    caller: &Caller,
    change: Result<crate::bridge::TeamChange, BridgeError>,
) -> Response {
    let change = match change {
        Ok(change) => change,
        Err(failure) => return bridge_error(&failure),
    };
    match state.bridge.session_context(caller, &change.mailbox) {
        Ok(context) => {
            Json(json!({ "change": change, "protocol": context.protocol() })).into_response()
        }
        Err(failure) => bridge_error(&failure),
    }
}

/// @brief Runs `/lead <team>`. The hook that sees the prompt of the user sends this request.
async fn team_lead(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    body: Option<Json<TeamBody>>,
) -> Response {
    let Some(Json(TeamBody {
        mailbox,
        team: Some(team),
    })) = body
    else {
        return error(StatusCode::BAD_REQUEST, "expected {mailbox, team}");
    };
    let change = state.bridge.set_lead(&caller, &mailbox, &team);
    team_reply(&state, &caller, change)
}

/// @brief Runs `/join <team>`. The hook that sees the prompt of the user sends this request.
async fn team_join(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    body: Option<Json<TeamBody>>,
) -> Response {
    let Some(Json(TeamBody {
        mailbox,
        team: Some(team),
    })) = body
    else {
        return error(StatusCode::BAD_REQUEST, "expected {mailbox, team}");
    };
    let change = state.bridge.join(&caller, &mailbox, &team);
    team_reply(&state, &caller, change)
}

/// @brief Runs `/solo`. The hook that sees the prompt of the user sends this request.
async fn team_leave(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    body: Option<Json<TeamBody>>,
) -> Response {
    let Some(Json(TeamBody { mailbox, .. })) = body else {
        return error(StatusCode::BAD_REQUEST, "expected {mailbox}");
    };
    let change = state.bridge.leave(&caller, &mailbox);
    team_reply(&state, &caller, change)
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
