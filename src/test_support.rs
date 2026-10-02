//! @file test_support.rs
//! @brief Shared test tools: an app on a new bus in memory, and requests signed like the hooks and the shims sign them.
//!
//! @details The tools panic when a step fails: a failed preparation is a failed test.

#![allow(clippy::missing_panics_doc, clippy::must_use_candidate)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request as HttpRequest, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::auth::{AuthRuntime, sign_request};
use crate::bridge::Bridge;
use crate::config::{AuthClientConfig, AuthConfig, BridgeConfig, EnvMap, WakeTarget};
use crate::db::open_in_memory;
use crate::http::{AppState, MAX_BODY_BYTES, router};
use crate::wake::{WakeDispatch, WakeFuture, WakeInput, WakeResult};

/// @brief The token of the `claude` client.
pub const CLAUDE: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
/// @brief The token of the `codex` client.
pub const CODEX: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
/// @brief The token of the `opencode` client.
pub const OPENCODE: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
/// @brief The token of the `admin` client.
pub const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// @brief A Codex session id.
pub const CODEX_SESSION: &str = "019f6767-789c-73b2-bc5c-ac8575f29efd";
/// @brief Another Codex session id.
pub const OTHER_CODEX_SESSION: &str = "019f6768-789c-73b2-bc5c-ac8575f29efd";
/// @brief The name and the token of the `claude` client.
pub const CLAUDE_CLIENT: (&str, &str) = ("claude", CLAUDE);
/// @brief The name and the token of the `codex` client.
pub const CODEX_CLIENT: (&str, &str) = ("codex", CODEX);
/// @brief The name and the token of the `opencode` client.
pub const OPENCODE_CLIENT: (&str, &str) = ("opencode", OPENCODE);

/// @brief A wake dispatcher that always fails.
struct NoWake;

impl WakeDispatch for NoWake {
    /// @brief Fails the wake.
    fn dispatch(&self, _target: &WakeTarget, _input: WakeInput) -> WakeFuture {
        Box::pin(async { WakeResult::failed("no wake in tests") })
    }
}

/// @brief Makes the settings of a client.
fn client(token: &str, agents: &[&str], admin: bool) -> AuthClientConfig {
    AuthClientConfig {
        token: Some(token.to_owned()),
        token_env: None,
        agents: agents.iter().map(|a| (*a).to_owned()).collect(),
        directory: None,
        admin,
    }
}

/// @brief Makes the full app, with the MCP endpoint, on a new bus in memory.
pub fn app() -> (Router, Arc<Bridge>) {
    let mut clients = BTreeMap::new();
    clients.insert("claude".to_owned(), client(CLAUDE, &["claude-*"], false));
    clients.insert("codex".to_owned(), client(CODEX, &["codex-*"], false));
    clients.insert(
        "opencode".to_owned(),
        client(OPENCODE, &["opencode", "opencode-*"], false),
    );
    clients.insert("admin".to_owned(), client(ADMIN, &["*"], true));
    let auth_config = AuthConfig {
        required: true,
        clients,
    };
    let auth = AuthRuntime::new(Some(&auth_config), &EnvMap::new()).unwrap();
    let config = BridgeConfig {
        port: 7447,
        max_message_bytes: 64 * 1024,
        auth: Some(auth_config),
        wake: BTreeMap::new(),
    };
    let secrets = auth.tokens().map(str::to_owned).collect();
    let bridge = Bridge::new(open_in_memory().unwrap(), config, secrets, Arc::new(NoWake));
    let state = AppState {
        bridge: Arc::clone(&bridge),
        auth: Arc::new(auth),
    };
    let mcp = Router::new().route_service(
        "/mcp",
        crate::mcp::service(Arc::clone(&bridge), MAX_BODY_BYTES),
    );
    (router(state, mcp), bridge)
}

/// @brief Gives the current time in milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// @brief Adds a JSON body to a request.
fn with_body(builder: axum::http::request::Builder, body: Option<&Value>) -> HttpRequest<Body> {
    match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

/// @brief Makes a request signed like a hook or a shim, for `session` when given.
pub fn signed(
    method: &str,
    path: &str,
    body: Option<&Value>,
    (client_id, token): (&str, &str),
    session: Option<&str>,
) -> HttpRequest<Body> {
    let url = format!("http://127.0.0.1:7447{path}");
    let headers = sign_request(
        client_id,
        token,
        (method, &url, body),
        now_ms(),
        None,
        session,
    );
    let mut builder = HttpRequest::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:7447");
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    with_body(builder, body)
}

/// @brief Makes a request with a bearer token, like an MCP client.
pub fn bearer(method: &str, path: &str, token: &str, body: Option<&Value>) -> HttpRequest<Body> {
    let builder = HttpRequest::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:7447")
        .header("authorization", format!("Bearer {token}"));
    with_body(builder, body)
}

/// @brief Sends a request, and gives the status and the raw body.
pub async fn call_raw(app: &Router, request: HttpRequest<Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// @brief Sends a request, and gives the status and the JSON body.
pub async fn call(app: &Router, request: HttpRequest<Body>) -> (StatusCode, Value) {
    let (status, body) = call_raw(app, request).await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

/// @brief Runs the Claude Code `SessionStart` hook of a session. It binds the mailbox to the session.
pub async fn start_claude(app: &Router, mailbox: &str, session: &str) -> (StatusCode, Value) {
    let path = format!("/claude/hook?agent={mailbox}&event=SessionStart");
    call(
        app,
        signed("GET", &path, None, CLAUDE_CLIENT, Some(session)),
    )
    .await
}

/// @brief Serves the full app on a loopback port, for the client tests.
///
/// @return The URL and the bus.
pub async fn serve() -> (String, Arc<Bridge>) {
    let (app, bridge) = app();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
        listener, app,
    )));
    (base, bridge)
}

/// @brief Makes a client of the served app.
pub fn daemon_client(base: &str, (client_id, token): (&str, &str)) -> crate::client::Client {
    crate::client::Client::new(base, client_id, Some(token.to_owned())).unwrap()
}
