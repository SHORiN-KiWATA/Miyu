use super::{ToolRegistry, ToolSpec};
use anyhow::bail;
use serde_json::json;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new(
        "ask_question",
        "Ask the user a structured question during the current response.",
        json!({"type":"object","properties":{},"additionalProperties":false}),
        |_| async move { bail!("ask_question requires an active interactive Miyu session") },
    ));
}
