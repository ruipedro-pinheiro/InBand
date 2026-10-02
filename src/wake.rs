//! @file wake.rs
//! @brief Wake requests to idle Codex and `OpenCode` sessions.
//!
//! @details The bridge sends a wake through the [`WakeDispatch`] trait.
//! The real dispatcher is in the dispatch module.
//! A wake never contains message content.

use std::future::Future;
use std::pin::Pin;

use crate::config::WakeTarget;

/// @brief The result of one wake attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeDisposition {
    /// The client started a new turn for the wake.
    Started,
    /// The client put the wake in its queue, after the current turn.
    Queued,
    /// The wake did not reach the client.
    Failed,
}

impl WakeDisposition {
    /// @brief Gives the name of the disposition.
    ///
    /// @return The name, as the daemon writes it in the logs and the database.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Queued => "queued",
            Self::Failed => "failed",
        }
    }

    /// @brief Tells if the wake reached the client.
    ///
    /// @details A started wake and a queued wake are successful.
    /// A successful wake stops the retries and starts the debounce time.
    ///
    /// @return True for a started or a queued wake.
    #[must_use]
    pub fn is_success(self) -> bool {
        matches!(self, Self::Started | Self::Queued)
    }
}

/// @brief The disposition of a wake attempt, with a short description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeResult {
    pub disposition: WakeDisposition,
    /// A short description for the log.
    pub detail: String,
}

impl WakeResult {
    /// @brief Makes the result of a failed wake.
    ///
    /// @param detail The cause of the failure.
    /// @return A result with the disposition `Failed`.
    #[must_use]
    pub fn failed(detail: impl Into<String>) -> Self {
        Self {
            disposition: WakeDisposition::Failed,
            detail: detail.into(),
        }
    }
}

/// @brief The data that a wake needs.
///
/// @details The prompt never contains message content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeInput {
    /// The mailbox that received the mail.
    pub recipient: String,
    /// The session to wake. The Codex wake needs it.
    pub session_id: Option<String>,
    /// The mailbox to name in the prompt. `None` uses `recipient`.
    pub mailbox: Option<String>,
    /// The wake prompt of the configuration.
    pub prompt: String,
}

/// @brief The future of one wake attempt.
pub type WakeFuture = Pin<Box<dyn Future<Output = WakeResult> + Send>>;

/// @brief Sends wakes to clients.
pub trait WakeDispatch: Send + Sync {
    /// @brief Sends one wake.
    ///
    /// @param target The wake target from the configuration.
    /// @param input The mailbox, the session and the prompt of the wake.
    /// @return The future of the wake attempt.
    fn dispatch(&self, target: &WakeTarget, input: WakeInput) -> WakeFuture;
}
