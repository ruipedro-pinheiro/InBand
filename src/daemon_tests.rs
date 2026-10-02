use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Instant;

use serde_json::Value;

use super::*;

const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CLAUDE: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// An install directory with this configuration and these tokens.
fn install_dir(name: &str, config: &str, tokens: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("inband-daemon-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.json"), config).unwrap();
    let token_file = dir.join("tokens.env");
    fs::write(&token_file, tokens).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    dir
}

fn config_with_auth(port: u16) -> String {
    format!(
        r#"{{"port": {port}, "maxMessageBytes": 65536, "wake": {{}},
           "auth": {{"required": true, "clients": {{
             "admin": {{"tokenEnv": "INBAND_ADMIN_TOKEN", "agents": ["*"], "admin": true}},
             "claude": {{"tokenEnv": "INBAND_CLAUDE_TOKEN", "agents": ["claude-*"]}}}}}}}}"#
    )
}

/// An environment without a real home directory.
fn env() -> EnvMap {
    [("HOME".to_owned(), "/nonexistent".to_owned())].into()
}

fn options(directory: &Path, grace: Duration) -> DaemonOptions {
    DaemonOptions {
        directory: Some(directory.to_owned()),
        shutdown_grace: grace,
    }
}

/// The Claude token has the pre-rename name. An open long poll must not hold the stop past the
/// grace time.
#[tokio::test]
async fn serves_the_install_directory_and_stops_within_the_grace() {
    let port = free_port();
    let dir = install_dir(
        "serve",
        &config_with_auth(port),
        &format!("INBAND_ADMIN_TOKEN={ADMIN}\nAGENT_BRIDGE_CLAUDE_TOKEN={CLAUDE}\n"),
    );
    let grace = Duration::from_millis(300);
    let listening = bind(&options(&dir, grace), env()).await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(
        listening,
        async move {
            let _ = stopped.await;
        },
        grace,
    ));

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let health: Value = client
        .get(format!("{base}/health"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert!(health.get("agents").is_some(), "admin sees the full status");
    let claude = client
        .get(format!("{base}/health"))
        .bearer_auth(CLAUDE)
        .send()
        .await
        .unwrap();
    assert_eq!(claude.status(), 200, "the legacy token name still loads");
    let anonymous = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(anonymous.status(), 401);

    let poll = tokio::spawn(
        client
            .get(format!("{base}/subscribe?prefix=claude&timeout=60"))
            .bearer_auth(ADMIN)
            .send(),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    poll.abort();

    let mode = fs::metadata(dir.join("bridge.db"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn refuses_to_start_without_a_safe_setup() {
    let grace = Duration::from_millis(100);
    let empty = std::env::temp_dir().join(format!("inband-daemon-empty-{}", std::process::id()));
    fs::create_dir_all(&empty).unwrap();
    assert!(matches!(
        bind(&options(&empty, grace), env()).await,
        Err(DaemonError::ReadConfig { .. })
    ));

    let weak = install_dir(
        "weak",
        &config_with_auth(free_port()),
        "INBAND_ADMIN_TOKEN=short\n",
    );
    assert!(matches!(
        bind(&options(&weak, grace), env()).await,
        Err(DaemonError::Auth(_))
    ));

    let open_config = format!(
        r#"{{"port": {}, "maxMessageBytes": 65536, "wake": {{}}}}"#,
        free_port()
    );
    let open = install_dir("open", &open_config, "");
    let mut remote = env();
    remote.insert("INBAND_BIND".to_owned(), "0.0.0.0".to_owned());
    remote.insert("INBAND_UNSAFE_REMOTE_BIND".to_owned(), "1".to_owned());
    assert!(matches!(
        bind(&options(&open, grace), remote).await,
        Err(DaemonError::OpenRemoteBind)
    ));
    let mut unflagged = env();
    unflagged.insert("INBAND_BIND".to_owned(), "0.0.0.0".to_owned());
    assert!(matches!(
        bind(&options(&open, grace), unflagged).await,
        Err(DaemonError::Bind(_))
    ));
}
