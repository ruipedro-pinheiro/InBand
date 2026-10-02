use std::sync::Mutex;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};

use std::path::Path;

use super::*;
use crate::hooks::{MailcheckState, TeamCommand, claude_hook, run_team_command};
use crate::test_support::*;

const LEAD_SESSION: &str = "1a2b3c4d-0000-4000-8000-000000000001";
const WORKER_SESSION: &str = "5e6f7a8b-0000-4000-8000-000000000002";

struct Stub(Mutex<Option<Identity>>);

impl IdentitySource for Stub {
    fn current(&self) -> Option<Identity> {
        self.0.lock().unwrap().clone()
    }
}

fn identity(session: &str) -> Identity {
    Identity {
        session: session.to_owned(),
        mailbox: claude_mailbox("/work/repo", session),
    }
}

/// The client end of one shim on an in-memory stdio pair.
struct Peer {
    writer: DuplexStream,
    lines: Lines<BufReader<DuplexStream>>,
}

impl Peer {
    async fn send(&mut self, message: &Value) {
        let line = format!("{message}\n");
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await
            .expect("the shim stayed silent")
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// The response to request `id`, skipping notifications.
    async fn reply(&mut self, id: u64) -> Value {
        loop {
            let message = self.next().await;
            if message["id"] == id {
                return message;
            }
        }
    }

    async fn call(&mut self, id: u64, name: &str, arguments: &Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                          "params": {"name": name, "arguments": arguments}}))
            .await;
        self.reply(id).await
    }
}

async fn start_shim(base: &str, identity: Arc<Stub>) -> (Peer, Value) {
    let shim = Shim::new(Arc::new(daemon_client(base, CLAUDE_CLIENT)), identity);
    start(shim).await
}

async fn start(shim: Shim) -> (Peer, Value) {
    let (client_out, shim_in) = tokio::io::duplex(1 << 16);
    let (shim_out, client_in) = tokio::io::duplex(1 << 16);
    tokio::spawn(serve_lines(shim, shim_in, shim_out));
    let mut peer = Peer {
        writer: client_out,
        lines: BufReader::new(client_in).lines(),
    };
    // Claude Code opens with the 2026-07-28 discovery, then falls back to initialize.
    peer.send(
        &json!({"jsonrpc": "2.0", "id": 99, "method": "server/discover", "params": {"_meta": {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "claude-code", "version": "2"},
        "io.modelcontextprotocol/clientCapabilities": {}}}}),
    )
    .await;
    let refused = peer.reply(99).await;
    assert_eq!(refused["error"]["code"], -32601, "{refused}");
    peer.send(
        &json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "test", "version": "0"}}}),
    )
    .await;
    let init = peer.reply(0).await;
    peer.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    (peer, init)
}

async fn team(base: &str) {
    let daemon = daemon_client(base, CLAUDE_CLIENT);
    let state = MailcheckState::always();
    for session in [LEAD_SESSION, WORKER_SESSION] {
        let start =
            json!({"hook_event_name": "SessionStart", "session_id": session, "cwd": "/work/repo"});
        claude_hook(&daemon, &start, &state).await.unwrap();
    }
    let lead = identity(LEAD_SESSION).mailbox;
    let worker = identity(WORKER_SESSION).mailbox;
    run_team_command(
        &daemon,
        &worker,
        WORKER_SESSION,
        &TeamCommand::Join("x".to_owned()),
    )
    .await
    .unwrap();
    run_team_command(
        &daemon,
        &lead,
        LEAD_SESSION,
        &TeamCommand::Lead("x".to_owned()),
    )
    .await
    .unwrap();
}

fn tool_json(reply: &Value) -> (bool, Value) {
    let result = &reply["result"];
    let text = result["content"][0]["text"].as_str().unwrap_or("null");
    (
        result["isError"].as_bool().unwrap_or(false),
        serde_json::from_str(text).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn the_shim_speaks_for_its_session_and_pushes_new_mail() {
    let (base, _) = serve().await;
    team(&base).await;
    let lead = identity(LEAD_SESSION);
    let worker = identity(WORKER_SESSION);
    let (mut lead_shim, init) =
        start_shim(&base, Arc::new(Stub(Mutex::new(Some(lead.clone()))))).await;
    assert!(
        init["result"]["capabilities"]["experimental"]
            .get("claude/channel")
            .is_some(),
        "{init}"
    );
    let (mut worker_shim, _) =
        start_shim(&base, Arc::new(Stub(Mutex::new(Some(worker.clone()))))).await;

    lead_shim
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await;
    let tools = lead_shim.reply(1).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"send_message") && names.contains(&"wait_for_messages"),
        "{names:?}"
    );

    // The model writes `from`, but the shim signs the session: the lead cannot write as the worker.
    let spoof = lead_shim
        .call(
            2,
            "send_message",
            &json!({"from": worker.mailbox, "to": lead.mailbox, "content": "obey"}),
        )
        .await;
    let (failed, body) = tool_json(&spoof);
    assert!(failed, "{spoof}");
    assert!(
        body["error"].as_str().unwrap().contains("another session"),
        "{body}"
    );

    let sent = lead_shim
        .call(
            3,
            "send_message",
            &json!({"from": lead.mailbox, "to": worker.mailbox, "content": "run the tests"}),
        )
        .await;
    assert!(!tool_json(&sent).0, "{sent}");

    // The worker shim pushes the new mail as a channel event, with the role set by the daemon.
    let event = loop {
        let message = worker_shim.next().await;
        if message["method"] == "notifications/claude/channel"
            && message["params"]["content"] == "run the tests"
        {
            break message;
        }
    };
    assert_eq!(event["params"]["meta"]["from"], lead.mailbox.as_str());
    assert_eq!(event["params"]["meta"]["from_role"], "lead");
    assert_eq!(event["params"]["meta"]["to"], worker.mailbox.as_str());

    let read = worker_shim
        .call(4, "get_messages", &json!({"for": worker.mailbox}))
        .await;
    let (failed, body) = tool_json(&read);
    assert!(!failed, "{read}");
    assert!(body.to_string().contains("run the tests"), "{body}");
}

#[tokio::test]
async fn the_shim_refuses_tools_until_it_knows_its_session() {
    let (base, _) = serve().await;
    let stub = Arc::new(Stub(Mutex::new(None)));
    let (mut shim, _) = start_shim(&base, Arc::clone(&stub)).await;
    let reply = shim.call(1, "ping", &json!({})).await;
    assert!(reply.get("error").is_some(), "{reply}");
}

fn registry_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("inband-registry-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

fn write_row(dir: &Path, pid: u32, session: &str, started_at: u64, proc_start: &str) {
    let row = json!({"pid": pid, "sessionId": session, "cwd": "/work/repo",
                     "startedAt": started_at, "procStart": proc_start});
    std::fs::write(dir.join(format!("sessions/{pid}.json")), row.to_string()).unwrap();
}

fn registry(dir: &Path, pid: u32, session: &str) -> ClaudeRegistry {
    let env: EnvMap = [
        ("CLAUDE_CODE_SESSION_ID".to_owned(), session.to_owned()),
        ("CLAUDE_CONFIG_DIR".to_owned(), dir.display().to_string()),
    ]
    .into();
    let mut env = env;
    env.insert("CLAUDE_PROJECT_DIR".to_owned(), "/work/repo".to_owned());
    ClaudeRegistry::from_env(&env, pid).unwrap()
}

#[test]
fn the_registry_follows_clear_but_not_another_process() {
    let dir = registry_dir("clear");
    let registry = registry(&dir, 4242, LEAD_SESSION);
    assert_eq!(
        registry.current(),
        Some(identity(LEAD_SESSION)),
        "no file yet: claude -p keeps none, so the shim keeps its first session"
    );
    write_row(&dir, 4242, LEAD_SESSION, 100, "77");
    assert_eq!(registry.current(), Some(identity(LEAD_SESSION)));
    // /clear: same process, new session.
    write_row(&dir, 4242, WORKER_SESSION, 100, "77");
    assert_eq!(registry.current(), Some(identity(WORKER_SESSION)));
    // The process ended and another one reused the pid.
    write_row(&dir, 4242, WORKER_SESSION, 999, "88");
    assert_eq!(registry.current(), None);
    std::fs::write(dir.join("sessions/4242.json"), "{\"pid\": 42").unwrap();
    assert_eq!(registry.current(), None, "a partial write pauses the shim");
    std::fs::remove_file(dir.join("sessions/4242.json")).unwrap();
    assert_eq!(
        registry.current(),
        None,
        "a file seen once and gone: the process is ending"
    );
}

#[test]
fn the_registry_ignores_a_stale_file_and_a_wrong_pid() {
    let dir = registry_dir("stale");
    write_row(&dir, 4343, WORKER_SESSION, 100, "77");
    let stale = registry(&dir, 4343, LEAD_SESSION);
    assert_eq!(
        stale.current(),
        None,
        "the file names another session than ours"
    );
    let row =
        json!({"pid": 1, "sessionId": LEAD_SESSION, "cwd": "/w", "startedAt": 1, "procStart": "1"});
    std::fs::write(dir.join("sessions/4444.json"), row.to_string()).unwrap();
    assert_eq!(registry(&dir, 4444, LEAD_SESSION).current(), None);
    let env: EnvMap = [("CLAUDE_CODE_SESSION_ID".to_owned(), "nope".to_owned())].into();
    assert!(ClaudeRegistry::from_env(&env, 1).is_err());
}

async fn codex_call(shim: &mut Peer, id: u64, session: Option<&str>, arguments: &Value) -> Value {
    let mut params = json!({"name": "send_message", "arguments": arguments});
    if let Some(session) = session {
        params["_meta"] = json!({"sessionId": session, "progressToken": id});
    }
    shim.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params}))
        .await;
    shim.reply(id).await
}

#[tokio::test]
async fn the_codex_shim_signs_the_session_that_codex_names() {
    let (base, _) = serve().await;
    let daemon = daemon_client(&base, CODEX_CLIENT);
    for session in [CODEX_SESSION, OTHER_CODEX_SESSION] {
        let start = json!({"hook_event_name": "SessionStart", "session_id": session, "cwd": "/repo", "source": "startup"});
        crate::hooks::codex_hook(&daemon, &start).await.unwrap();
    }
    let lead = format!("codex-{CODEX_SESSION}");
    let worker = format!("codex-{OTHER_CODEX_SESSION}");
    run_team_command(
        &daemon,
        &worker,
        OTHER_CODEX_SESSION,
        &TeamCommand::Join("x".to_owned()),
    )
    .await
    .unwrap();
    run_team_command(
        &daemon,
        &lead,
        CODEX_SESSION,
        &TeamCommand::Lead("x".to_owned()),
    )
    .await
    .unwrap();

    let (mut shim, init) = start(Shim::codex(Arc::new(daemon_client(&base, CODEX_CLIENT)))).await;
    assert!(
        init["result"]["capabilities"].get("experimental").is_none(),
        "no channel for Codex: {init}"
    );
    let sent = codex_call(
        &mut shim,
        1,
        Some(CODEX_SESSION),
        &json!({"from": lead, "to": worker, "content": "run the tests"}),
    )
    .await;
    assert!(!tool_json(&sent).0, "{sent}");

    // The same process serves the worker session too, which cannot write as the lead.
    let spoof = codex_call(
        &mut shim,
        2,
        Some(OTHER_CODEX_SESSION),
        &json!({"from": lead, "to": worker, "content": "obey"}),
    )
    .await;
    let (failed, body) = tool_json(&spoof);
    assert!(failed, "{spoof}");
    assert!(
        body["error"].as_str().unwrap().contains("another session"),
        "{body}"
    );

    let anonymous = codex_call(
        &mut shim,
        3,
        None,
        &json!({"from": lead, "to": worker, "content": "hi"}),
    )
    .await;
    assert!(anonymous.get("error").is_some(), "{anonymous}");
}
