use serde_json::json;

use super::*;
use crate::test_support::*;

#[test]
fn sse_messages_survive_split_chunks() {
    let mut lines = SseLines::default();
    assert!(lines.push(b"event: message\ndata: {\"id\":").is_empty());
    let messages = lines.push(b"1,\"result\":{}}\n\ndata: not json\ndata: {\"method\":\"x\"}\n");
    assert_eq!(
        messages,
        vec![json!({"id": 1, "result": {}}), json!({"method": "x"})]
    );
}

#[test]
fn progress_is_reported_and_errors_become_refusals() {
    let mut beats = Vec::new();
    let progress = json!({"method": "notifications/progress", "params": {"progress": 2}});
    assert!(
        handle_rpc_message(&progress, &mut |params| beats.push(params.clone()))
            .unwrap()
            .is_none()
    );
    assert_eq!(beats, vec![json!({"progress": 2})]);
    let error = json!({"id": 1, "error": {"code": -32602, "message": "bad"}});
    assert!(matches!(
        handle_rpc_message(&error, &mut |_| {}),
        Err(ClientError::Refused { .. })
    ));
}

#[test]
fn reads_the_error_text_of_daemon_answers() {
    assert_eq!(error_message(r#"{"error":"nope"}"#), "nope");
    assert_eq!(
        error_message(r#"{"error":{"code":-32001,"message":"auth"}}"#),
        "auth"
    );
    assert_eq!(error_message("plain"), "plain");
}

#[tokio::test]
async fn refusals_carry_the_status_and_the_reason() {
    let (base, _) = serve().await;
    let anonymous = Client::new(&base, "claude", None).unwrap();
    let failure = anonymous
        .request(Method::GET, "/health", None, None, Duration::from_secs(2))
        .await
        .unwrap_err();
    assert!(
        matches!(failure, ClientError::Refused { status, .. } if status == StatusCode::UNAUTHORIZED),
        "{failure}"
    );
    let admin = daemon_client(&base, ("admin", ADMIN));
    let health = admin
        .request(Method::GET, "/health", None, None, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
}

#[test]
fn only_loopback_daemons_unless_unsafe() {
    let env: EnvMap = [
        (
            "INBAND_URL".to_owned(),
            "http://example.com:7447".to_owned(),
        ),
        ("HOME".to_owned(), "/nonexistent".to_owned()),
    ]
    .into();
    assert!(matches!(
        Client::from_env("claude", env),
        Err(ClientError::Url(_))
    ));
}
