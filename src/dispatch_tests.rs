use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde_json::Value;

use super::*;
use crate::config::WakeCommon;

const CODEX_SESSION: &str = "019f6767-789c-73b2-bc5c-ac8575f29efd";

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("inband-dispatch-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A fake `codex` that writes one argument per line to `args.txt`, then runs `body`.
fn fake_codex(name: &str, body: &str) -> (PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let log = dir.join("args.txt");
    let script = dir.join("codex");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n{body}\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

fn common(prompt: &str) -> WakeCommon {
    WakeCommon {
        prompt: prompt.to_owned(),
        debounce_seconds: 30,
        max_wakes_per_hour: 20,
    }
}

fn codex_target(command: &FsPath, prompt: &str) -> WakeTarget {
    WakeTarget::Codex {
        command: command.display().to_string(),
        retry_delays_seconds: Vec::new(),
        common: common(prompt),
    }
}

fn input(session: Option<&str>, mailbox: &str, prompt: &str) -> WakeInput {
    WakeInput {
        recipient: mailbox.to_owned(),
        session_id: session.map(str::to_owned),
        mailbox: Some(mailbox.to_owned()),
        prompt: prompt.to_owned(),
    }
}

#[tokio::test]
async fn codex_wake_runs_codex_queue_without_a_shell() {
    let (script, log) = fake_codex("ok", "exit 0");
    let dir = script.parent().unwrap().to_owned();
    // A shell would run the command substitution. The argument list keeps it as text.
    let prompt = format!("mail for {{mailbox}} $(touch {}/pwned)", dir.display());
    let mailbox = format!("codex-{CODEX_SESSION}");
    let result = RealWake::new()
        .unwrap()
        .dispatch(
            &codex_target(&script, &prompt),
            input(Some(CODEX_SESSION), &mailbox, &prompt),
        )
        .await;
    assert_eq!(result.disposition, WakeDisposition::Queued, "{result:?}");
    let args = fs::read_to_string(log).unwrap();
    let expected_prompt = format!("mail for {mailbox} $(touch {}/pwned)", dir.display());
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        [
            "queue",
            "--thread",
            CODEX_SESSION,
            "--message",
            expected_prompt.as_str()
        ]
    );
    assert!(!dir.join("pwned").exists());
}

#[tokio::test]
async fn codex_wake_reports_the_exit_code_and_stderr() {
    let (script, _) = fake_codex("fail", "echo 'unknown thread' >&2\nexit 3");
    let result = RealWake::new()
        .unwrap()
        .dispatch(
            &codex_target(&script, "p"),
            input(Some(CODEX_SESSION), "codex-x", "p"),
        )
        .await;
    assert_eq!(result.disposition, WakeDisposition::Failed);
    assert!(
        result.detail.contains("(3)") && result.detail.contains("unknown thread"),
        "{}",
        result.detail
    );
}

#[tokio::test]
async fn codex_wake_times_out_and_kills_the_cli() {
    let (script, _) = fake_codex("slow", "sleep 5");
    let started = Instant::now();
    let result = RealWake::with_timeout(Duration::from_millis(200))
        .unwrap()
        .dispatch(
            &codex_target(&script, "p"),
            input(Some(CODEX_SESSION), "codex-x", "p"),
        )
        .await;
    assert_eq!(result.disposition, WakeDisposition::Failed);
    assert!(result.detail.contains("timeout"), "{}", result.detail);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn codex_wake_fails_cleanly_without_cli_or_session() {
    let wake = RealWake::new().unwrap();
    let missing = wake
        .dispatch(
            &codex_target(FsPath::new("/nonexistent/codex"), "p"),
            input(Some(CODEX_SESSION), "codex-x", "p"),
        )
        .await;
    assert!(
        missing.detail.contains("cannot start"),
        "{}",
        missing.detail
    );
    let (script, _) = fake_codex("nosession", "exit 0");
    let no_session = wake
        .dispatch(&codex_target(&script, "p"), input(None, "codex-x", "p"))
        .await;
    assert_eq!(no_session.disposition, WakeDisposition::Failed);
}

#[derive(Clone, Default)]
struct FakeOpencode {
    prompts: Arc<Mutex<Vec<(String, Value)>>>,
    sessions: Value,
    fail: bool,
}

async fn list_sessions(State(fake): State<FakeOpencode>) -> Json<Value> {
    Json(fake.sessions.clone())
}

async fn prompt_async(
    State(fake): State<FakeOpencode>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, String) {
    fake.prompts.lock().unwrap().push((id, body));
    if fake.fail {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "session is busy".to_owned(),
        )
    } else {
        (StatusCode::NO_CONTENT, String::new())
    }
}

async fn serve(fake: FakeOpencode) -> String {
    let router = Router::new()
        .route("/session", get(list_sessions))
        .route("/session/{id}/prompt_async", post(prompt_async))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

fn opencode_target(base_url: &str) -> WakeTarget {
    WakeTarget::Opencode {
        base_url: base_url.to_owned(),
        common: common("mail for {mailbox}"),
    }
}

#[tokio::test]
async fn opencode_wake_targets_the_session_of_the_mailbox() {
    let fake = FakeOpencode::default();
    let base = serve(fake.clone()).await;
    let mailbox = crate::opencode_session::mailbox("ses_AbC123").unwrap();
    let result = RealWake::new()
        .unwrap()
        .dispatch(
            &opencode_target(&base),
            input(Some("ses_AbC123"), &mailbox, "mail for {mailbox}"),
        )
        .await;
    assert_eq!(result.disposition, WakeDisposition::Started, "{result:?}");
    let prompts = fake.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].0, "ses_AbC123");
    assert_eq!(
        prompts[0].1["parts"][0]["text"],
        format!("mail for {mailbox}")
    );
}

#[tokio::test]
async fn the_fixed_opencode_mailbox_wakes_the_latest_root_session() {
    let fake = FakeOpencode {
        sessions: serde_json::json!([
            {"id": "ses_child", "parentID": "ses_new", "time": {"updated": 9.0}},
            {"id": "ses_old", "time": {"updated": 1.0}},
            {"id": "ses_new", "time": {"updated": 5.0}}
        ]),
        ..FakeOpencode::default()
    };
    let base = serve(fake.clone()).await;
    let result = RealWake::new()
        .unwrap()
        .dispatch(&opencode_target(&base), input(None, "opencode", "p"))
        .await;
    assert_eq!(result.disposition, WakeDisposition::Started, "{result:?}");
    assert_eq!(fake.prompts.lock().unwrap()[0].0, "ses_new");
}

#[tokio::test]
async fn opencode_wake_reports_errors_and_refuses_unsafe_ids() {
    let fake = FakeOpencode {
        fail: true,
        ..FakeOpencode::default()
    };
    let base = serve(fake.clone()).await;
    let wake = RealWake::new().unwrap();
    let busy = wake
        .dispatch(
            &opencode_target(&base),
            input(Some("ses_x"), "opencode-x", "p"),
        )
        .await;
    assert_eq!(busy.disposition, WakeDisposition::Failed);
    assert!(
        busy.detail.contains("500") && busy.detail.contains("session is busy"),
        "{}",
        busy.detail
    );
    for unsafe_id in ["..", ".", "a/b", "ses x", ""] {
        let refused = wake
            .dispatch(
                &opencode_target(&base),
                input(Some(unsafe_id), "opencode-x", "p"),
            )
            .await;
        assert!(
            refused.detail.contains("invalid OpenCode session id"),
            "{unsafe_id:?}: {}",
            refused.detail
        );
    }
    assert_eq!(
        fake.prompts.lock().unwrap().len(),
        1,
        "unsafe ids must not reach the server"
    );
    let down = wake
        .dispatch(
            &opencode_target("http://127.0.0.1:9"),
            input(Some("ses_x"), "opencode-x", "p"),
        )
        .await;
    assert_eq!(down.disposition, WakeDisposition::Failed);
}
