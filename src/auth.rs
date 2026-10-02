//! Client tokens, bearer and HMAC request authentication, and mailbox authorization.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::{AuthConfig, EnvMap};

pub const CLIENT_HEADER: &str = "x-inband-client";
pub const TIMESTAMP_HEADER: &str = "x-inband-timestamp";
pub const NONCE_HEADER: &str = "x-inband-nonce";
pub const SIGNATURE_HEADER: &str = "x-inband-signature";
/// The session a signed request acts for. v1 clients never send it, so their signatures stay the
/// same: the signed payload includes it only when it is present.
pub const SESSION_HEADER: &str = "x-inband-session";
const LEGACY_HEADER_PREFIX: &str = "x-agent-bridge-";

const TOKEN_MIN_LENGTH: usize = 32;
const NONCE_WINDOW_MS: u64 = 300_000;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("auth client \"{0}\" has no token")]
    TokenMissing(String),
    #[error("auth client \"{0}\" token is too short; expected at least {TOKEN_MIN_LENGTH} chars")]
    TokenTooShort(String),
    #[error("auth client \"{0}\" token is a placeholder")]
    TokenPlaceholder(String),
    #[error("duplicate auth token configured for client \"{0}\"")]
    DuplicateToken(String),
    #[error("auth.required is true but no auth clients are configured")]
    NoClients,
    #[error("missing Authorization header")]
    MissingAuthorization,
    #[error("missing bearer token")]
    MissingBearer,
    #[error("invalid bearer token")]
    InvalidBearer,
    #[error("missing inband signed auth headers")]
    MissingSignedHeaders,
    #[error("unknown inband auth client")]
    UnknownClient,
    #[error("stale inband signed request")]
    Stale,
    #[error("invalid inband auth nonce")]
    InvalidNonce,
    #[error("inband signed request replay detected")]
    Replay,
    #[error("invalid inband request signature")]
    InvalidSignature,
    #[error("invalid inband session id")]
    InvalidSession,
    #[error("auth client \"{client}\" is not authorized to use {field}=\"{agent}\"")]
    NotAuthorized {
        client: String,
        field: &'static str,
        agent: String,
    },
    #[error("auth client \"{0}\" is not authorized for admin operations")]
    NotAdmin(String),
}

/// How a request proved its identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Bearer,
    Hmac,
    Disabled,
}

/// The authenticated client of one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthInfo {
    pub client_id: String,
    pub agents: Vec<String>,
    pub directory: Vec<String>,
    pub admin: bool,
    pub mode: AuthMode,
}

impl AuthInfo {
    /// Full access, used when `auth.required` is false.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            client_id: "auth-disabled".to_owned(),
            agents: vec!["*".to_owned()],
            directory: vec!["*".to_owned()],
            admin: true,
            mode: AuthMode::Disabled,
        }
    }
}

#[derive(Debug)]
struct Client {
    id: String,
    token: String,
    agents: Vec<String>,
    directory: Vec<String>,
    admin: bool,
}

impl Client {
    fn info(&self, mode: AuthMode) -> AuthInfo {
        let all = || vec!["*".to_owned()];
        AuthInfo {
            client_id: self.id.clone(),
            agents: if self.admin {
                all()
            } else {
                self.agents.clone()
            },
            directory: if self.admin {
                all()
            } else {
                self.directory.clone()
            },
            admin: self.admin,
            mode,
        }
    }
}

/// The configured clients and the nonces seen inside the replay window.
#[derive(Debug)]
pub struct AuthRuntime {
    required: bool,
    clients: Vec<Client>,
    seen_nonces: Mutex<HashMap<String, u64>>,
}

fn is_placeholder(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    let change_me = ["changeme", "change-me", "change_me", "change me"]
        .iter()
        .any(|word| lower.contains(word));
    change_me
        || ["placeholder", "example", "secret", "password"]
            .iter()
            .any(|word| lower.contains(word))
        || lower == "token"
}

fn resolve_token(
    client_id: &str,
    token: Option<&str>,
    token_env: Option<&str>,
    env: &EnvMap,
) -> Result<String, AuthError> {
    let token = match token_env {
        Some(name) if !name.is_empty() => env.get(name).map(String::as_str),
        _ => token,
    };
    let token = token
        .filter(|token| !token.is_empty())
        .ok_or_else(|| AuthError::TokenMissing(client_id.to_owned()))?;
    if token.len() < TOKEN_MIN_LENGTH {
        return Err(AuthError::TokenTooShort(client_id.to_owned()));
    }
    if is_placeholder(token) {
        return Err(AuthError::TokenPlaceholder(client_id.to_owned()));
    }
    Ok(token.to_owned())
}

impl AuthRuntime {
    /// Resolves client tokens from the config and the environment.
    ///
    /// # Errors
    /// Returns an error for a missing, short, placeholder or duplicate token.
    pub fn new(config: Option<&AuthConfig>, env: &EnvMap) -> Result<Self, AuthError> {
        let Some(config) = config else {
            return Ok(Self {
                required: false,
                clients: Vec::new(),
                seen_nonces: Mutex::default(),
            });
        };
        let mut clients = Vec::new();
        let mut tokens = HashSet::new();
        for (id, client) in &config.clients {
            let token = resolve_token(
                id,
                client.token.as_deref(),
                client.token_env.as_deref(),
                env,
            )?;
            if !tokens.insert(token.clone()) {
                return Err(AuthError::DuplicateToken(id.clone()));
            }
            clients.push(Client {
                id: id.clone(),
                token,
                agents: client.agents.clone(),
                directory: client
                    .directory
                    .clone()
                    .unwrap_or_else(|| client.agents.clone()),
                admin: client.admin,
            });
        }
        if config.required && clients.is_empty() {
            return Err(AuthError::NoClients);
        }
        Ok(Self {
            required: config.required,
            clients,
            seen_nonces: Mutex::default(),
        })
    }

    #[must_use]
    pub fn required(&self) -> bool {
        self.required
    }

    /// The token of a configured client, for the daemon's outbound checks.
    pub fn tokens(&self) -> impl Iterator<Item = &str> {
        self.clients.iter().map(|client| client.token.as_str())
    }

    /// Authenticates an `Authorization: Bearer` header.
    ///
    /// # Errors
    /// Returns an error for a missing, malformed or unknown token.
    pub fn authenticate_bearer(&self, authorization: Option<&str>) -> Result<AuthInfo, AuthError> {
        let Some(authorization) = authorization else {
            return if self.required {
                Err(AuthError::MissingAuthorization)
            } else {
                Ok(AuthInfo::disabled())
            };
        };
        let token = authorization
            .split_once(char::is_whitespace)
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token.trim())
            .filter(|token| !token.is_empty())
            .ok_or(AuthError::MissingBearer)?;
        // Compare every client in constant time so the timing does not reveal which one matched.
        let mut found = None;
        for client in &self.clients {
            if constant_equal(token, &client.token) {
                found = Some(client);
            }
        }
        found
            .map(|client| client.info(AuthMode::Bearer))
            .ok_or(AuthError::InvalidBearer)
    }

    /// Authenticates a request signed by a hook or the channel shim.
    ///
    /// # Errors
    /// See [`Self::verify_signed`].
    pub fn authenticate_signed(
        &self,
        request: &SignedRequest<'_>,
        now_ms: u64,
    ) -> Result<AuthInfo, AuthError> {
        self.verify_signed(request, now_ms).map(|(info, _)| info)
    }

    /// Authenticates a signed request and returns its client and the session it was signed for.
    ///
    /// # Errors
    /// Returns an error for missing headers, an unknown client, a stale timestamp, a bad nonce,
    /// an invalid session id, a replayed nonce or a wrong signature.
    pub fn verify_signed(
        &self,
        request: &SignedRequest<'_>,
        now_ms: u64,
    ) -> Result<(AuthInfo, Option<String>), AuthError> {
        let session = request.header(SESSION_HEADER);
        if session.is_some_and(|session| !is_valid_session(session)) {
            return Err(AuthError::InvalidSession);
        }
        let session = session.map(str::to_owned);
        if !self.required {
            return Ok((AuthInfo::disabled(), session));
        }
        let header = |name: &str| request.header(name);
        let (Some(client_id), Some(timestamp), Some(nonce), Some(signature)) = (
            header(CLIENT_HEADER),
            header(TIMESTAMP_HEADER),
            header(NONCE_HEADER),
            header(SIGNATURE_HEADER),
        ) else {
            return Err(AuthError::MissingSignedHeaders);
        };
        let client = self
            .clients
            .iter()
            .find(|client| client.id == client_id)
            .ok_or(AuthError::UnknownClient)?;
        let timestamp_ms: u64 = timestamp.parse().map_err(|_| AuthError::Stale)?;
        if now_ms.abs_diff(timestamp_ms) > NONCE_WINDOW_MS {
            return Err(AuthError::Stale);
        }
        if !is_valid_nonce(nonce) {
            return Err(AuthError::InvalidNonce);
        }
        let expected = signature_value(
            &client.token,
            &SigningInput {
                client_id,
                method: request.method,
                url: request.url,
                body: request.body,
                timestamp,
                nonce,
                session: session.as_deref(),
            },
        );
        if !constant_equal(signature, &expected) {
            return Err(AuthError::InvalidSignature);
        }
        let mut seen = self
            .seen_nonces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cutoff = now_ms.saturating_sub(NONCE_WINDOW_MS);
        seen.retain(|_, seen_at| *seen_at >= cutoff);
        let key = format!("{client_id}:{nonce}");
        if seen.contains_key(&key) {
            return Err(AuthError::Replay);
        }
        seen.insert(key, now_ms);
        Ok((client.info(AuthMode::Hmac), session))
    }

    /// Bearer when an `Authorization` header is present, signed headers otherwise.
    ///
    /// # Errors
    /// See [`Self::authenticate_bearer`] and [`Self::authenticate_signed`].
    pub fn authenticate(
        &self,
        request: &SignedRequest<'_>,
        now_ms: u64,
    ) -> Result<AuthInfo, AuthError> {
        self.authenticate_request(request, now_ms)
            .map(|(info, _)| info)
    }

    /// Like [`Self::authenticate`], and also returns the session of a signed request. A bearer
    /// request has no session: its token is shared by every session of its family.
    ///
    /// # Errors
    /// See [`Self::authenticate_bearer`] and [`Self::verify_signed`].
    pub fn authenticate_request(
        &self,
        request: &SignedRequest<'_>,
        now_ms: u64,
    ) -> Result<(AuthInfo, Option<String>), AuthError> {
        match request.header("authorization") {
            Some(authorization) => Ok((self.authenticate_bearer(Some(authorization))?, None)),
            None => self.verify_signed(request, now_ms),
        }
    }
}

/// A request as seen by the signature check.
pub struct SignedRequest<'a> {
    pub method: &'a str,
    /// Path and query, or a full URL.
    pub url: &'a str,
    pub body: Option<&'a Value>,
    /// Header lookup by lowercase name.
    pub headers: &'a dyn Fn(&str) -> Option<&'a str>,
}

impl<'a> SignedRequest<'a> {
    /// A header, with the pre-rename `x-agent-bridge-*` name as a fallback for old hooks.
    fn header(&self, name: &str) -> Option<&'a str> {
        (self.headers)(name).or_else(|| {
            let rest = name.strip_prefix("x-inband-")?;
            (self.headers)(&format!("{LEGACY_HEADER_PREFIX}{rest}"))
        })
    }
}

/// Session ids of Claude Code and Codex (UUIDs) and OpenCode (`ses_…`): no char can end a line of
/// the signed payload.
#[must_use]
pub fn is_valid_session(session: &str) -> bool {
    (1..=128).contains(&session.len())
        && session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn is_valid_nonce(nonce: &str) -> bool {
    (6..=128).contains(&nonce.len())
        && nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn constant_equal(left: &str, right: &str) -> bool {
    // Hash first so the comparison time does not depend on where the inputs differ or on their lengths.
    let left_hash = Sha256::digest(left.as_bytes());
    let right_hash = Sha256::digest(right.as_bytes());
    bool::from(left_hash.as_slice().ct_eq(right_hash.as_slice())) && left.len() == right.len()
}

/// JSON with object keys sorted at every level, as the v1 clients produce it.
#[must_use]
pub fn stable_stringify(value: &Value) -> String {
    let mut out = String::new();
    write_stable(value, &mut out);
    out
}

fn write_stable(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_stable(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_stable(&map[key], out);
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

fn path_and_query(url: &str) -> String {
    let base = url::Url::parse("http://127.0.0.1").ok();
    let parsed = url::Url::parse(url)
        .ok()
        .or_else(|| base.and_then(|base| base.join(url).ok()));
    match parsed {
        Some(parsed) => match parsed.query().filter(|query| !query.is_empty()) {
            Some(query) => format!("{}?{query}", parsed.path()),
            None => parsed.path().to_owned(),
        },
        None => url.to_owned(),
    }
}

struct SigningInput<'a> {
    client_id: &'a str,
    method: &'a str,
    url: &'a str,
    body: Option<&'a Value>,
    timestamp: &'a str,
    nonce: &'a str,
    session: Option<&'a str>,
}

fn signature_value(token: &str, input: &SigningInput<'_>) -> String {
    let body = input.body.map(stable_stringify).unwrap_or_default();
    let digest = hex::encode(Sha256::digest(body.as_bytes()));
    let mut payload = [
        input.method.to_ascii_uppercase().as_str(),
        path_and_query(input.url).as_str(),
        digest.as_str(),
        input.timestamp,
        input.nonce,
        input.client_id,
    ]
    .join("\n");
    if let Some(session) = input.session {
        payload.push('\n');
        payload.push_str(session);
    }
    // HMAC accepts keys of any length, so this cannot fail.
    let mut mac = HmacSha256::new_from_slice(token.as_bytes()).unwrap_or_else(|_| unreachable!());
    mac.update(payload.as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// The signature headers for a request, as `(name, value)` pairs: four, plus the session header
/// when the request acts for one session.
#[must_use]
pub fn sign_request(
    client_id: &str,
    token: &str,
    request: (&str, &str, Option<&Value>),
    now_ms: u64,
    nonce: Option<&str>,
    session: Option<&str>,
) -> Vec<(&'static str, String)> {
    let (method, url, body) = request;
    let timestamp = now_ms.to_string();
    let nonce = nonce.map_or_else(|| hex::encode(rand::random::<[u8; 16]>()), str::to_owned);
    let signature = signature_value(
        token,
        &SigningInput {
            client_id,
            method,
            url,
            body,
            timestamp: &timestamp,
            nonce: &nonce,
            session,
        },
    );
    let mut headers = vec![
        (CLIENT_HEADER, client_id.to_owned()),
        (TIMESTAMP_HEADER, timestamp),
        (NONCE_HEADER, nonce),
        (SIGNATURE_HEADER, signature),
    ];
    if let Some(session) = session {
        headers.push((SESSION_HEADER, session.to_owned()));
    }
    headers
}

#[must_use]
pub fn agent_matches_pattern(agent: &str, pattern: &str) -> bool {
    let agent = agent.trim().to_ascii_lowercase();
    let pattern = pattern.trim().to_ascii_lowercase();
    if pattern == "*" {
        return true;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => agent.starts_with(prefix),
        None => agent == pattern,
    }
}

/// # Errors
/// Returns [`AuthError::NotAuthorized`] when no pattern of the client matches `agent`.
pub fn assert_agent_authorized(
    auth: &AuthInfo,
    agent: &str,
    field: &'static str,
) -> Result<(), AuthError> {
    if auth.admin
        || auth
            .agents
            .iter()
            .any(|pattern| agent_matches_pattern(agent, pattern))
    {
        return Ok(());
    }
    Err(AuthError::NotAuthorized {
        client: auth.client_id.clone(),
        field,
        agent: agent.to_owned(),
    })
}

/// # Errors
/// Returns [`AuthError::NotAuthorized`] when the client cannot use the whole `prefix-*` family.
pub fn assert_family_authorized(
    auth: &AuthInfo,
    prefix: &str,
    field: &'static str,
) -> Result<(), AuthError> {
    let family = format!("{}-*", prefix.trim().to_ascii_lowercase());
    if auth.admin
        || auth
            .agents
            .iter()
            .any(|pattern| pattern == "*" || *pattern == family)
    {
        return Ok(());
    }
    Err(AuthError::NotAuthorized {
        client: auth.client_id.clone(),
        field,
        agent: prefix.to_owned(),
    })
}

/// # Errors
/// Returns [`AuthError::NotAdmin`] for a non-admin client.
pub fn assert_admin(auth: &AuthInfo) -> Result<(), AuthError> {
    if auth.admin {
        Ok(())
    } else {
        Err(AuthError::NotAdmin(auth.client_id.clone()))
    }
}

/// Mailbox patterns whose history the client can read. `None` means all.
#[must_use]
pub fn visible_patterns(auth: &AuthInfo) -> Option<&[String]> {
    (!auth.admin).then_some(auth.agents.as_slice())
}

/// Mailbox patterns that `ping` lists for the client. `None` means all.
#[must_use]
pub fn directory_patterns(auth: &AuthInfo) -> Option<&[String]> {
    (!auth.admin && !auth.directory.iter().any(|pattern| pattern == "*"))
        .then_some(auth.directory.as_slice())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::config::AuthClientConfig;

    const CLAUDE: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const CODEX: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NOW: u64 = 1_790_000_000_000;

    fn client(token: &str, agents: &[&str], admin: bool) -> AuthClientConfig {
        AuthClientConfig {
            token: Some(token.to_owned()),
            token_env: None,
            agents: agents.iter().map(|a| (*a).to_owned()).collect(),
            directory: None,
            admin,
        }
    }

    fn runtime() -> AuthRuntime {
        let mut clients = BTreeMap::new();
        clients.insert("claude".to_owned(), client(CLAUDE, &["claude-*"], false));
        clients.insert("codex".to_owned(), client(CODEX, &["codex-*"], false));
        clients.insert("admin".to_owned(), client(ADMIN, &["*"], true));
        AuthRuntime::new(
            Some(&AuthConfig {
                required: true,
                clients,
            }),
            &EnvMap::new(),
        )
        .unwrap()
    }

    fn signed<'a>(
        method: &'a str,
        url: &'a str,
        body: Option<&'a Value>,
        headers: &'a HashMap<String, String>,
        lookup: &'a dyn Fn(&str) -> Option<&'a str>,
    ) -> SignedRequest<'a> {
        let _ = headers;
        SignedRequest {
            method,
            url,
            body,
            headers: lookup,
        }
    }

    #[test]
    fn bearer_tokens_map_to_their_client_and_scopes() {
        let auth = runtime();
        let claude = auth
            .authenticate_bearer(Some(&format!("Bearer {CLAUDE}")))
            .unwrap();
        assert_eq!(claude.client_id, "claude");
        assert!(assert_agent_authorized(&claude, "claude-api-a1b2", "from").is_ok());
        assert!(matches!(
            assert_agent_authorized(
                &claude,
                "codex-019f6767-789c-73b2-bc5c-ac8575f29efd",
                "from"
            ),
            Err(AuthError::NotAuthorized { .. })
        ));
        assert_eq!(
            auth.authenticate_bearer(None),
            Err(AuthError::MissingAuthorization)
        );
        assert_eq!(
            auth.authenticate_bearer(Some("Bearer nope")),
            Err(AuthError::InvalidBearer)
        );
        assert_eq!(
            auth.authenticate_bearer(Some("Basic abc")),
            Err(AuthError::MissingBearer)
        );

        let admin = auth
            .authenticate_bearer(Some(&format!("bearer {ADMIN}")))
            .unwrap();
        assert!(admin.admin);
        assert!(assert_agent_authorized(&admin, "codex-x", "for").is_ok());
        assert!(assert_admin(&admin).is_ok());
        assert_eq!(
            assert_admin(&claude),
            Err(AuthError::NotAdmin("claude".to_owned()))
        );
    }

    #[test]
    fn signatures_match_the_v1_clients() {
        let token = "k".repeat(64);
        let cases = [
            (
                "GET",
                "http://127.0.0.1:7447/claude/hook?agent=claude-api-a1b2&event=SessionStart",
                None,
                "sha256=0bbcd943dd3a2fe618f8c57a97d1dc29d6f610a4512869b2ae117c40fb1dad1c",
            ),
            (
                "POST",
                "http://127.0.0.1:7447/presence",
                Some(json!({"online": true, "agent": "claude-api-a1b2"})),
                "sha256=4ce0a20046632ad90f7a89068f16ac663fa868b2c02897d285260ab8679e3ac8",
            ),
            (
                "post",
                "/codex/hook",
                Some(
                    json!({"session_id": "x", "nested": {"b": 2, "a": [1, "é", null, false]},
                            "text": "line\nquote\" ctrl\u{1} emoji 😀 tab\t"}),
                ),
                "sha256=a9b25c8d8b877df41feff7f39b2e4087b11d0e81dcc3f30890cb2ebaf7fa711e",
            ),
        ];
        for (method, url, body, expected) in cases {
            let headers = sign_request(
                "claude",
                &token,
                (method, url, body.as_ref()),
                NOW,
                Some("nonce-abc123"),
                None,
            );
            assert_eq!(headers[3].1, expected, "{method} {url}");
        }
    }

    fn header_map(
        headers: &[(&'static str, String)],
        rename: Option<&str>,
    ) -> HashMap<String, String> {
        headers
            .iter()
            .map(|(name, value)| {
                let name = match rename {
                    Some(prefix) => name.replace("x-inband-", prefix),
                    None => (*name).to_owned(),
                };
                (name, value.clone())
            })
            .collect()
    }

    #[test]
    fn signed_requests_are_fresh_unique_and_authentic() {
        let auth = runtime();
        let body = json!({"agent": "claude-api-a1b2", "online": true});
        let headers = header_map(
            &sign_request(
                "claude",
                CLAUDE,
                ("POST", "http://127.0.0.1:7447/presence", Some(&body)),
                NOW,
                Some("nonce-1x"),
                None,
            ),
            None,
        );
        let lookup = |name: &str| headers.get(name).map(String::as_str);
        let request = signed("POST", "/presence", Some(&body), &headers, &lookup);
        assert_eq!(
            auth.authenticate_signed(&request, NOW + 30_000)
                .unwrap()
                .client_id,
            "claude"
        );
        assert_eq!(
            auth.authenticate_signed(&request, NOW + 31_000),
            Err(AuthError::Replay)
        );
        assert_eq!(
            auth.authenticate_signed(&request, NOW + 400_000),
            Err(AuthError::Stale)
        );

        let other_body = json!({"agent": "claude-lead-0000", "online": true});
        let tampered = signed("POST", "/presence", Some(&other_body), &headers, &lookup);
        assert_eq!(
            auth.authenticate_signed(&tampered, NOW),
            Err(AuthError::InvalidSignature)
        );

        let none = HashMap::new();
        let empty = |name: &str| none.get(name).map(String::as_str);
        let missing = signed("GET", "/health", None, &none, &empty);
        assert_eq!(
            auth.authenticate_signed(&missing, NOW),
            Err(AuthError::MissingSignedHeaders)
        );
    }

    #[test]
    fn a_signed_session_cannot_be_changed_added_or_removed() {
        let auth = runtime();
        let url = "http://127.0.0.1:7447/mcp";
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call"});
        let sign = |nonce: &str, session: Option<&str>| {
            header_map(
                &sign_request(
                    "claude",
                    CLAUDE,
                    ("POST", url, Some(&body)),
                    NOW,
                    Some(nonce),
                    session,
                ),
                None,
            )
        };
        let verify = |headers: &HashMap<String, String>| {
            let lookup = |name: &str| headers.get(name).map(String::as_str);
            auth.verify_signed(&signed("POST", url, Some(&body), headers, &lookup), NOW)
        };

        let (info, session) = verify(&sign("nonce-s1", Some("session-a"))).unwrap();
        assert_eq!(info.client_id, "claude");
        assert_eq!(session.as_deref(), Some("session-a"));

        let mut changed = sign("nonce-s2", Some("session-a"));
        changed.insert(SESSION_HEADER.to_owned(), "session-b".to_owned());
        assert_eq!(verify(&changed), Err(AuthError::InvalidSignature));

        let mut removed = sign("nonce-s3", Some("session-a"));
        removed.remove(SESSION_HEADER);
        assert_eq!(verify(&removed), Err(AuthError::InvalidSignature));

        let mut added = sign("nonce-s4", None);
        added.insert(SESSION_HEADER.to_owned(), "session-a".to_owned());
        assert_eq!(verify(&added), Err(AuthError::InvalidSignature));

        for bad in ["", "a\nb", "a b", &"s".repeat(129)] {
            let mut invalid = sign("nonce-s5", None);
            invalid.insert(SESSION_HEADER.to_owned(), bad.to_owned());
            assert_eq!(verify(&invalid), Err(AuthError::InvalidSession), "{bad:?}");
        }
    }

    #[test]
    fn bearer_requests_carry_no_session() {
        let auth = runtime();
        let headers: HashMap<String, String> = [
            ("authorization".to_owned(), format!("Bearer {CLAUDE}")),
            (SESSION_HEADER.to_owned(), "session-a".to_owned()),
        ]
        .into();
        let lookup = |name: &str| headers.get(name).map(String::as_str);
        let (info, session) = auth
            .authenticate_request(&signed("POST", "/mcp", None, &headers, &lookup), NOW)
            .unwrap();
        assert_eq!(info.mode, AuthMode::Bearer);
        assert_eq!(session, None);
    }

    #[test]
    fn accepts_the_pre_rename_header_names() {
        let auth = runtime();
        let url = "http://127.0.0.1:7447/claude/hook?agent=claude-api-a1b2&event=SessionStart";
        let headers = header_map(
            &sign_request(
                "claude",
                CLAUDE,
                ("GET", url, None),
                NOW,
                Some("legacy-1"),
                None,
            ),
            Some("x-agent-bridge-"),
        );
        let lookup = |name: &str| headers.get(name).map(String::as_str);
        let request = signed("GET", url, None, &headers, &lookup);
        assert_eq!(
            auth.authenticate(&request, NOW).unwrap().mode,
            AuthMode::Hmac
        );
    }

    #[test]
    fn rejects_bad_nonces_and_unknown_clients() {
        let auth = runtime();
        let headers = header_map(
            &sign_request(
                "claude",
                CLAUDE,
                ("GET", "/health", None),
                NOW,
                Some("bad nonce!"),
                None,
            ),
            None,
        );
        let lookup = |name: &str| headers.get(name).map(String::as_str);
        assert_eq!(
            auth.authenticate_signed(&signed("GET", "/health", None, &headers, &lookup), NOW),
            Err(AuthError::InvalidNonce)
        );
        let ghost = header_map(
            &sign_request(
                "ghost",
                CLAUDE,
                ("GET", "/health", None),
                NOW,
                Some("nonce-ok"),
                None,
            ),
            None,
        );
        let lookup = |name: &str| ghost.get(name).map(String::as_str);
        assert_eq!(
            auth.authenticate_signed(&signed("GET", "/health", None, &ghost, &lookup), NOW),
            Err(AuthError::UnknownClient)
        );
    }

    #[test]
    fn rejects_weak_and_duplicate_tokens() {
        let build = |token: &str| {
            let mut clients = BTreeMap::new();
            clients.insert("bad".to_owned(), client(token, &["bad-*"], false));
            AuthRuntime::new(
                Some(&AuthConfig {
                    required: true,
                    clients,
                }),
                &EnvMap::new(),
            )
        };
        assert_eq!(
            build("short").unwrap_err(),
            AuthError::TokenTooShort("bad".to_owned())
        );
        assert_eq!(
            build("change-me-secret-change-me-secret").unwrap_err(),
            AuthError::TokenPlaceholder("bad".to_owned())
        );
        let mut clients = BTreeMap::new();
        clients.insert("a".to_owned(), client(CLAUDE, &["a-*"], false));
        clients.insert("b".to_owned(), client(CLAUDE, &["b-*"], false));
        assert!(matches!(
            AuthRuntime::new(
                Some(&AuthConfig {
                    required: true,
                    clients
                }),
                &EnvMap::new()
            ),
            Err(AuthError::DuplicateToken(_))
        ));
        assert_eq!(
            AuthRuntime::new(
                Some(&AuthConfig {
                    required: true,
                    clients: BTreeMap::new()
                }),
                &EnvMap::new()
            )
            .unwrap_err(),
            AuthError::NoClients
        );
    }

    #[test]
    fn tokens_can_come_from_the_environment() {
        let mut clients = BTreeMap::new();
        clients.insert(
            "claude".to_owned(),
            AuthClientConfig {
                token: None,
                token_env: Some("INBAND_CLAUDE_TOKEN".to_owned()),
                agents: vec!["claude-*".to_owned()],
                directory: None,
                admin: false,
            },
        );
        let env: EnvMap = [("INBAND_CLAUDE_TOKEN".to_owned(), CLAUDE.to_owned())].into();
        let auth = AuthRuntime::new(
            Some(&AuthConfig {
                required: true,
                clients,
            }),
            &env,
        )
        .unwrap();
        assert!(
            auth.authenticate_bearer(Some(&format!("Bearer {CLAUDE}")))
                .is_ok()
        );
    }

    #[test]
    fn disabled_auth_grants_full_access() {
        let auth = AuthRuntime::new(None, &EnvMap::new()).unwrap();
        assert!(!auth.required());
        assert!(auth.authenticate_bearer(None).unwrap().admin);
    }

    #[test]
    fn directory_and_family_scopes() {
        let mut clients = BTreeMap::new();
        clients.insert("claude".to_owned(), client(CLAUDE, &["claude-*"], false));
        let mut codex = client(CODEX, &["codex-*"], false);
        codex.directory = Some(vec!["*".to_owned()]);
        clients.insert("codex".to_owned(), codex);
        let auth = AuthRuntime::new(
            Some(&AuthConfig {
                required: true,
                clients,
            }),
            &EnvMap::new(),
        )
        .unwrap();
        let claude = auth
            .authenticate_bearer(Some(&format!("Bearer {CLAUDE}")))
            .unwrap();
        let codex = auth
            .authenticate_bearer(Some(&format!("Bearer {CODEX}")))
            .unwrap();
        assert_eq!(
            directory_patterns(&claude),
            Some(&["claude-*".to_owned()][..])
        );
        assert_eq!(directory_patterns(&codex), None);
        assert_eq!(visible_patterns(&codex), Some(&["codex-*".to_owned()][..]));
        assert!(assert_family_authorized(&claude, "claude", "prefix").is_ok());
        assert!(assert_family_authorized(&claude, "codex", "prefix").is_err());
    }

    #[test]
    fn pattern_matching() {
        assert!(agent_matches_pattern("claude-api-a1b2", "claude-*"));
        assert!(agent_matches_pattern("Claude-X", "claude-*"));
        assert!(!agent_matches_pattern("codex-x", "claude-*"));
        assert!(agent_matches_pattern("anything", "*"));
        assert!(agent_matches_pattern("opencode", "opencode"));
        assert!(!agent_matches_pattern("opencode-2", "opencode"));
    }
}
