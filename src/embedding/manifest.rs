//! 本地模型资产：目录布局与查找链。
//!
//! 一个模型就是一个目录（`manifest.json` + ONNX + tokenizer），目录名即模型 id。
//! 查找顺序照抄 `assets/fonts`：环境变量 → `~/.miyu/models` → 源码树 →
//! `/usr/share/miyu/models` → 可执行文件所在前缀。

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub(crate) const MODELS_DIR_ENV: &str = "MIYU_EMBEDDING_MODELS_DIR";
pub(crate) const DEFAULT_LOCAL_MODEL: &str = "bge-small-zh-v1.5-int8";
const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Pooling {
    Cls,
    Mean,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ModelManifest {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) display_name: String,
    #[serde(default = "default_model_file")]
    pub(crate) model_file: String,
    #[serde(default = "default_tokenizer_file")]
    pub(crate) tokenizer_file: String,
    pub(crate) dims: usize,
    #[serde(default = "default_pooling")]
    pub(crate) pooling: Pooling,
    #[serde(default = "default_true")]
    pub(crate) normalize: bool,
    #[serde(default = "default_max_length")]
    pub(crate) max_length: usize,
    #[serde(default)]
    pub(crate) query_prefix: String,
    #[serde(default = "default_min_score")]
    pub(crate) min_score: f32,
}

fn default_model_file() -> String {
    "model.onnx".to_string()
}
fn default_tokenizer_file() -> String {
    "tokenizer.json".to_string()
}
fn default_pooling() -> Pooling {
    Pooling::Cls
}
fn default_true() -> bool {
    true
}
fn default_max_length() -> usize {
    512
}
fn default_min_score() -> f32 {
    0.35
}

#[derive(Debug, Clone)]
pub(crate) struct LocalModel {
    pub(crate) dir: PathBuf,
    pub(crate) manifest: ModelManifest,
}

impl LocalModel {
    pub(crate) fn model_path(&self) -> PathBuf {
        self.dir.join(&self.manifest.model_file)
    }

    pub(crate) fn tokenizer_path(&self) -> PathBuf {
        self.dir.join(&self.manifest.tokenizer_file)
    }

    /// Stable identity for stored vectors: switching models invalidates them.
    pub(crate) fn model_id(&self) -> String {
        format!("local:{}", self.manifest.id)
    }
}

/// `name` is either a model id looked up along the search chain, or a path to
/// a model directory (anything containing a separator, or an existing dir).
pub(crate) fn resolve_local_model(name: &str) -> Result<LocalModel> {
    let name = name.trim();
    if name.is_empty() {
        bail!("embedding.local_model is empty");
    }
    let as_path = Path::new(name);
    if as_path.is_absolute() || name.contains('/') || name.contains('\\') {
        return load_model_dir(as_path);
    }
    let candidates = candidate_model_dirs();
    for base in &candidates {
        let dir = base.join(name);
        if dir.join(MANIFEST_FILE).is_file() {
            return load_model_dir(&dir);
        }
    }
    let searched = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "embedding model `{name}` was not found; install it under /usr/share/miyu/models or ~/.miyu/models, or set {MODELS_DIR_ENV} (searched: {searched})"
    )
}

pub(crate) fn candidate_model_dirs() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os(MODELS_DIR_ENV) {
        candidates.push(PathBuf::from(path));
    }
    if let Some(home) = crate::paths::miyu_home_dir() {
        candidates.push(home.join("models"));
    }
    #[cfg(debug_assertions)]
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/models"));
    candidates.push(PathBuf::from("/usr/share/miyu/models"));
    if let Ok(executable) = crate::paths::miyu_executable() {
        if let Some(prefix) = executable.parent().and_then(Path::parent) {
            candidates.push(prefix.join("share/miyu/models"));
        }
        if let Some(workspace) = executable
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
        {
            candidates.push(workspace.join("assets/models"));
        }
    }
    candidates
}

pub(crate) fn load_model_dir(dir: &Path) -> Result<LocalModel> {
    let manifest_path = dir.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest: ModelManifest = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    if manifest.dims == 0 {
        bail!("{}: dims must be positive", manifest_path.display());
    }
    if manifest.id.trim().is_empty() {
        bail!("{}: id is empty", manifest_path.display());
    }
    let model = LocalModel {
        dir: dir.to_path_buf(),
        manifest,
    };
    for path in [model.model_path(), model.tokenizer_path()] {
        if !path.is_file() {
            bail!("embedding model file is missing: {}", path.display());
        }
    }
    Ok(model)
}

/// Model ids available along the search chain, for pickers and `embed status`.
pub(crate) fn installed_local_models() -> Vec<LocalModel> {
    let mut seen = std::collections::BTreeSet::new();
    let mut models = Vec::new();
    for base in candidate_model_dirs() {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.join(MANIFEST_FILE).is_file() {
                continue;
            }
            if let Ok(model) = load_model_dir(&dir) {
                if seen.insert(model.manifest.id.clone()) {
                    models.push(model);
                }
            }
        }
    }
    models
}
