//! @file config.rs
//! @brief Reads and checks `config.json`.
//!
//! @details The daemon refuses a configuration with one value that is not valid.
//! The error gives the path of that value, for example `wake.codex.command`.
//! The module also contains the loopback rules: by default, the daemon and its URLs stay on this machine.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use serde_json::{Map, Value};
use url::{Host, Url};

/// @brief The environment variables of the process.
///
/// @details The functions take this map, not the real environment, so that the tests can give their own values.
pub type EnvMap = HashMap<String, String>;

/// @brief The maximum length of a mailbox name or a client name.
const MAX_NAME_LEN: usize = 64;
/// @brief The maximum size of a wake prompt.
const MAX_PROMPT_BYTES: usize = 16_384;
/// @brief The maximum number of retries of a Codex wake.
const MAX_RETRY_DELAYS: usize = 16;

/// @brief The configuration of the daemon.
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

/// @brief The authentication settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    /// False only when the file sets `"required": false`.
    pub required: bool,
    /// The clients, by name.
    pub clients: BTreeMap<String, AuthClientConfig>,
}

/// @brief The settings of one client, for example `claude`.
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

/// @brief The settings that all wake targets have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeCommon {
    /// The text of the wake. `{mailbox}` becomes the mailbox name.
    pub prompt: String,
    /// The minimum time after a successful wake of the same mailbox.
    pub debounce_seconds: u32,
    /// The maximum number of wakes for one mailbox in one hour.
    pub max_wakes_per_hour: u32,
}

/// @brief A client that the daemon can wake.
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
    /// @brief Gives the settings that all wake targets have.
    #[must_use]
    pub fn common(&self) -> &WakeCommon {
        match self {
            Self::Opencode { common, .. } | Self::Codex { common, .. } => common,
        }
    }
}

/// @brief What is wrong with a configuration value.
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
    /// @brief Writes the problem as a short English text.
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

/// @brief The errors of the configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{path} {problem}")]
    Invalid { path: String, problem: Problem },
}

/// @brief Makes the error for one value that is not valid.
fn invalid(path: impl Into<String>, problem: Problem) -> ConfigError {
    ConfigError::Invalid {
        path: path.into(),
        problem,
    }
}

/// @brief Tells if an `INBAND_UNSAFE_*` variable is `1`, `true` or `yes`.
fn unsafe_enabled(env: &EnvMap, name: &str) -> bool {
    env.get(name)
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

/// @brief Tells if a name is a valid mailbox name or client name.
///
/// @return True for 1 to 64 chars of `[a-z0-9_-]`.
#[must_use]
pub fn is_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// @brief Tells if a pattern is valid.
///
/// @return True for `*`, for a name, and for a name that ends with `*`.
#[must_use]
pub fn is_agent_pattern(pattern: &str) -> bool {
    pattern == "*" || is_agent_name(pattern.strip_suffix('*').unwrap_or(pattern))
}

/// @brief Tells if an address is on the loopback interface only.
#[must_use]
pub fn is_loopback_bind_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// @brief Gives the address where the daemon listens.
///
/// @details The address comes from `INBAND_BIND`. The default is `127.0.0.1`.
/// An address that other machines can reach needs `INBAND_UNSAFE_REMOTE_BIND=1`.
///
/// @throws ConfigError The address is not loopback, and the unsafe flag is not set.
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

/// @brief Tells if the host of a URL is this machine.
fn is_loopback_url_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// @brief Checks an http base URL and removes its trailing `/`, query and fragment.
///
/// @details The URL must point to this machine, unless `INBAND_UNSAFE_REMOTE_URLS=1`.
///
/// @param raw The URL from the configuration or the environment.
/// @param env The environment variables.
/// @return The base URL.
/// @throws ConfigError The URL is not valid, does not use http, or points to another machine.
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

/// @brief Reads a JSON object.
///
/// @throws ConfigError The value is not an object.
fn object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, ConfigError> {
    value
        .as_object()
        .ok_or_else(|| invalid(path, Problem::NotObject))
}

/// @brief Reads a JSON string.
///
/// @throws ConfigError The value is missing, or is not a string.
fn string<'a>(value: Option<&'a Value>, path: &str) -> Result<&'a str, ConfigError> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(path, Problem::NotString))
}

/// @brief Reads an integer in a range.
///
/// @throws ConfigError The value is missing, or is not an integer from `min` to `max`.
fn integer(value: Option<&Value>, path: &str, min: u64, max: u64) -> Result<u64, ConfigError> {
    value
        .and_then(Value::as_u64)
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| invalid(path, Problem::IntegerOutOfRange { min, max }))
}

/// @brief Reads an integer from 1 to 3600.
///
/// @throws ConfigError The value is missing, or is out of range.
fn small_integer(value: Option<&Value>, path: &str) -> Result<u32, ConfigError> {
    let n = integer(value, path, 1, 3600)?;
    u32::try_from(n).map_err(|_| invalid(path, Problem::IntegerOutOfRange { min: 1, max: 3600 }))
}

/// @brief Reads a wake prompt.
///
/// @throws ConfigError The prompt is empty, or is larger than 16 KiB.
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

/// @brief Reads the path or the name of an executable.
///
/// @details The daemon runs the executable without a shell.
/// The value can thus contain only letters, digits, `_`, `.`, `/` and `-`.
/// A shell command, such as `codex; rm -rf ~`, is refused.
///
/// @throws ConfigError The value is empty, or contains other chars.
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

/// @brief Reads one entry of `wake`.
///
/// @param name The name of the entry, for the error paths.
/// @param value The JSON value of the entry.
/// @param env The environment variables, for the URL rules.
/// @return The wake target.
/// @throws ConfigError A value of the entry is missing or not valid.
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

/// @brief Reads a list of mailbox patterns.
///
/// @details The patterns are changed to lower case.
///
/// @throws ConfigError The list is empty, or one pattern is not valid.
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

/// @brief Reads the `auth` object.
///
/// @details Each client needs a token, either in `token` or through `tokenEnv`.
/// Authentication stays on unless `required` is exactly `false`.
///
/// @return The settings, or `None` when there is no `auth` object.
/// @throws ConfigError A client has a name, a pattern or a token that is not valid.
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

/// @brief Reads and checks the text of `config.json`.
///
/// @param raw The text of the file.
/// @param env The environment variables.
/// @return The configuration.
/// @throws ConfigError The text is not JSON, or one value is not valid. The error gives the first such value.
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

    /// @brief Makes a set of variables for a test.
    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// @brief Gives the error text of a configuration that must be refused.
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

    /// @brief A valid configuration for the tests.
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
