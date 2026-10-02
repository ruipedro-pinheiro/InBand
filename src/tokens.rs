//! `tokens.env` loading, the `AGENT_BRIDGE_*` aliases of installs made before the rename,
//! and per-client token lookup.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::EnvMap;

const LEGACY_PREFIX: &str = "AGENT_BRIDGE_";
const PREFIX: &str = "INBAND_";
const CURRENT_FILE: &str = ".local/share/mcp-servers/inband/tokens.env";
const LEGACY_FILE: &str = ".local/share/mcp-servers/agent-bridge/tokens.env";
/// Permission bits for the group and other users.
const GROUP_OTHER_BITS: u32 = 0o077;

/// True when only the owner can read or write the file.
#[must_use]
pub fn is_private_mode(mode: u32) -> bool {
    mode & GROUP_OTHER_BITS == 0
}

#[derive(Debug, thiserror::Error)]
pub enum TokensError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
}

/// Result of [`load_token_env_file`].
#[derive(Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    Loaded { path: PathBuf, private: bool },
    Missing(PathBuf),
    NoHome,
}

/// Copies every `AGENT_BRIDGE_X` value to `INBAND_X` when `INBAND_X` is not set.
pub fn apply_legacy_env(env: &mut EnvMap) {
    let legacy: Vec<(String, String)> = env
        .iter()
        .filter_map(|(name, value)| {
            let rest = name.strip_prefix(LEGACY_PREFIX)?;
            Some((format!("{PREFIX}{rest}"), value.clone()))
        })
        .collect();
    for (name, value) in legacy {
        env.entry(name).or_insert(value);
    }
}

fn default_token_file(env: &EnvMap, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    if let Some(path) = env
        .get("INBAND_TOKENS_FILE")
        .filter(|path| !path.is_empty())
    {
        return Some(PathBuf::from(path));
    }
    let home = Path::new(env.get("HOME").filter(|home| !home.is_empty())?);
    let current = home.join(CURRENT_FILE);
    let legacy = home.join(LEGACY_FILE);
    Some(if exists(&current) || !exists(&legacy) {
        current
    } else {
        legacy
    })
}

/// Adds the `NAME=value` lines of a token file to `env`. Variables already set win.
pub fn read_token_lines(text: &str, env: &mut EnvMap) {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((name, raw_value)) = trimmed.split_once('=') else {
            continue;
        };
        let valid_name = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        if !valid_name || env.contains_key(name) {
            continue;
        }
        let quoted = raw_value.len() >= 2
            && ((raw_value.starts_with('"') && raw_value.ends_with('"'))
                || (raw_value.starts_with('\'') && raw_value.ends_with('\'')));
        let value = if quoted {
            &raw_value[1..raw_value.len() - 1]
        } else {
            raw_value
        };
        env.insert(name.to_owned(), value.to_owned());
    }
}

/// Loads the token file into `env`: `INBAND_TOKENS_FILE`, else the current install path, else
/// the path used before the rename.
///
/// # Errors
/// Returns an error when the file exists but cannot be read.
pub fn load_token_env_file(env: &mut EnvMap) -> Result<LoadOutcome, TokensError> {
    apply_legacy_env(env);
    let Some(path) = default_token_file(env, Path::exists) else {
        return Ok(LoadOutcome::NoHome);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LoadOutcome::Missing(path));
        }
        Err(source) => return Err(TokensError::Read { path, source }),
    };
    let private =
        std::fs::metadata(&path).is_ok_and(|meta| is_private_mode(meta.permissions().mode()));
    read_token_lines(&text, env);
    apply_legacy_env(env);
    Ok(LoadOutcome::Loaded { path, private })
}

/// The token of a client: `INBAND_<CLIENT>_TOKEN`, else `INBAND_TOKEN`.
#[must_use]
pub fn client_token<'a>(client_id: &str, env: &'a EnvMap) -> Option<&'a str> {
    let scoped: String = client_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    env.get(&format!("{PREFIX}{scoped}_TOKEN"))
        .or_else(|| env.get("INBAND_TOKEN"))
        .map(String::as_str)
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

    #[test]
    fn reads_tokens_without_overwriting_existing_values() {
        let mut values = env(&[("INBAND_CLAUDE_TOKEN", "already-set")]);
        read_token_lines(
            "INBAND_CLAUDE_TOKEN=from-file\nINBAND_CODEX_TOKEN='quoted'\n# comment\nIGNORED lowercase=value\nbad-name=x\n\n",
            &mut values,
        );
        assert_eq!(client_token("claude", &values), Some("already-set"));
        assert_eq!(client_token("codex", &values), Some("quoted"));
        assert!(!values.contains_key("bad-name"));
    }

    #[test]
    fn prefers_the_client_token_over_the_generic_one() {
        let values = env(&[
            ("INBAND_TOKEN", "generic"),
            ("INBAND_CLAUDE_TOKEN", "claude"),
        ]);
        assert_eq!(client_token("claude", &values), Some("claude"));
        assert_eq!(client_token("codex", &values), Some("generic"));
        assert_eq!(client_token("codex", &env(&[])), None);
    }

    #[test]
    fn legacy_names_count_unless_the_current_name_is_set() {
        let mut values = env(&[
            ("AGENT_BRIDGE_CLAUDE_TOKEN", "old"),
            ("AGENT_BRIDGE_CODEX_TOKEN", "old-codex"),
            ("INBAND_CODEX_TOKEN", "new-codex"),
        ]);
        apply_legacy_env(&mut values);
        assert_eq!(client_token("claude", &values), Some("old"));
        assert_eq!(client_token("codex", &values), Some("new-codex"));
    }

    #[test]
    fn chooses_the_token_file() {
        let home = env(&[("HOME", "/h")]);
        let none = |_: &Path| false;
        assert_eq!(
            default_token_file(&home, none),
            Some(PathBuf::from("/h").join(CURRENT_FILE))
        );
        let only_legacy = |path: &Path| path.ends_with(LEGACY_FILE);
        assert_eq!(
            default_token_file(&home, only_legacy),
            Some(PathBuf::from("/h").join(LEGACY_FILE))
        );
        let both = |_: &Path| true;
        assert_eq!(
            default_token_file(&home, both),
            Some(PathBuf::from("/h").join(CURRENT_FILE))
        );
        let explicit = env(&[("HOME", "/h"), ("INBAND_TOKENS_FILE", "/x/t.env")]);
        assert_eq!(
            default_token_file(&explicit, none),
            Some(PathBuf::from("/x/t.env"))
        );
        assert_eq!(default_token_file(&env(&[]), none), None);
    }

    #[test]
    fn loads_a_private_file_and_reports_permissions() {
        let dir = std::env::temp_dir().join(format!("inband-tokens-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tokens.env");
        std::fs::write(&file, "INBAND_CLAUDE_TOKEN=abc\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

        let mut values = env(&[("INBAND_TOKENS_FILE", file.to_str().unwrap())]);
        let outcome = load_token_env_file(&mut values).unwrap();
        assert_eq!(
            outcome,
            LoadOutcome::Loaded {
                path: file.clone(),
                private: false
            }
        );
        assert_eq!(client_token("claude", &values), Some("abc"));

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut values = env(&[("INBAND_TOKENS_FILE", file.to_str().unwrap())]);
        assert_eq!(
            load_token_env_file(&mut values).unwrap(),
            LoadOutcome::Loaded {
                path: file,
                private: true
            }
        );

        let missing = dir.join("absent.env");
        let mut values = env(&[("INBAND_TOKENS_FILE", missing.to_str().unwrap())]);
        assert_eq!(
            load_token_env_file(&mut values).unwrap(),
            LoadOutcome::Missing(missing)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
