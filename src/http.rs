//! The HTTP server: the loopback guard, authentication, and the routes of the hooks and the shim.
//!
//! Every request first passes the loopback guard, then authentication. The routes then call the
//! bridge with the authenticated [`Caller`], and the bridge checks what that caller may do.

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

/// Bodies above this size are refused before authentication reads them.
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_SUBSCRIBE_SECONDS: u64 = 55;
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];
const SESSION_START_SOURCES: [&str; 4] = ["startup", "resume", "clear", "compact"];

#[derive(Clone)]
pub struct AppState {
    pub bridge: Arc<Bridge>,
    pub auth: Arc<AuthRuntime>,
}

/// The routes of the hooks and the shim, behind authentication and the loopback guard. `extra`
/// holds routes that need the same protection, such as the MCP service.
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

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// Maps a bridge refusal to an HTTP status. Storage errors stay in the daemon log.
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

/// True for `127.0.0.1`, `localhost` or `[::1]`, with any port. Tunnels may forward another port.
fn is_loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !name.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    LOOPBACK_HOSTS
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
}

/// Refuses requests for another host name, which a DNS rebinding page sends, and every request that
/// a browser marks with an `Origin`. Agents and hooks send neither.
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Checks the bearer token or the signature, and attaches the [`Caller`] to the request. The body
/// is read here because the signature covers it, then put back for the route.
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

/// The admin token gets the full status. Other clients only learn that the daemon runs: their
/// sessions see their team through `ping`.
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

#[derive(Deserialize)]
struct SubscribeQuery {
    mailbox: Option<String>,
    prefix: Option<String>,
    timeout: Option<String>,
    after_id: Option<String>,
}

/// Long poll of the channel shim: the unread mail after `after_id`, or the next delivery.
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

#[derive(Deserialize)]
struct PresenceBody {
    agent: String,
    online: bool,
}

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

#[derive(Deserialize)]
struct ClaudeHookQuery {
    agent: Option<String>,
    event: Option<String>,
}

fn hook_context(event: &str, text: &str) -> Response {
    Json(json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": text } }))
        .into_response()
}

/// Output for the Claude Code hooks. At `SessionStart`, a request signed for a session also binds
/// the mailbox to that session.
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

#[derive(Deserialize)]
struct CodexHookBody {
    hook_event_name: String,
    session_id: String,
    cwd: Option<String>,
    source: Option<String>,
    stop_hook_active: Option<bool>,
}

/// The Codex `SessionStart` and `Stop` hooks. The request must be signed for the session that the
/// payload names.
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

#[derive(Deserialize)]
struct TeamBody {
    mailbox: String,
    team: Option<String>,
}

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

/// `/lead <team>`, from the hook that sees what the user typed.
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

/// `/join <team>`, from the hook that sees what the user typed.
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

/// `/solo`, from the hook that sees what the user typed.
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
