#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum AgentResponseKind {
    Chat,
    CommandSuggestion,
    Correction,
    ToolCall,
    Plan,
    Memory,
    Ignore,
}
