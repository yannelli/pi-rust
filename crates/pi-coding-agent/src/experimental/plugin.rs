//! The plugin API module `@earendil-works/pi-coding-agent/experimental/plugin` (upstream `plugin.ts`). Rust
//! facets use the service tokens below; bundled JavaScript facets import the module these values describe.

pub use super::services::agent_controller::{
    AGENT_CONTROLLER, AgentCompactionRequest, AgentController, AgentOperationError, AgentOperationResponse,
    AgentPromptRequest, AgentPromptResult, AgentQueueResponse,
};
pub use super::services::presentation_ui::{PRESENTATION_UI, PresentationSelectItem, PresentationUi};
pub use super::services::slash_commands::{
    SLASH_COMMANDS, SlashCommandCompletion, SlashCommandContribution, SlashCommandRunResult,
};

pub const PI_PLUGIN_API: &str = "@earendil-works/pi-coding-agent/experimental/plugin";

/// The module's exports as Chord service tokens (`{ id, local }`), as `defineService` creates them.
pub fn plugin_api_exports() -> serde_json::Value {
    serde_json::json!({
        "AgentController": { "id": "pi.agent-controller", "local": false },
        "PresentationUI": { "id": "pi.local.presentation-ui", "local": true },
        "SlashCommands": { "id": "pi.local.slash-commands", "local": true },
    })
}
