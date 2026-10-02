//! Phase 0 spike: check that an rmcp stdio server can push a `claude/channel` event to Claude Code.

use std::time::Duration;

use rmcp::{
    ServerHandler, ServiceExt,
    model::{
        CustomNotification, ExperimentalCapabilities, JsonObject, ProtocolVersion,
        ServerCapabilities, ServerConfig, ServerNotification,
    },
    service::{NotificationContext, RoleServer},
    transport::stdio,
};
use serde_json::json;

struct Spike;

impl ServerHandler for Spike {
    // Claude Code skips channel events on 2026-07-28 and later, which have no unsolicited notifications.
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        std::borrow::Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))
    }

    fn get_info(&self) -> ServerConfig {
        let mut experimental = ExperimentalCapabilities::new();
        experimental.insert("claude/channel".to_owned(), JsonObject::new());
        let mut capabilities = ServerCapabilities::default();
        capabilities.experimental = Some(experimental);
        // Claude Code skips channel events on revisions without unsolicited notifications.
        ServerConfig::new(capabilities)
            .with_protocol_version(ProtocolVersion::V_2025_11_25)
            .with_instructions(
            "Spike channel. Events arrive as <channel source=\"spike\">. Reply in the terminal with the text of the event.",
        )
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let event = CustomNotification::new(
            "notifications/claude/channel",
            Some(json!({
                "content": "Hello from the Rust spike. Say SPIKE-OK in the terminal.",
                "meta": { "from": "rust-spike", "from_role": "worker" },
            })),
        );
        if let Err(error) = context
            .peer
            .send_notification(ServerNotification::CustomNotification(event))
            .await
        {
            eprintln!("spike: notification failed: {error}");
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = Spike.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
