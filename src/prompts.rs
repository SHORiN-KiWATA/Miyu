use base64::Engine;

include!(concat!(env!("OUT_DIR"), "/default_miyu_prompt.rs"));

pub const YOLO_REMINDER: &str = include_str!("prompts/yolo.md");
pub const PLAN_REMINDER: &str = include_str!("prompts/plan.md");
pub const MEME_DESCRIPTION_PROMPT: &str = include_str!("prompts/meme-description.md");
pub const INPUT_METHOD_DIAGNOSIS_PROMPT: &str =
    include_str!("prompts/linux-input-method-diagnose.md");
pub const GAME_COMPATIBILITY_PROMPT: &str = include_str!("prompts/linux-game-compatibility.md");

pub fn default_system_prompt() -> String {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(OBFUSCATED_DEFAULT_SYSTEM_PROMPT)
        .expect("embedded default prompt must be valid base64");
    let decoded = bytes
        .into_iter()
        .enumerate()
        .map(|(index, byte)| byte ^ PROMPT_MASK[index % PROMPT_MASK.len()])
        .collect::<Vec<_>>();
    String::from_utf8(decoded).expect("embedded default prompt must be valid utf-8")
}
