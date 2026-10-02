use std::os::unix::fs::PermissionsExt;

use serde_json::{Value, json};

use super::*;
use crate::test_support::*;

const LEAD_SESSION: &str = "1a2b3c4d-0000-4000-8000-000000000001";
const WORKER_SESSION: &str = "5e6f7a8b-0000-4000-8000-000000000002";

/// The JSON of a Claude Code hook, in `/work/My Repo`.
fn claude(event: &str, session: &str, extra: &Value) -> Value {
    let mut payload =
        json!({ "hook_event_name": event, "session_id": session, "cwd": "/work/My Repo" });
    for (key, value) in extra.as_object().unwrap() {
        payload[key] = value.clone();
    }
    payload
}

fn context_of(output: &Value) -> &str {
    output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("no context in {output}"))
}

#[test]
fn parses_only_whole_prompt_team_commands() {
    assert_eq!(
        parse_team_command("/lead x"),
        Some(Ok(TeamCommand::Lead("x".to_owned())))
    );
    assert_eq!(
        parse_team_command("  $join  web \n"),
        Some(Ok(TeamCommand::Join("web".to_owned())))
    );
    assert_eq!(parse_team_command("/solo"), Some(Ok(TeamCommand::Solo)));
    assert!(matches!(parse_team_command("/lead"), Some(Err(_))));
    assert!(matches!(parse_team_command("/lead a b"), Some(Err(_))));
    assert!(matches!(parse_team_command("/solo now"), Some(Err(_))));
    for prompt in [
        "lead x",
        "please /lead x",
        "/leader x",
        "/lead x\nand more",
        "/help",
        "",
    ] {
        assert_eq!(parse_team_command(prompt), None, "{prompt:?}");
    }
}

#[test]
fn names_claude_mailboxes_like_v1() {
    assert_eq!(
        claude_mailbox("/work/My Repo/", "ABCD1234-0000-4000-8000-000000000000"),
        "claude-my-repo-abcd"
    );
    assert_eq!(claude_mailbox("/", "zz"), "claude-root-0000");
    assert_eq!(claude_mailbox("/w/__", "1"), "claude-dir-1");
    assert_eq!(
        claude_mailbox("/a/a-very-long-directory-name-here", "0f0f"),
        "claude-a-very-long-director-0f0f"
    );
}

/// A team command before `SessionStart` is refused: the mailbox is not bound to the session yet.
#[tokio::test]
async fn claude_hooks_bind_the_session_and_run_team_commands() {
    let (base, bridge) = serve().await;
    let daemon = daemon_client(&base, CLAUDE_CLIENT);
    let state = MailcheckState::always();
    let lead = claude_mailbox("/work/My Repo", LEAD_SESSION);
    let worker = claude_mailbox("/work/My Repo", WORKER_SESSION);

    let early = claude_hook(
        &daemon,
        &claude(
            "UserPromptSubmit",
            LEAD_SESSION,
            &json!({"prompt": "/lead x"}),
        ),
        &state,
    )
    .await
    .unwrap();
    assert_eq!(early["decision"], "block", "{early}");

    for session in [LEAD_SESSION, WORKER_SESSION] {
        let start = claude_hook(
            &daemon,
            &claude("SessionStart", session, &json!({})),
            &state,
        )
        .await
        .unwrap();
        assert!(
            context_of(&start).contains("not in an InBand team"),
            "{start}"
        );
    }
    let lead_out = claude_hook(
        &daemon,
        &claude(
            "UserPromptSubmit",
            LEAD_SESSION,
            &json!({"prompt": "/lead x"}),
        ),
        &state,
    )
    .await
    .unwrap();
    let text = context_of(&lead_out);
    assert!(
        text.contains(&format!("`{lead}` is now the lead of team `x`")),
        "{text}"
    );
    assert!(
        text.contains("is the lead of team `x`"),
        "the new protocol follows: {text}"
    );
    let join = claude_hook(
        &daemon,
        &claude(
            "UserPromptSubmit",
            WORKER_SESSION,
            &json!({"prompt": "/join x"}),
        ),
        &state,
    )
    .await
    .unwrap();
    assert!(context_of(&join).contains("joined team `x` as a worker"));
    let plain = claude_hook(
        &daemon,
        &claude(
            "UserPromptSubmit",
            WORKER_SESSION,
            &json!({"prompt": "fix the tests"}),
        ),
        &state,
    )
    .await;
    assert_eq!(plain, None, "an ordinary prompt gets no output");

    check_mail_reminders(&daemon, &bridge, &lead, &worker).await;
    let solo = claude_hook(
        &daemon,
        &claude(
            "UserPromptSubmit",
            WORKER_SESSION,
            &json!({"prompt": "/solo"}),
        ),
        &state,
    )
    .await
    .unwrap();
    assert!(context_of(&solo).contains("It is solo"), "{solo}");
    let usage = claude_hook(
        &daemon,
        &claude(
            "UserPromptSubmit",
            WORKER_SESSION,
            &json!({"prompt": "/join"}),
        ),
        &state,
    )
    .await
    .unwrap();
    assert_eq!(usage["decision"], "block");
}

/// `PostToolUse` tells a session about its unread mail, and only then.
async fn check_mail_reminders(
    daemon: &crate::client::Client,
    bridge: &std::sync::Arc<crate::bridge::Bridge>,
    lead: &str,
    worker: &str,
) {
    let state = MailcheckState::always();
    let lead_caller = crate::bridge::Caller {
        auth: crate::auth::AuthInfo {
            client_id: "claude".to_owned(),
            agents: vec!["claude-*".to_owned()],
            directory: vec!["claude-*".to_owned()],
            admin: false,
            mode: crate::auth::AuthMode::Hmac,
        },
        session: Some(LEAD_SESSION.to_owned()),
    };
    bridge
        .send(&lead_caller, lead, worker, "run the tests")
        .unwrap();
    let mail = claude_hook(
        daemon,
        &claude("PostToolUse", WORKER_SESSION, &json!({})),
        &state,
    )
    .await
    .unwrap();
    assert!(context_of(&mail).contains("1 unread"), "{mail}");
    let notice = claude_hook(
        daemon,
        &claude("PostToolUse", LEAD_SESSION, &json!({})),
        &state,
    )
    .await
    .unwrap();
    assert!(
        context_of(&notice).contains("1 unread"),
        "the lead got the join notice"
    );
    bridge.fetch_unread(&lead_caller, lead).unwrap();
    let none = claude_hook(
        daemon,
        &claude("PostToolUse", LEAD_SESSION, &json!({})),
        &state,
    )
    .await;
    assert_eq!(none, None);
}

#[tokio::test]
async fn a_wrong_token_is_refused_and_blocks_the_command() {
    let (base, _) = serve().await;
    let forged = daemon_client(&base, ("claude", "f".repeat(64).as_str()));
    let state = MailcheckState::always();
    let start = claude_hook(
        &forged,
        &claude("SessionStart", LEAD_SESSION, &json!({})),
        &state,
    )
    .await
    .unwrap();
    assert!(context_of(&start).contains("not available"), "{start}");
    let lead = claude_hook(
        &forged,
        &claude(
            "UserPromptSubmit",
            LEAD_SESSION,
            &json!({"prompt": "/lead x"}),
        ),
        &state,
    )
    .await
    .unwrap();
    assert_eq!(lead["decision"], "block");
}

#[tokio::test]
async fn codex_hooks_forward_the_payload_for_its_own_session() {
    let (base, _) = serve().await;
    let daemon = daemon_client(&base, CODEX_CLIENT);
    let start = json!({"hook_event_name": "SessionStart", "session_id": CODEX_SESSION, "cwd": "/repo", "source": "startup"});
    let out = codex_hook(&daemon, &start).await.unwrap();
    assert!(
        context_of(&out).contains(&format!("`codex-{CODEX_SESSION}`")),
        "{out}"
    );
    let lead = json!({"hook_event_name": "UserPromptSubmit", "session_id": CODEX_SESSION, "prompt": "$lead x"});
    let out = codex_hook(&daemon, &lead).await.unwrap();
    assert!(
        context_of(&out).contains("is now the lead of team `x`"),
        "{out}"
    );
    let stop =
        json!({"hook_event_name": "Stop", "session_id": CODEX_SESSION, "stop_hook_active": false});
    assert_eq!(codex_hook(&daemon, &stop).await.unwrap()["continue"], true);
    let invalid = json!({"hook_event_name": "SessionStart", "session_id": "not-a-uuid"});
    assert_eq!(codex_hook(&daemon, &invalid).await, None);
}

#[tokio::test]
async fn hooks_fail_open_when_the_daemon_is_down() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let daemon = daemon_client(&format!("http://127.0.0.1:{port}"), CLAUDE_CLIENT);
    let start = claude_hook(
        &daemon,
        &claude("SessionStart", LEAD_SESSION, &json!({})),
        &MailcheckState::always(),
    )
    .await
    .unwrap();
    assert!(context_of(&start).contains("Your inband mailbox for this session is"));
    let codex = daemon_client(&format!("http://127.0.0.1:{port}"), CODEX_CLIENT);
    let stop =
        json!({"hook_event_name": "Stop", "session_id": CODEX_SESSION, "stop_hook_active": false});
    assert_eq!(codex_hook(&codex, &stop).await, None);
}

#[test]
fn mailchecks_are_spaced_per_mailbox() {
    let dir = std::env::temp_dir().join(format!("inband-mailcheck-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let env: crate::config::EnvMap =
        [("XDG_RUNTIME_DIR".to_owned(), dir.display().to_string())].into();
    let state = MailcheckState::from_env(&env);
    assert!(state.due("claude-a-0001"));
    assert!(!state.due("claude-a-0001"));
    assert!(state.due("claude-b-0002"));
    let mode = std::fs::metadata(dir.join("inband"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}
