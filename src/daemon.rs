//! `inband daemon`: loads the install directory, then serves the hook routes and the MCP endpoint.

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

/// Waits and long polls can last minutes. On shutdown they get this long, then the daemon exits.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const INSTALL_DIR: &str = ".local/share/mcp-servers/inband";

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

/// Where the daemon finds `config.json`, `tokens.env` and `bridge.db`.
pub struct DaemonOptions {
    /// The install directory. Default: `$INBAND_HOME`, else `~/.local/share/mcp-servers/inband`.
    pub directory: Option<PathBuf>,
    pub shutdown_grace: Duration,
}

/// The default install directory for an environment.
#[must_use]
pub fn default_directory(env: &EnvMap) -> Option<PathBuf> {
    let non_empty = |name: &str| env.get(name).filter(|value| !value.is_empty());
    non_empty("INBAND_HOME")
        .map(PathBuf::from)
        .or_else(|| non_empty("HOME").map(|home| Path::new(home).join(INSTALL_DIR)))
}

/// Runs the daemon with the process environment until SIGINT or SIGTERM.
///
/// # Errors
/// Returns the first startup error, or a server failure.
pub async fn run(directory: Option<PathBuf>) -> Result<(), DaemonError> {
    let env: EnvMap = std::env::vars().collect();
    let options = DaemonOptions {
        directory,
        shutdown_grace: SHUTDOWN_GRACE,
    };
    let listener = bind(&options, env).await?;
    serve(listener, shutdown_signal(), options.shutdown_grace).await
}

/// A loaded daemon with its listening socket, ready to serve.
pub struct Listening {
    pub address: String,
    listener: TcpListener,
    app: Router,
    bridge: Arc<Bridge>,
}

/// Loads the install directory and opens the listening socket.
///
/// # Errors
/// Returns an error for a missing or invalid config, unusable tokens, an unsafe bind, the database
/// or the socket.
pub async fn bind(options: &DaemonOptions, mut env: EnvMap) -> Result<Listening, DaemonError> {
    let explicit = options.directory.is_some();
    let directory = options
        .directory
        .clone()
        .or_else(|| default_directory(&env))
        .ok_or(DaemonError::NoDirectory)?;
    if explicit {
        // An explicit directory holds its own token file, unless INBAND_TOKENS_FILE says otherwise.
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

/// Serves until `shutdown` completes, then gives open requests `grace` to finish.
///
/// # Errors
/// Returns a server failure.
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

fn finished(
    result: Result<std::io::Result<()>, tokio::task::JoinError>,
) -> Result<(), DaemonError> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(DaemonError::Serve(error)),
        Err(join) => Err(DaemonError::Serve(std::io::Error::other(join.to_string()))),
    }
}

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
