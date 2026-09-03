//! serde 的默认值函数。
//!
//! 每个都被某个字段的 `#[serde(default = "...")]` 指名，所以**函数名是配置文件
//! 格式的一部分**——改名等于让老配置读不出来。同名的 `is_default_*` 用于
//! `skip_serializing_if`，让写回的配置只留下用户真正改过的项。
//!
//! 值本身也是契约：`real_context_defaults_match_the_deployed_contract` 这类测试
//! 守的就是「改默认值等于改所有没显式配置的用户的行为」。

use crate::config::*;

pub(crate) fn is_default_platform_command_prefix(value: &String) -> bool {
    value == DEFAULT_PLATFORM_COMMAND_PREFIX
}

pub(crate) fn default_persona_reminder_interval() -> u32 {
    // 08-23 工具体制 A/B:interval=5 时探针全过 5/12,=3 时 8/12——提醒
    // 新鲜度直接决定工具冲刷后的风格保持,别为省几十 token 调大。
    3
}

pub(crate) fn default_timeout() -> u64 {
    60
}

pub(crate) fn default_vision_response_header_timeout() -> u64 {
    15
}

pub(crate) fn default_vision_stream_idle_timeout() -> u64 {
    20
}

pub(crate) fn default_vision_image_timeout() -> u64 {
    60
}

pub(crate) fn default_mcp_timeout() -> u64 {
    30
}

pub(crate) fn default_prompts_dir() -> String {
    "prompts".to_string()
}

pub(crate) fn default_identities_dir() -> String {
    "identities".to_string()
}

pub(crate) fn default_user_identity_file() -> String {
    "user-identity.md".to_string()
}

pub(crate) fn default_temperature() -> f32 {
    1.0
}

pub(crate) fn is_default_timeout(value: &u64) -> bool {
    *value == default_timeout()
}

pub(crate) fn is_default_temperature(value: &f32) -> bool {
    (*value - default_temperature()).abs() < f32::EPSILON
}

pub(crate) fn default_anthropic_max_tokens() -> u32 {
    4096
}

pub(crate) fn default_context_window() -> usize {
    168_000
}

pub(crate) fn is_default_anthropic_max_tokens(value: &u32) -> bool {
    *value == default_anthropic_max_tokens()
}

pub(crate) fn default_provider_protocol() -> String {
    "auto".to_string()
}

pub(crate) fn is_auto_protocol(value: &str) -> bool {
    value.trim().is_empty() || value == "auto"
}

pub(crate) fn default_true() -> bool {
    true
}

pub(crate) fn default_tools_loading_mode() -> String {
    // 默认 full(09-01 定稿):对 Miyu 这个 ~60 工具量级的目录,stub 的省 token
    // 优势本就薄(两模式缓存命中率一样,stub 只是常驻块更小,而这优势随 load
    // 的工具增多被尾部契约吃掉),且约束解码型模型(glm-5.3-flash)吃不下空壳。
    // full 更可靠(免 load 舞蹈/免"先调用后报错")、选工具准确度实测持平。想省
    // 的模型仍可按模型级 provider.model_tools_loading_mode 单独降回 stub。
    //
    // stub(v7 §八点七):byte-constant 工具数组 + 按需取契约。claude-code 桥、
    // tool-call 桥、mcp_serve 桥都直接读 registry 真 spec,不受本模式影响。
    // 旧 "hybrid" 档 09-01 删除,历史值回退 stub。
    "full".to_string()
}

pub(crate) fn default_subagent_concurrency() -> usize {
    4
}

pub(crate) fn default_tools_timeout_secs() -> u64 {
    180
}

pub(crate) fn default_command_deny() -> Vec<String> {
    [
        "rm -rf /",
        "rm -rf ~",
        "mkfs.",
        "dd if=/dev/zero of=/dev/",
        ":(){ :|:& };:",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

pub(crate) fn default_display_language() -> String {
    "auto".to_string()
}

pub(crate) fn default_reasoning_display() -> String {
    "summary".to_string()
}

pub(crate) fn default_tool_call_display() -> String {
    "summary".to_string()
}

pub(crate) fn default_command_output_lines() -> usize {
    10
}

pub(crate) fn default_repl_replay_turns() -> usize {
    3
}

pub(crate) fn default_mixed_model_endpoint_display() -> String {
    "interactive".to_string()
}

pub(crate) fn default_memory_association_facts() -> usize {
    2
}

pub(crate) fn default_memory_diary_batch_size() -> usize {
    14
}

pub(crate) fn default_memory_short_diary_retention_days() -> u64 {
    14
}

pub(crate) fn default_memory_diary_promotion_recalls() -> u64 {
    3
}

pub(crate) fn default_memory_organizer_timeout_seconds() -> u64 {
    120
}

pub(crate) fn default_memory_association_episodes() -> usize {
    1
}

pub(crate) fn default_memory_association_max_chars() -> usize {
    1800
}

pub(crate) fn default_memory_association_entry_chars() -> usize {
    120
}

pub(crate) fn default_tool_result_prune_chars() -> usize {
    8192
}

pub(crate) fn default_tool_result_prune_head_chars() -> usize {
    4096
}

pub(crate) fn default_tool_result_prune_tail_chars() -> usize {
    1024
}

pub(crate) fn default_memory_snippet_chars() -> usize {
    500
}

pub(crate) fn default_memory_forget_after_days() -> u64 {
    90
}

pub(crate) fn default_memory_half_life_days() -> f64 {
    7.0
}

pub(crate) fn default_memory_min_strength() -> f64 {
    0.15
}

pub(crate) fn default_memory_review_boost() -> f64 {
    0.35
}

pub(crate) fn default_memory_min_task_chars() -> usize {
    16
}

pub(crate) fn default_memory_min_method_chars() -> usize {
    120
}

pub(crate) fn default_print_image_width_percent() -> u8 {
    45
}

pub(crate) fn default_print_image_height_percent() -> u8 {
    35
}

pub(crate) fn default_memes_width_percent() -> u8 {
    35
}

pub(crate) fn default_memes_height_percent() -> u8 {
    25
}

pub(crate) fn default_memes_max_image_mb() -> u64 {
    10
}

pub(crate) fn default_memes_search_max_results() -> usize {
    1
}

pub(crate) fn default_memes_auto_send_probability() -> f32 {
    0.05
}

pub(crate) fn default_web_search_max_results() -> usize {
    4
}

pub(crate) fn default_web_images_max_results() -> usize {
    5
}

pub(crate) fn default_web_images_source_mode() -> String {
    "auto".to_string()
}

pub(crate) fn default_web_images_max_download_mb() -> f64 {
    4.0
}

pub(crate) fn default_web_images_preview_count() -> usize {
    1
}

pub(crate) fn default_web_images_timeout() -> u64 {
    20
}

pub(crate) fn default_deep_research_dir() -> String {
    default_miyu_home()
        .join("data/documents/deep-thinking")
        .display()
        .to_string()
}

pub(crate) fn default_deep_research_depth() -> String {
    "high".to_string()
}

pub(crate) fn default_deep_research_max_review_revisions() -> usize {
    0
}

pub(crate) fn default_deep_research_max_tool_steps() -> usize {
    0
}

pub(crate) fn default_deep_research_tool_timeout() -> u64 {
    90
}

pub(crate) fn default_subagent_max_tool_steps() -> usize {
    100
}

pub(crate) fn default_image_generation_provider_type() -> String {
    "openai".to_string()
}

pub(crate) fn default_openai_images_base_url() -> String {
    "https://api.openai.com".to_string()
}

pub(crate) fn default_image_generation_model() -> String {
    "gpt-image-1".to_string()
}

pub(crate) fn default_image_generation_aspect_ratio() -> String {
    "自动".to_string()
}

pub(crate) fn default_image_generation_resolution() -> String {
    "1K".to_string()
}

pub(crate) fn default_image_generation_output_dir() -> String {
    default_miyu_home()
        .join("data/pictures/generated-images")
        .display()
        .to_string()
}

pub(crate) fn default_miyu_home() -> PathBuf {
    std::env::var_os("MIYU_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".miyu")))
        .unwrap_or_else(|| PathBuf::from("~/.miyu"))
}

pub(crate) fn default_image_generation_timeout() -> u64 {
    180
}

pub(crate) fn default_kb_max_search_results() -> usize {
    5
}

pub(crate) fn default_kb_snippet_context_chars() -> usize {
    240
}

pub(crate) fn default_kb_proximity_window_chars() -> usize {
    512
}

pub(crate) fn default_kb_max_read_lines() -> usize {
    200
}

pub(crate) fn default_kb_max_file_size_kb() -> usize {
    1024
}

pub(crate) fn default_kb_allowed_extensions() -> String {
    ".txt,.md,.json,.jsonc,.json5,.yaml,.yml,.csv,.log,.py,.js,.ts,.jsx,.tsx,.mjs,.cjs,.html,.css,.scss,.sass,.less,.cfg,.ini,.conf,.toml,.kdl,.desktop,.service,.timer,.socket,.target,.mount,.rules,.network,.netdev,.properties,.hjson,.ron,.rst,.xml,.sh,.bash,.zsh,.fish,.nu,.ps1,.lua,.nix,.rasi,.yuck,.sql,.rs,.go,.c,.h,.cpp,.hpp,.java,.kt,.php,.rb,.pl,.org,.adoc,.tex".to_string()
}

pub(crate) fn default_kb_allowed_filenames() -> String {
    ".env,.env.local,.env.example,.env.sample,.envrc,.editorconfig,.gitignore,.gitattributes,.npmrc,.vimrc,.bashrc,.zshrc,.profile,.xinitrc,.xresources,config,dockerfile,containerfile,makefile,justfile,procfile,pkgbuild".to_string()
}

pub(crate) fn default_kb_semantic_chunk_chars() -> usize {
    512
}

pub(crate) fn default_kb_semantic_chunk_overlap() -> usize {
    80
}

pub(crate) fn default_kb_semantic_top_k() -> usize {
    5
}

pub(crate) fn default_kb_semantic_min_score() -> f32 {
    0.25
}

pub(crate) fn default_kb_keyword_strong_score_threshold() -> f32 {
    180.0
}

pub(crate) fn default_kb_embedding_timeout_seconds() -> u64 {
    60
}

pub(crate) fn default_diagnostics_timeout() -> u64 {
    5
}

pub(crate) fn default_diagnostics_max_stdout_chars() -> usize {
    8_000
}

pub(crate) fn default_diagnostics_max_stderr_chars() -> usize {
    4_000
}

pub(crate) fn default_calculator_backend() -> String {
    "rust-simple".to_string()
}

/// Compact trigger watermark. 0.8 (was 0.9) leaves room between the trigger
/// and the force watermark for the cheap mechanical layer to act first.
pub(crate) fn default_tool_output_spill_bytes() -> usize {
    50_000
}

pub(crate) fn default_trim_at_ratio() -> f32 {
    0.8
}

pub(crate) fn default_compact_force_ratio() -> f32 {
    0.9
}

pub(crate) fn default_compact_soft_ratio() -> f32 {
    0.5
}

pub(crate) fn default_compact_snip_ratio() -> f32 {
    0.6
}

pub(crate) fn default_cold_prune_after_minutes() -> u64 {
    1440
}

pub(crate) fn default_trim_batch_ratio() -> f32 {
    0.15
}

pub(crate) fn default_on_overflow() -> String {
    "compact".to_string()
}

pub(crate) fn default_claude_code_permission_mode() -> String {
    "bypassPermissions".to_string()
}

pub(crate) fn default_claude_code_native_tools() -> String {
    "all".to_string()
}

pub(crate) fn default_claude_code_miyu_tools() -> String {
    "all".to_string()
}

pub(crate) fn default_claude_code_idle_timeout_seconds() -> u64 {
    300
}

pub(crate) fn default_antigravity_native_tools() -> String {
    "all".to_string()
}

pub(crate) fn default_antigravity_miyu_tools() -> String {
    "all".to_string()
}

pub(crate) fn default_antigravity_idle_timeout_seconds() -> u64 {
    300
}

pub(crate) fn default_antigravity_print_timeout_seconds() -> u64 {
    24 * 60 * 60
}

pub(crate) fn bool_is_true(value: &bool) -> bool {
    *value
}
