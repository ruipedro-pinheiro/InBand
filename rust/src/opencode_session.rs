//! OpenCode session mailboxes: `opencode-<16 hex chars>`, derived from the session id.
//!
//! OpenCode session ids mix upper and lower case (`ses_f0311d340ffenkofYtqi2xYpYM`), and mailbox
//! names are lower case. Lower-casing the id could give two sessions the same mailbox, so the
//! mailbox holds a digest of the exact id instead. The daemon recomputes the digest to check that
//! a request signed for a session acts only for that session's mailbox.

use sha2::{Digest, Sha256};

pub const OPENCODE_FAMILY: &str = "opencode";
const DIGEST_CHARS: usize = 16;
const MAX_SESSION_ID: usize = 128;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid OpenCode session id \"{0}\"")]
pub struct InvalidSessionId(pub String);

/// The mailbox of an OpenCode session.
///
/// # Errors
/// Returns an error for an empty id, an id longer than 128 chars, or chars other than ASCII
/// letters, digits, `_` and `-`.
pub fn mailbox(session_id: &str) -> Result<String, InvalidSessionId> {
    if !is_valid_session_id(session_id) {
        return Err(InvalidSessionId(session_id.to_owned()));
    }
    let digest = hex::encode(Sha256::digest(session_id.as_bytes()));
    Ok(format!("{OPENCODE_FAMILY}-{}", &digest[..DIGEST_CHARS]))
}

/// True for 1 to 128 ASCII letters, digits, `_` and `-`. Such an id is safe as one URL path segment:
/// it holds no `/`, and it can never be `.` or `..`.
#[must_use]
pub fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= MAX_SESSION_ID
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// True for an `opencode-<16 hex chars>` mailbox.
#[must_use]
pub fn is_session_mailbox(name: &str) -> bool {
    name.strip_prefix("opencode-").is_some_and(|digest| {
        digest.len() == DIGEST_CHARS
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// True when `session_id` is the session that owns `mailbox`.
#[must_use]
pub fn owns(session_id: &str, mailbox_name: &str) -> bool {
    mailbox(session_id).is_ok_and(|derived| derived == mailbox_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "ses_f0311d340ffenkofYtqi2xYpYM";

    #[test]
    fn derives_a_stable_lower_case_mailbox() {
        let name = mailbox(SESSION).unwrap();
        assert_eq!(name, mailbox(SESSION).unwrap());
        assert!(is_session_mailbox(&name));
        assert!(crate::config::is_agent_name(&name));
        assert!(owns(SESSION, &name));
    }

    #[test]
    fn ids_that_differ_only_in_case_get_different_mailboxes() {
        let lower = SESSION.to_ascii_lowercase();
        assert_ne!(mailbox(SESSION).unwrap(), mailbox(&lower).unwrap());
        assert!(!owns(&lower, &mailbox(SESSION).unwrap()));
    }

    #[test]
    fn rejects_ids_that_could_smuggle_other_text() {
        for bad in ["", "ses one", "ses_\u{200B}x", "../ses", &"a".repeat(129)] {
            assert!(mailbox(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_digest_mailboxes_are_session_mailboxes() {
        assert!(!is_session_mailbox("opencode"));
        assert!(!is_session_mailbox("opencode-ABCDEF0123456789"));
        assert!(!is_session_mailbox("opencode-0123"));
        assert!(is_session_mailbox("opencode-0123456789abcdef"));
    }
}
