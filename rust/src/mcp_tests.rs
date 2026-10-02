use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request as HttpRequest;
use serde_json::{Value, json};

use crate::bridge::Bridge;
use crate::test_support::*;

/// The JSON-RPC messages of an SSE or JSON response body.
fn messages(body: &str) -> Vec<Value> {
    let events: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data.trim()).ok())
        .collect();
    if events.is_empty() {
        serde_json::from_str(body).into_iter().collect()
    } else {
        events
    }
}

fn tool_call(name: &str, arguments: &Value, meta: Option<&Value>) -> Value {
    let mut params = json!({ "name": name, "arguments": arguments });
    if let Some(meta) = meta {
        params["_meta"] = meta.clone();
    }
    json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": params })
}

fn with_protocol(mut request: HttpRequest<Body>) -> HttpRequest<Body> {
    request
        .headers_mut()
        .insert("mcp-protocol-version", "2025-06-18".parse().unwrap());
    request
}

/// The result of a tool call: whether it is an error, and its JSON text.
async fn run(app: &Router, request: HttpRequest<Body>) -> (bool, Value) {
    let (_, body) = call_raw(app, with_protocol(request)).await;
    let reply = messages(&body)
        .into_iter()
        .find(|message| message.get("id").is_some())
        .unwrap_or_else(|| panic!("no response in {body}"));
    let result = &reply["result"];
    let text = result["content"][0]["text"].as_str().unwrap_or("null");
    let value = serde_json::from_str(text)
        .or_else(|_| toon_format::decode_default::<Value>(text))
        .unwrap_or(Value::Null);
    (result["isError"].as_bool().unwrap_or(false), value)
}

/// The raw text of a tool result.
async fn run_text(app: &Router, request: HttpRequest<Body>) -> String {
    let (_, body) = call_raw(app, with_protocol(request)).await;
    let reply = messages(&body)
        .into_iter()
        .find(|message| message.get("id").is_some())
        .unwrap();
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn codex_mailbox(session: &str) -> String {
    format!("codex-{session}")
}

/// Claude lead `claude-lead-0001` (session `sess-lead`), with the Codex session `CODEX_SESSION` as a
/// worker of team `x`. Every step goes through the HTTP routes, signed like the real hooks.
async fn team_with_codex(app: &Router) {
    start_claude(app, "claude-lead-0001", "sess-lead").await;
    let start = json!({"hook_event_name": "SessionStart", "session_id": CODEX_SESSION, "cwd": "/repo", "source": "startup"});
    let (status, _) = call(
        app,
        signed(
            "POST",
            "/codex/hook",
            Some(&start),
            CODEX_CLIENT,
            Some(CODEX_SESSION),
        ),
    )
    .await;
    assert!(status.is_success());
    let join = json!({"mailbox": codex_mailbox(CODEX_SESSION), "team": "x"});
    let (status, body) = call(
        app,
        signed(
            "POST",
            "/team/join",
            Some(&join),
            CODEX_CLIENT,
            Some(CODEX_SESSION),
        ),
    )
    .await;
    assert!(status.is_success(), "{body}");
    let lead = json!({"mailbox": "claude-lead-0001", "team": "x"});
    let (status, _) = call(
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
    assert!(status.is_success());
}

#[tokio::test]
async fn the_tool_list_has_no_way_to_take_the_lead() {
    let (app, _) = app();
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}});
    let (_, body) = call_raw(
        &app,
        with_protocol(bearer("POST", "/mcp", CODEX, Some(&list))),
    )
    .await;
    let reply = messages(&body).remove(0);
    let mut names: Vec<&str> = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "clear_conversation",
            "get_history",
            "get_messages",
            "ping",
            "send_message",
            "wait_for_messages"
        ]
    );
}

#[tokio::test]
async fn codex_speaks_with_the_session_that_codex_puts_in_meta() {
    let (app, _) = app();
    team_with_codex(&app).await;
    let mailbox = codex_mailbox(CODEX_SESSION);
    let args = json!({"from": mailbox, "to": "claude-lead-0001", "content": "result"});

    let own = json!({"sessionId": CODEX_SESSION, "progressToken": 1});
    let (is_error, sent) = run(
        &app,
        bearer(
            "POST",
            "/mcp",
            CODEX,
            Some(&tool_call("send_message", &args, Some(&own))),
        ),
    )
    .await;
    assert!(!is_error, "{sent}");
    assert_eq!(sent["deliveredTo"][0], "claude-lead-0001");

    let other = json!({"sessionId": OTHER_CODEX_SESSION});
    let (is_error, refused) = run(
        &app,
        bearer(
            "POST",
            "/mcp",
            CODEX,
            Some(&tool_call("send_message", &args, Some(&other))),
        ),
    )
    .await;
    assert!(is_error, "another session id: {refused}");

    let (is_error, refused) = run(
        &app,
        bearer(
            "POST",
            "/mcp",
            CODEX,
            Some(&tool_call("send_message", &args, None)),
        ),
    )
    .await;
    assert!(is_error, "no session id: {refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("signed by its own session"),
        "{refused}"
    );

    // The model writes the arguments, not _meta: a session id in the arguments changes nothing.
    let smuggled = json!({"from": mailbox, "to": "claude-lead-0001", "content": "x", "_meta": {"sessionId": CODEX_SESSION}, "sessionId": CODEX_SESSION});
    let (is_error, refused) = run(
        &app,
        bearer(
            "POST",
            "/mcp",
            CODEX,
            Some(&tool_call("send_message", &smuggled, None)),
        ),
    )
    .await;
    assert!(is_error, "session id smuggled in the arguments: {refused}");
}

#[tokio::test]
async fn a_claude_bearer_call_speaks_for_no_session() {
    let (app, _) = app();
    team_with_codex(&app).await;
    let args =
        json!({"from": "claude-lead-0001", "to": codex_mailbox(CODEX_SESSION), "content": "obey"});
    // Claude Code sends no session in _meta, so its bearer calls cannot act for a bound mailbox.
    let (is_error, refused) = run(
        &app,
        bearer(
            "POST",
            "/mcp",
            CLAUDE,
            Some(&tool_call("send_message", &args, None)),
        ),
    )
    .await;
    assert!(is_error, "{refused}");
    // A session id for another session is refused too.
    let other = json!({"sessionId": "sess-other"});
    let (is_error, refused) = run(
        &app,
        bearer(
            "POST",
            "/mcp",
            CLAUDE,
            Some(&tool_call("send_message", &args, Some(&other))),
        ),
    )
    .await;
    assert!(is_error, "{refused}");
}

#[tokio::test]
async fn a_signed_session_reads_only_its_own_mail() {
    let (app, _) = app();
    team_with_codex(&app).await;
    let mailbox = codex_mailbox(CODEX_SESSION);
    let task = json!({"from": "claude-lead-0001", "to": mailbox, "content": "run the tests"});
    let (is_error, sent) = run(
        &app,
        signed(
            "POST",
            "/mcp",
            Some(&tool_call("send_message", &task, None)),
            CLAUDE_CLIENT,
            Some("sess-lead"),
        ),
    )
    .await;
    assert!(!is_error, "{sent}");

    let read = json!({"for": mailbox});
    let (is_error, stolen) = run(
        &app,
        signed(
            "POST",
            "/mcp",
            Some(&tool_call("get_messages", &read, None)),
            CLAUDE_CLIENT,
            Some("sess-lead"),
        ),
    )
    .await;
    assert!(is_error, "the lead read the worker's mail: {stolen}");
    let meta = json!({"sessionId": CODEX_SESSION});
    let (is_error, mail) = run(
        &app,
        bearer(
            "POST",
            "/mcp",
            CODEX,
            Some(&tool_call("get_messages", &read, Some(&meta))),
        ),
    )
    .await;
    assert!(!is_error, "{mail}");
    assert_eq!(mail["messages"][0]["content"], "run the tests");
    assert_eq!(mail["messages"][0]["sender_role"], "lead");
    // Messages stay compact JSON: every message names its sender with a key.
    let text = run_text(
        &app,
        bearer(
            "POST",
            "/mcp",
            CODEX,
            Some(&tool_call("get_messages", &read, Some(&meta))),
        ),
    )
    .await;
    assert!(text.starts_with('{') && !text.contains("\n  "), "{text}");
}

#[tokio::test]
async fn ping_and_clear_follow_the_caller() {
    let (app, _) = app();
    team_with_codex(&app).await;
    let ping = json!({"from": "claude-lead-0001"});
    let (is_error, status) = run(
        &app,
        signed(
            "POST",
            "/mcp",
            Some(&tool_call("ping", &ping, None)),
            CLAUDE_CLIENT,
            Some("sess-lead"),
        ),
    )
    .await;
    assert!(!is_error, "{status}");
    assert_eq!(status["team"], "x");
    assert_eq!(status["agents"].as_array().unwrap().len(), 2);
    // Uniform agent rows give a TOON table: one header, one line per agent.
    let text = run_text(
        &app,
        signed(
            "POST",
            "/mcp",
            Some(&tool_call("ping", &ping, None)),
            CLAUDE_CLIENT,
            Some("sess-lead"),
        ),
    )
    .await;
    assert!(text.contains("agents[2]{"), "{text}");

    let wipe = json!({"confirm": "wipe"});
    let (is_error, _) = run(
        &app,
        signed(
            "POST",
            "/mcp",
            Some(&tool_call("clear_conversation", &wipe, None)),
            CLAUDE_CLIENT,
            Some("sess-lead"),
        ),
    )
    .await;
    assert!(is_error);
    let (is_error, cleared) = run(
        &app,
        bearer(
            "POST",
            "/mcp",
            ADMIN,
            Some(&tool_call("clear_conversation", &wipe, None)),
        ),
    )
    .await;
    assert!(!is_error, "{cleared}");
}

async fn send_later(bridge: Arc<Bridge>, to: String) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    let lead = crate::bridge::Caller {
        auth: crate::auth::AuthInfo::disabled(),
        session: None,
    };
    bridge
        .send(&lead, "claude-lead-0001", &to, "late task")
        .unwrap();
}

#[tokio::test]
async fn wait_for_messages_beats_then_returns_a_preview() {
    let (app, bridge) = app();
    team_with_codex(&app).await;
    let mailbox = codex_mailbox(CODEX_SESSION);
    tokio::spawn(send_later(Arc::clone(&bridge), mailbox.clone()));
    let wait = json!({"for": mailbox, "timeout_seconds": 10});
    let meta = json!({"sessionId": CODEX_SESSION, "progressToken": "p1"});
    let request = with_protocol(bearer(
        "POST",
        "/mcp",
        CODEX,
        Some(&tool_call("wait_for_messages", &wait, Some(&meta))),
    ));
    let (_, body) = call_raw(&app, request).await;
    let events = messages(&body);
    let beat = events
        .iter()
        .position(|event| event["method"] == "notifications/progress")
        .unwrap_or_else(|| panic!("no heartbeat in {body}"));
    assert_eq!(events[beat]["params"]["progressToken"], "p1");
    let reply = events
        .iter()
        .position(|event| event.get("result").is_some())
        .unwrap();
    assert!(beat < reply, "the heartbeat comes first");
    let text = events[reply]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let preview: Value = serde_json::from_str(text).unwrap();
    assert_eq!(preview["messages"][0]["content"], "late task");
    // A preview does not consume the mail.
    let read = json!({"for": mailbox});
    let meta = json!({"sessionId": CODEX_SESSION});
    let (_, mail) = run(
        &app,
        bearer(
            "POST",
            "/mcp",
            CODEX,
            Some(&tool_call("get_messages", &read, Some(&meta))),
        ),
    )
    .await;
    assert_eq!(mail["messages"].as_array().unwrap().len(), 1);
}
