use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadPolicy {
    Summary,
    Group,
    Hidden,
}

impl Default for LoadPolicy {
    fn default() -> Self {
        Self::Summary
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolDescription {
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub parameters: Value,
    pub always_loaded: bool,
    #[serde(default)]
    pub load_policy: LoadPolicy,
    #[serde(default)]
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolGroupDescription {
    pub display_name: String,
    pub summary: String,
}

static TOOL_DESCRIPTIONS: OnceLock<HashMap<String, ToolDescription>> = OnceLock::new();
static TOOL_GROUPS: OnceLock<HashMap<String, ToolGroupDescription>> = OnceLock::new();
const TOOL_GROUPS_RAW: &str = include_str!("descriptions/groups.json");

macro_rules! tool_description_files {
    () => {
        [
            include_str!("descriptions/add_meme.json"),
            include_str!("descriptions/apply_patch.json"),
            include_str!("descriptions/archlinux_news.json"),
            include_str!("descriptions/archlinux_official_package_query.json"),
            include_str!("descriptions/archwiki_query.json"),
            include_str!("descriptions/aur_check_status.json"),
            include_str!("descriptions/aur_get_package_info.json"),
            include_str!("descriptions/aur_search_packages.json"),
            include_str!("descriptions/cancel_alarm.json"),
            include_str!("descriptions/calculate_hash.json"),
            include_str!("descriptions/check_issue.json"),
            include_str!("descriptions/check_os_info.json"),
            include_str!("descriptions/decode_encoded_text.json"),
            include_str!("descriptions/deep_research.json"),
            include_str!("descriptions/deep_research_linux_game_compatibility.json"),
            include_str!("descriptions/delete_meme.json"),
            include_str!("descriptions/draw_fortune_lot.json"),
            include_str!("descriptions/draw_tarot_card.json"),
            include_str!("descriptions/draw_zhouyi_hexagram.json"),
            include_str!("descriptions/edit_file.json"),
            include_str!("descriptions/edit_knowledge_base_file.json"),
            include_str!("descriptions/edit_string.json"),
            include_str!("descriptions/fcitx5_input_method_wiki_qurey.json"),
            include_str!("descriptions/generate_image.json"),
            include_str!("descriptions/get_exchange_rate.json"),
            include_str!("descriptions/get_weather.json"),
            include_str!("descriptions/glob.json"),
            include_str!("descriptions/grep.json"),
            include_str!("descriptions/install_aur_package.json"),
            include_str!("descriptions/linux_input_method_diagnose.json"),
            include_str!("descriptions/list_alarms.json"),
            include_str!("descriptions/load_skill.json"),
            include_str!("descriptions/online_man_get_page.json"),
            include_str!("descriptions/online_man_search.json"),
            include_str!("descriptions/print_image.json"),
            include_str!("descriptions/protondb_query.json"),
            include_str!("descriptions/query_caniplayonlinux.json"),
            include_str!("descriptions/query_deepseek_status.json"),
            include_str!("descriptions/query_moegirl.json"),
            include_str!("descriptions/read_clipboard.json"),
            include_str!("descriptions/read_file.json"),
            include_str!("descriptions/read_knowledge_base_file.json"),
            include_str!("descriptions/recall_memories.json"),
            include_str!("descriptions/recall_past_events.json"),
            include_str!("descriptions/register_deep_research_reference.json"),
            include_str!("descriptions/register_deep_research_topic_title.json"),
            include_str!("descriptions/register_script.json"),
            include_str!("descriptions/remove_deep_research_reference.json"),
            include_str!("descriptions/remove_knowledge_base_file.json"),
            include_str!("descriptions/remember_fact.json"),
            include_str!("descriptions/review_aur_package.json"),
            include_str!("descriptions/roll_dice.json"),
            include_str!("descriptions/run_command.json"),
            include_str!("descriptions/search_evicted_context.json"),
            include_str!("descriptions/search_knowledge_base.json"),
            include_str!("descriptions/search_knowledge_base_by_name.json"),
            include_str!("descriptions/search_meme.json"),
            include_str!("descriptions/search_web_images.json"),
            include_str!("descriptions/scientific_calculator.json"),
            include_str!("descriptions/set_alarm.json"),
            include_str!("descriptions/show_meme.json"),
            include_str!("descriptions/task.json"),
            include_str!("descriptions/todoupdate.json"),
            include_str!("descriptions/todowrite.json"),
            include_str!("descriptions/trash_path.json"),
            include_str!("descriptions/unregister_script.json"),
            include_str!("descriptions/update_meme.json"),
            include_str!("descriptions/upload_text_to_knowledge_base.json"),
            include_str!("descriptions/vision_analyze.json"),
            include_str!("descriptions/web_fetch.json"),
            include_str!("descriptions/web_search.json"),
            include_str!("descriptions/write_file.json"),
        ]
    };
}

pub fn all() -> &'static HashMap<String, ToolDescription> {
    TOOL_DESCRIPTIONS.get_or_init(|| {
        let mut map = HashMap::new();
        for raw in tool_description_files!() {
            let desc: ToolDescription =
                serde_json::from_str(raw).expect("built-in tool description JSON must be valid");
            map.insert(desc.name.clone(), desc);
        }
        map
    })
}

pub fn get(name: &str) -> Option<&'static ToolDescription> {
    all().get(name)
}

pub fn group_summary(group: &str) -> String {
    groups()
        .get(group)
        .map(|desc| desc.summary.clone())
        .unwrap_or_else(|| group.to_string())
}

pub fn group_display_name(group: &str) -> Option<String> {
    groups().get(group).map(|desc| desc.display_name.clone())
}

fn groups() -> &'static HashMap<String, ToolGroupDescription> {
    TOOL_GROUPS.get_or_init(|| {
        serde_json::from_str(TOOL_GROUPS_RAW).expect("tool group description JSON must be valid")
    })
}
