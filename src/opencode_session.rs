//! @file opencode_session.rs
//! @brief The mailboxes of `OpenCode` sessions.
//!
//! @details An `OpenCode` mailbox is `opencode-<16 hex chars>`.
//! The hex chars come from the SHA-256 digest of the session id.
//! A session id has upper case and lower case letters, but a mailbox name has only lower case.
//! Two ids can differ only in case. A lower case copy of the id can thus give two sessions the same mailbox.
//! The digest of the exact id prevents this.
//! The daemon calculates the digest again to make sure that a session acts only for its own mailbox.

use sha2::{Digest, Sha256};

/// @brief The prefix of all `OpenCode` mailboxes.
pub const OPENCODE_FAMILY: &str = "opencode";
/// @brief The number of hex chars of the digest in a mailbox name.
const DIGEST_CHARS: usize = 16;
/// @brief The maximum length of a session id.
const MAX_SESSION_ID: usize = 128;

/// @brief The error for a session id that is not valid.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid OpenCode session id \"{0}\"")]
pub struct InvalidSessionId(pub String);

/// @brief Gives the mailbox of an `OpenCode` session.
///
/// @param session_id The exact session id.
/// @return The name `opencode-<16 hex chars>`.
/// @throws InvalidSessionId The id is empty, longer than 128 chars, or has chars other than ASCII letters, digits, `_` and `-`.
pub fn mailbox(session_id: &str) -> Result<String, InvalidSessionId> {
    if !is_valid_session_id(session_id) {
        return Err(InvalidSessionId(session_id.to_owned()));
    }
    let digest = hex::encode(Sha256::digest(session_id.as_bytes()));
    Ok(format!("{OPENCODE_FAMILY}-{}", &digest[..DIGEST_CHARS]))
}

/// @brief Tells if a session id is valid.
///
/// @details A valid id has 1 to 128 ASCII letters, digits, `_` and `-`.
/// Such an id is safe as one segment of a URL path.
/// It has no `/`, and it cannot be `.` or `..`.
///
/// @param session_id The session id to examine.
/// @return True for a valid id.
#[must_use]
pub fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= MAX_SESSION_ID
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// @brief Tells if a name is the mailbox of an `OpenCode` session.
///
/// @param name The mailbox name.
/// @return True for `opencode-<16 hex chars>`.
#[must_use]
pub fn is_session_mailbox(name: &str) -> bool {
    name.strip_prefix("opencode-").is_some_and(|digest| {
        digest.len() == DIGEST_CHARS
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// @brief Tells if a session owns a mailbox.
///
/// @param session_id The session id of the request.
/// @param mailbox_name The mailbox name.
/// @return True when the mailbox of the session is `mailbox_name`.
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
