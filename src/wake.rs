//! Wake requests sent to idle Codex and `OpenCode` sessions. The real clients come in phase 2;
//! the bridge only sees the [`WakeDispatch`] trait.

use std::future::Future;
use std::pin::Pin;

use crate::config::WakeTarget;

/// What happened to one wake attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeDisposition {
    Started,
    Queued,
    DeferredActiveTurn,
    Failed,
}

impl WakeDisposition {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Queued => "queued",
            Self::DeferredActiveTurn => "deferred-active-turn",
            Self::Failed => "failed",
        }
    }

    /// Started and queued wakes count as success: they stop retries and start the debounce.
    #[must_use]
    pub fn is_success(self) -> bool {
        matches!(self, Self::Started | Self::Queued)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeResult {
    pub disposition: WakeDisposition,
    pub detail: String,
}

impl WakeResult {
    #[must_use]
    pub fn failed(detail: impl Into<String>) -> Self {
        Self {
            disposition: WakeDisposition::Failed,
            detail: detail.into(),
        }
    }
}

/// The identity a wake needs. The prompt never contains message content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeInput {
    pub recipient: String,
    pub session_id: Option<String>,
    pub mailbox: Option<String>,
    pub prompt: String,
}

pub type WakeFuture = Pin<Box<dyn Future<Output = WakeResult> + Send>>;

/// Sends a wake to a client.
pub trait WakeDispatch: Send + Sync {
    fn dispatch(&self, target: &WakeTarget, input: WakeInput) -> WakeFuture;
}
