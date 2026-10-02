use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::Request as HttpRequest;
use tower::ServiceExt;

use super::*;
use crate::auth::sign_request;
use crate::config::{AuthClientConfig, AuthConfig, BridgeConfig, EnvMap, WakeTarget};
use crate::db::open_in_memory;
use crate::wake::{WakeDispatch, WakeFuture, WakeInput, WakeResult};

const CLAUDE: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const CODEX: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CODEX_SESSION: &str = "019f6767-789c-73b2-bc5c-ac8575f29efd";
const OTHER_CODEX_SESSION: &str = "019f6768-789c-73b2-bc5c-ac8575f29efd";

struct NoWake;

impl WakeDispatch for NoWake {
    fn dispatch(&self, _target: &WakeTarget, _input: WakeInput) -> WakeFuture {
        Box::pin(async { WakeResult::failed("no wake in tests") })
    }
}

fn client(token: &str, agents: &[&str], admin: bool) -> AuthClientConfig {
    AuthClientConfig {
        token: Some(token.to_owned()),
        token_env: None,
        agents: agents.iter().map(|a| (*a).to_owned()).collect(),
        directory: None,
        admin,
    }
}

fn app() -> (Router, Arc<Bridge>) {
    let mut clients = BTreeMap::new();
    clients.insert("claude".to_owned(), client(CLAUDE, &["claude-*"], false));
    clients.insert("codex".to_owned(), client(CODEX, &["codex-*"], false));
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
    (router(state, Router::new()), bridge)
}

/// A request signed like a hook does, for `session` when it is set.
fn signed(
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
    match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

fn bearer(method: &str, path: &str, token: &str, body: Option<&Value>) -> HttpRequest<Body> {
    let builder = HttpRequest::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:7447")
        .header("authorization", format!("Bearer {token}"));
    match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

async fn call(app: &Router, request: HttpRequest<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

const CLAUDE_CLIENT: (&str, &str) = ("claude", CLAUDE);
const CODEX_CLIENT: (&str, &str) = ("codex", CODEX);

async fn start_claude(app: &Router, mailbox: &str, session: &str) -> (StatusCode, Value) {
    let path = format!("/claude/hook?agent={mailbox}&event=SessionStart");
    call(
        app,
        signed("GET", &path, None, CLAUDE_CLIENT, Some(session)),
    )
    .await
}

#[tokio::test]
async fn refuses_other_hosts_and_browser_origins() {
    let (app, _) = app();
    for host in [
        "evil.example:7447",
        "127.0.0.1.evil.example",
        "localhost.evil:7447",
    ] {
        let mut request = bearer("GET", "/health", ADMIN, None);
        request
            .headers_mut()
            .insert(header::HOST, host.parse().unwrap());
        assert_eq!(call(&app, request).await.0, StatusCode::FORBIDDEN, "{host}");
    }
    let mut browser = bearer("GET", "/health", ADMIN, None);
    browser
        .headers_mut()
        .insert(header::ORIGIN, "http://evil.example".parse().unwrap());
    assert_eq!(call(&app, browser).await.0, StatusCode::FORBIDDEN);

    for host in [
        "localhost:7447",
        "127.0.0.1",
        "[::1]:7447",
        "LOCALHOST:9000",
    ] {
        let mut request = bearer("GET", "/health", ADMIN, None);
        request
            .headers_mut()
            .insert(header::HOST, host.parse().unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{host}");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
}

#[tokio::test]
async fn refuses_unauthenticated_requests() {
    let (app, _) = app();
    let anonymous = HttpRequest::builder()
        .uri("/health")
        .header("host", "127.0.0.1:7447")
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(&app, anonymous).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], -32001);
    assert_eq!(
        call(&app, bearer("GET", "/health", "nope", None)).await.0,
        StatusCode::UNAUTHORIZED
    );
    // A signature for another path does not open this one.
    let mut moved = signed("GET", "/health", None, CLAUDE_CLIENT, None);
    *moved.uri_mut() = "/subscribe?mailbox=claude-api-a1b2".parse().unwrap();
    assert_eq!(call(&app, moved).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn health_is_full_for_the_admin_and_minimal_for_others() {
    let (app, _) = app();
    start_claude(&app, "claude-api-a1b2", "sess-a").await;
    let (status, full) = call(&app, bearer("GET", "/health", ADMIN, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(full["ok"], true);
    assert_eq!(full["agents"][0]["name"], "claude-api-a1b2");
    let (status, minimal) = call(&app, bearer("GET", "/health", CLAUDE, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(minimal["ok"], true);
    assert!(minimal.get("agents").is_none(), "{minimal}");
}

#[tokio::test]
async fn claude_session_start_binds_the_mailbox_and_returns_the_protocol() {
    let (app, _) = app();
    let (status, body) = start_claude(&app, "claude-api-a1b2", "sess-a").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let context = body["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        context.contains("`claude-api-a1b2`") && context.contains("not in an InBand team"),
        "{context}"
    );

    let (status, _) = start_claude(&app, "claude-api-a1b2", "sess-b").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "another session takes a bound mailbox"
    );
    let unsigned = bearer(
        "GET",
        "/claude/hook?agent=claude-api-a1b2&event=SessionStart",
        CLAUDE,
        None,
    );
    assert_eq!(call(&app, unsigned).await.0, StatusCode::FORBIDDEN);
    let (status, _) = start_claude(&app, "claude-api-a1b2", "sess-a").await;
    assert_eq!(status, StatusCode::OK, "the same session can start again");
}

async fn team_of_two(app: &Router) {
    start_claude(app, "claude-lead-0001", "sess-lead").await;
    start_claude(app, "claude-w-0002", "sess-w").await;
    let join = json!({"mailbox": "claude-w-0002", "team": "x"});
    let (status, _) = call(
        app,
        signed(
            "POST",
            "/team/join",
            Some(&join),
            CLAUDE_CLIENT,
            Some("sess-w"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let lead = json!({"mailbox": "claude-lead-0001", "team": "x"});
    let (status, body) = call(
        app,
        signed(
            "POST",
            "/team/lead",
            Some(&lead),
            CLAUDE_CLIENT,
            Some("sess-lead"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["protocol"]
            .as_str()
            .unwrap()
            .contains("is the lead of team `x`")
    );
}

#[tokio::test]
async fn team_commands_need_the_signed_session() {
    let (app, _) = app();
    start_claude(&app, "claude-lead-0001", "sess-lead").await;
    let lead = json!({"mailbox": "claude-lead-0001", "team": "x"});
    let (status, _) = call(&app, bearer("POST", "/team/lead", CLAUDE, Some(&lead))).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a bearer token speaks for no session"
    );
    let (status, _) = call(
        &app,
        signed(
            "POST",
            "/team/lead",
            Some(&lead),
            CLAUDE_CLIENT,
            Some("sess-other"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    team_of_two(&app).await;
    let leave = json!({"mailbox": "claude-w-0002"});
    let (status, body) = call(
        &app,
        signed(
            "POST",
            "/team/leave",
            Some(&leave),
            CLAUDE_CLIENT,
            Some("sess-w"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["change"]["role"], "solo");
}

#[tokio::test]
async fn post_tool_use_and_subscribe_serve_only_the_own_session() {
    let (app, bridge) = app();
    team_of_two(&app).await;
    let lead = Caller {
        auth: crate::auth::AuthInfo {
            client_id: "claude".to_owned(),
            agents: vec!["claude-*".to_owned()],
            directory: vec!["claude-*".to_owned()],
            admin: false,
            mode: crate::auth::AuthMode::Hmac,
        },
        session: Some("sess-lead".to_owned()),
    };
    bridge
        .send(&lead, "claude-lead-0001", "claude-w-0002", "run the tests")
        .unwrap();

    let hook = "/claude/hook?agent=claude-w-0002&event=PostToolUse";
    let (status, body) = call(
        &app,
        signed("GET", hook, None, CLAUDE_CLIENT, Some("sess-w")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("1 unread")
    );
    let (status, _) = call(
        &app,
        signed("GET", hook, None, CLAUDE_CLIENT, Some("sess-lead")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let subscribe = "/subscribe?mailbox=claude-w-0002&after_id=0&timeout=1";
    let (status, body) = call(
        &app,
        signed("GET", subscribe, None, CLAUDE_CLIENT, Some("sess-w")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["messages"][0]["content"], "run the tests");
    assert_eq!(body["messages"][0]["sender_role"], "lead");
    let (status, _) = call(
        &app,
        signed("GET", subscribe, None, CLAUDE_CLIENT, Some("sess-lead")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the lead cannot read the worker's mail"
    );
    let bad = "/subscribe?mailbox=claude-w-0002&after_id=-1";
    assert_eq!(
        call(
            &app,
            signed("GET", bad, None, CLAUDE_CLIENT, Some("sess-w"))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let family = "/subscribe?prefix=claude&timeout=1";
    assert_eq!(
        call(
            &app,
            signed("GET", family, None, CLAUDE_CLIENT, Some("sess-w"))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn presence_is_set_by_the_own_session_only() {
    let (app, _) = app();
    team_of_two(&app).await;
    let offline = json!({"agent": "claude-lead-0001", "online": false});
    let (status, _) = call(
        &app,
        signed(
            "POST",
            "/presence",
            Some(&offline),
            CLAUDE_CLIENT,
            Some("sess-w"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &app,
        signed(
            "POST",
            "/presence",
            Some(&offline),
            CLAUDE_CLIENT,
            Some("sess-lead"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn codex_hooks_need_the_session_of_the_payload() {
    let (app, _) = app();
    let start = json!({"hook_event_name": "SessionStart", "session_id": CODEX_SESSION, "cwd": "/repo", "source": "startup"});
    let (status, _) = call(
        &app,
        signed(
            "POST",
            "/codex/hook",
            Some(&start),
            CODEX_CLIENT,
            Some(OTHER_CODEX_SESSION),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a session registers only itself"
    );
    let (status, _) = call(&app, bearer("POST", "/codex/hook", CODEX, Some(&start))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(
        &app,
        signed(
            "POST",
            "/codex/hook",
            Some(&start),
            CODEX_CLIENT,
            Some(CODEX_SESSION),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let context = body["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        context.contains(&format!("`codex-{CODEX_SESSION}`")),
        "{context}"
    );

    let stop =
        json!({"hook_event_name": "Stop", "session_id": CODEX_SESSION, "stop_hook_active": false});
    let (status, body) = call(
        &app,
        signed(
            "POST",
            "/codex/hook",
            Some(&stop),
            CODEX_CLIENT,
            Some(CODEX_SESSION),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["continue"], true);
    let bad = json!({"hook_event_name": "SessionStart", "session_id": CODEX_SESSION, "cwd": "/repo", "source": "evil"});
    let (status, _) = call(
        &app,
        signed(
            "POST",
            "/codex/hook",
            Some(&bad),
            CODEX_CLIENT,
            Some(CODEX_SESSION),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn oversized_bodies_are_refused_before_authentication() {
    let (app, _) = app();
    let huge = "x".repeat(MAX_BODY_BYTES + 1);
    let request = HttpRequest::builder()
        .method("POST")
        .uri("/presence")
        .header("host", "127.0.0.1:7447")
        .header("authorization", format!("Bearer {CLAUDE}"))
        .body(Body::from(huge))
        .unwrap();
    assert_eq!(call(&app, request).await.0, StatusCode::PAYLOAD_TOO_LARGE);
}
