//! @file daemon.rs
//! @brief The `inband daemon` command.
//!
//! @details The daemon reads the install directory: `config.json`, `tokens.env` and `bridge.db`.
//! It then serves the routes of the hooks and the MCP endpoint on the loopback interface.

use std::future::{Future, IntoFuture};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;

use crate::auth::{AuthError, AuthRuntime};
use crate::bridge::Bridge;
use crate::config::{
    ConfigError, EnvMap, is_loopback_bind_host, load_bridge_config, resolve_bind_host,
};
use crate::db::{self, DbError};
use crate::dispatch::RealWake;
use crate::http::{AppState, MAX_BODY_BYTES, router};
use crate::tokens::{LoadOutcome, TokensError, load_token_env_file};

/// @brief The time that the open requests get to finish when the daemon stops.
///
/// @details A wait or a long poll can take minutes. After this time, the daemon stops it.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// @brief The default install directory, relative to the home directory.
const INSTALL_DIR: &str = ".local/share/mcp-servers/inband";

/// @brief The reasons why the daemon cannot start or continue.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("no install directory: set HOME or INBAND_HOME, or pass --dir")]
    NoDirectory,
    #[error("cannot read {path}: {source}")]
    ReadConfig {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not valid: {source}")]
    Config { path: PathBuf, source: ConfigError },
    #[error("invalid bind address: {0}")]
    Bind(ConfigError),
    #[error(transparent)]
    Tokens(#[from] TokensError),
    #[error("auth: {0}")]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("cannot build the wake HTTP client: {0}")]
    Http(#[from] reqwest::Error),
    #[error("refusing a non-loopback bind while auth is disabled")]
    OpenRemoteBind,
    #[error("cannot listen on {address}: {source}")]
    Listen {
        address: String,
        source: std::io::Error,
    },
    #[error("server stopped: {0}")]
    Serve(std::io::Error),
}

/// @brief The options of the daemon.
pub struct DaemonOptions {
    /// The install directory. Default: `$INBAND_HOME`, else `~/.local/share/mcp-servers/inband`.
    pub directory: Option<PathBuf>,
    pub shutdown_grace: Duration,
}

/// @brief Gives the default install directory.
///
/// @return `$INBAND_HOME`, else `~/.local/share/mcp-servers/inband`, else `None` without a home directory.
#[must_use]
pub fn default_directory(env: &EnvMap) -> Option<PathBuf> {
    let non_empty = |name: &str| env.get(name).filter(|value| !value.is_empty());
    non_empty("INBAND_HOME")
        .map(PathBuf::from)
        .or_else(|| non_empty("HOME").map(|home| Path::new(home).join(INSTALL_DIR)))
}

/// @brief Runs the daemon until it receives SIGINT or SIGTERM.
///
/// @param directory The install directory. `None` uses the default.
/// @throws DaemonError The first error at start, or a failure of the server.
pub async fn run(directory: Option<PathBuf>) -> Result<(), DaemonError> {
    let env: EnvMap = std::env::vars().collect();
    let options = DaemonOptions {
        directory,
        shutdown_grace: SHUTDOWN_GRACE,
    };
    let listener = bind(&options, env).await?;
    serve(listener, shutdown_signal(), options.shutdown_grace).await
}

/// @brief A daemon that has read its files and opened its socket.
pub struct Listening {
    pub address: String,
    listener: TcpListener,
    app: Router,
    bridge: Arc<Bridge>,
}

/// @brief Reads the install directory and opens the socket.
///
/// @details With an explicit directory, the daemon reads the `tokens.env` of that directory, unless `INBAND_TOKENS_FILE` is set.
/// Without authentication, the daemon refuses an address that other machines can reach.
///
/// @param options The directory and the stop time.
/// @param env The environment variables.
/// @return The daemon, ready to serve.
/// @throws DaemonError The configuration is missing or not valid, a token cannot be used, the address is not safe, or the database or the socket fails.
pub async fn bind(options: &DaemonOptions, mut env: EnvMap) -> Result<Listening, DaemonError> {
    let explicit = options.directory.is_some();
    let directory = options
        .directory
        .clone()
        .or_else(|| default_directory(&env))
        .ok_or(DaemonError::NoDirectory)?;
    if explicit {
        env.entry("INBAND_TOKENS_FILE".to_owned())
            .or_insert_with(|| directory.join("tokens.env").display().to_string());
    }
    match load_token_env_file(&mut env)? {
        LoadOutcome::Loaded {
            path,
            private: false,
        } => eprintln!(
            "inband: warning: {} can be read by other users. Run: chmod 600 {}",
            path.display(),
            path.display()
        ),
        LoadOutcome::Missing(path) => {
            eprintln!("inband: no token file at {}", path.display());
        }
        _ => {}
    }

    let config_path = directory.join("config.json");
    let raw = std::fs::read_to_string(&config_path).map_err(|source| DaemonError::ReadConfig {
        path: config_path.clone(),
        source,
    })?;
    let config = load_bridge_config(&raw, &env).map_err(|source| DaemonError::Config {
        path: config_path,
        source,
    })?;
    let auth = AuthRuntime::new(config.auth.as_ref(), &env)?;
    let host = resolve_bind_host(&env).map_err(DaemonError::Bind)?;
    if !auth.required() {
        if !is_loopback_bind_host(&host) {
            return Err(DaemonError::OpenRemoteBind);
        }
        eprintln!("inband: warning: auth is disabled, so every local process can use the daemon");
    }

    let connection = db::open(&directory.join("bridge.db"))?;
    let secrets = auth.tokens().map(str::to_owned).collect();
    let port = config.port;
    let bridge = Bridge::new(connection, config, secrets, Arc::new(RealWake::new()?));
    let state = AppState {
        bridge: Arc::clone(&bridge),
        auth: Arc::new(auth),
    };
    let mcp = Router::new().route_service(
        "/mcp",
        crate::mcp::service(Arc::clone(&bridge), MAX_BODY_BYTES),
    );
    let address = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let listener = TcpListener::bind(&address)
        .await
        .map_err(|source| DaemonError::Listen {
            address: address.clone(),
            source,
        })?;
    Ok(Listening {
        address,
        listener,
        app: router(state, mcp),
        bridge,
    })
}

/// @brief Serves the requests until `shutdown` completes.
///
/// @details The daemon then gives `grace` to the open requests, and stops the others.
///
/// @param listening The daemon from [`bind`].
/// @param shutdown Completes when the daemon must stop.
/// @param grace The time for the open requests.
/// @throws DaemonError The server fails.
pub async fn serve(
    listening: Listening,
    shutdown: impl Future<Output = ()>,
    grace: Duration,
) -> Result<(), DaemonError> {
    let Listening {
        address,
        listener,
        app,
        bridge,
    } = listening;
    eprintln!("inband listening on http://{address}/mcp");
    if let Err(error) = bridge.reconcile_codex_wakes() {
        eprintln!("inband: cannot wake the Codex sessions with unread mail: {error}");
    }
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .into_future(),
    );
    tokio::select! {
        result = &mut server => return finished(result),
        () = shutdown => {}
    }
    eprintln!("inband: shutting down");
    let _ = stop.send(());
    if let Ok(result) = tokio::time::timeout(grace, &mut server).await {
        finished(result)
    } else {
        eprintln!("inband: closing the waits still open");
        server.abort();
        Ok(())
    }
}

/// @brief Changes the end of the server task into a result.
fn finished(
    result: Result<std::io::Result<()>, tokio::task::JoinError>,
) -> Result<(), DaemonError> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(DaemonError::Serve(error)),
        Err(join) => Err(DaemonError::Serve(std::io::Error::other(join.to_string()))),
    }
}

/// @brief Completes when the process receives SIGINT or SIGTERM.
async fn shutdown_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
