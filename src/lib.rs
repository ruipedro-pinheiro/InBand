//! @file lib.rs
//! @brief InBand: a local message bus for coding agents.
//!
//! @details One daemon carries mail between Claude Code, Codex and `OpenCode` sessions.

pub mod auth;
pub mod bridge;
pub mod client;
pub mod codex_session;
pub mod config;
pub mod daemon;
pub mod db;
pub mod dispatch;
pub mod hooks;
pub mod http;
pub mod install;
pub mod mcp;
pub mod opencode_cli;
pub mod opencode_session;
pub mod protocol;
pub mod sanitize;
pub mod shim;
#[cfg(test)]
pub mod test_support;
pub mod tokens;
pub mod wake;
