//! @file tokens.rs
//! @brief Reads `tokens.env` and finds the token of each client.
//!
//! @details Each client (Claude Code, Codex, `OpenCode`, admin) has its own token.
//! Before the rename to InBand, the variables started with `AGENT_BRIDGE_`.
//! These old names still work.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::EnvMap;

/// @brief The variable prefix before the rename.
const LEGACY_PREFIX: &str = "AGENT_BRIDGE_";
/// @brief The variable prefix of InBand.
const PREFIX: &str = "INBAND_";
/// @brief The token file, relative to the home directory.
const CURRENT_FILE: &str = ".local/share/mcp-servers/inband/tokens.env";
/// @brief The token file before the rename, relative to the home directory.
const LEGACY_FILE: &str = ".local/share/mcp-servers/agent-bridge/tokens.env";
/// @brief The permission bits of the group and of the other users.
const GROUP_OTHER_BITS: u32 = 0o077;

/// @brief Tells if only the owner can read and write a file.
///
/// @param mode The Unix permission bits of the file.
/// @return True when the group and the other users have no permission.
#[must_use]
pub fn is_private_mode(mode: u32) -> bool {
    mode & GROUP_OTHER_BITS == 0
}

/// @brief The error when the token file exists but cannot be read.
#[derive(Debug, thiserror::Error)]
pub enum TokensError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
}

/// @brief The result of [`load_token_env_file`].
#[derive(Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    /// The file was read. `private` is false when other users can read it.
    Loaded { path: PathBuf, private: bool },
    /// There is no file at this path.
    Missing(PathBuf),
    /// There is no home directory, so there is no default path.
    NoHome,
}

/// @brief Copies each `AGENT_BRIDGE_X` value to `INBAND_X`.
///
/// @details A value that is already in `INBAND_X` stays.
///
/// @param env The variables, changed in place.
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

/// @brief Finds the token file.
///
/// @details The order is: `INBAND_TOKENS_FILE`, then the current path, then the path before the rename.
/// The old path is used only when it exists and the current path does not.
///
/// @param env The variables of the process.
/// @param exists Tells if a file exists. The tests replace it.
/// @return The path, or `None` when there is no home directory.
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

/// @brief Adds the `NAME=value` lines of a token file to the variables.
///
/// @details Only names in upper case with `_` and digits are read.
/// A variable that is already set keeps its value.
///
/// @param text The text of the token file.
/// @param env The variables, changed in place.
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

/// @brief Reads the token file into the variables.
///
/// @param env The variables, changed in place.
/// @return Where the file is, and if only its owner can read it.
/// @throws TokensError The file exists, but cannot be read.
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

/// @brief Gives the token of a client.
///
/// @param client_id The client name, for example `claude`.
/// @param env The variables.
/// @return `INBAND_<CLIENT>_TOKEN`, else `INBAND_TOKEN`, else `None`.
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

    /// @brief Makes a set of variables for a test.
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
