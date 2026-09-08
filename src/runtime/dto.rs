//! 跨层传输的纯数据结构。
// 兄弟模块的类型互相引用（DaemonState 持有 EventHub、run 记录引用
// ManagerState 等），统一从 mod.rs 的再导出取，免得每个文件维护一份
// 交叉导入清单。
use crate::ipc::ImageAttachment;
use serde::{Deserialize, Serialize};

// ── ThinkingVariantUpdate ──
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThinkingVariantUpdate {
    pub(crate) provider_id: String,
    pub(crate) model: String,
    pub(crate) selected: Option<String>,
}

// ── PromptDocuments ──
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromptDocuments {
    #[serde(default)]
    pub(crate) personas: Vec<PromptDocument>,
    #[serde(default)]
    pub(crate) identities: Vec<PromptDocument>,
}

// ── SafeQueuedPrompt ──
#[derive(Clone, Serialize)]
pub(crate) struct SafeQueuedPrompt {
    pub(crate) id: String,
    pub(crate) content: String,
    pub(crate) submitted_at: String,
    pub(crate) attachments: Vec<SafeUserAttachment>,
}

// ── RedoWebPrompt ──
pub(crate) struct RedoWebPrompt {
    pub(crate) prompt_id: String,
    pub(crate) content: String,
    pub(crate) display_content: String,
    pub(crate) images: Vec<Option<ImageAttachment>>,
}

// ── PromptDocument ──
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromptDocument {
    pub(crate) name: String,
    pub(crate) content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) avatar_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) board_image_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) board_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) board_subtitle: Option<String>,
    /// WebUI 输入框为空时的提示。留空则按人格名生成「给 <名字> 发消息」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) composer_placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) starter_prompts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) original_name: Option<String>,
}

// ── SafeUserAttachment ──
#[derive(Clone, Serialize)]
pub(crate) struct SafeUserAttachment {
    pub(crate) id: String,
    pub(crate) url: String,
    pub(crate) name: String,
    pub(crate) mime: String,
    pub(crate) kind: String,
    pub(crate) size: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
}
