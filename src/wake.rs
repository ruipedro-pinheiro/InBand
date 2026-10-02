//! Wake requests for idle Codex and `OpenCode` sessions.
//!
//! The bus sends wakes through the [`WakeDispatch`] trait, so the tests can replace the real
//! dispatcher of the `dispatch` module. A wake never contains message content.

use std::future::Future;
use std::pin::Pin;

use crate::config::WakeTarget;

/// The outcome of one wake attempt.
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
    /// Returns the name of the disposition, as the logs and the `wakes` table store it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Queued => "queued",
            Self::Failed => "failed",
        }
    }

    /// Returns `true` when the wake reached the client.
    ///
    /// A successful wake stops the retries and starts the debounce time.
    #[must_use]
    pub fn is_success(self) -> bool {
        matches!(self, Self::Started | Self::Queued)
    }
}

/// The outcome of a wake attempt, with a short description for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeResult {
    pub disposition: WakeDisposition,
    /// A short description for the log.
    pub detail: String,
}

impl WakeResult {
    /// Creates the result of a failed wake.
    #[must_use]
    pub fn failed(detail: impl Into<String>) -> Self {
        Self {
            disposition: WakeDisposition::Failed,
            detail: detail.into(),
        }
    }
}

/// The data that a wake needs. The prompt never contains message content.
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

pub type WakeFuture = Pin<Box<dyn Future<Output = WakeResult> + Send>>;

/// Sends wakes to clients.
pub trait WakeDispatch: Send + Sync {
    /// Sends one wake to the client of `target`.
    fn dispatch(&self, target: &WakeTarget, input: WakeInput) -> WakeFuture;
}
