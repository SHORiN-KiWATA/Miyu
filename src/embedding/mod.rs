//! 语义嵌入：给知识库、记忆联想、表情包、被淘汰上下文当**辅助**检索。
//!
//! 两种后端共用一个 [`Embedder`]：本地 ONNX 模型（跑在独立 worker 子进程里，
//! 默认启用、内置 `bge-small-zh-v1.5-int8`）或远程 OpenAI 兼容 `/embeddings`。
//! 任何一环不可用——没装 ONNX Runtime、模型目录缺失、远程超时——都只是少了
//! 语义分，关键词检索照常，功能不停摆。调用方拿到 `Err` 记一条 debug 日志就
//! 该继续走关键词路径，别把错误往上抛。
//!
//! 存储约定：向量以小端 f32 BLOB 落库，并带 `model_id` 标签；换模型后旧向量
//! 视为不存在，由各域按内容哈希增量补建。

mod local;
mod manifest;
mod remote;
#[cfg(test)]
mod tests;
mod vectors;
mod worker;

pub(crate) use manifest::{installed_local_models, resolve_local_model, LocalModel};
pub(crate) use vectors::{cosine, rrf_fuse, vector_from_blob, vector_to_blob, RRF_K};
pub(crate) use worker::{embedding_worker_requested, run_embedding_worker, shutdown_worker};

use crate::config::{AppConfig, EmbeddingBackend, ProviderConfig};
use anyhow::Result;
use std::time::Duration;

enum Backend {
    Local(LocalModel),
    Remote {
        provider: ProviderConfig,
        model: String,
        timeout: Duration,
    },
}

/// One configured embedding model. Cheap to build per call site: no I/O
/// beyond locating the model directory.
pub(crate) struct Embedder {
    backend: Backend,
    model_id: String,
    min_score: f32,
    query_prefix: String,
    idle_unload: Duration,
}

impl Embedder {
    /// `None` means "no semantic pass": disabled, or nothing usable is
    /// configured. The reason is logged at debug level so a missing model
    /// asset is diagnosable without turning the feature into an error.
    pub(crate) fn from_config(config: &AppConfig) -> Option<Self> {
        let embedding = &config.embedding;
        if !embedding.enabled {
            return None;
        }
        let idle_unload = Duration::from_secs(embedding.idle_unload_seconds.max(5));
        match embedding.resolved_backend() {
            EmbeddingBackend::Remote => {
                let provider = config
                    .providers
                    .iter()
                    .find(|provider| provider.id == embedding.provider_id.trim())?
                    .clone();
                let model = embedding.model.trim().to_string();
                if model.is_empty() {
                    return None;
                }
                Some(Self {
                    model_id: format!("{}/{model}", provider.id),
                    backend: Backend::Remote {
                        provider,
                        model,
                        timeout: Duration::from_secs(embedding.timeout_seconds.max(1)),
                    },
                    min_score: embedding.min_score,
                    query_prefix: String::new(),
                    idle_unload,
                })
            }
            _ => match resolve_local_model(&embedding.local_model) {
                Ok(model) => Some(Self {
                    model_id: model.model_id(),
                    min_score: model.manifest.min_score,
                    query_prefix: model.manifest.query_prefix.clone(),
                    backend: Backend::Local(model),
                    idle_unload,
                }),
                Err(error) => {
                    tracing::debug!(error = %error, "local embedding model unavailable");
                    None
                }
            },
        }
    }

    /// Stable identity of the vectors this embedder produces.
    pub(crate) fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Cosine below this is not a semantic hit on its own; fused rankings
    /// still let a keyword hit through regardless.
    pub(crate) fn min_score(&self) -> f32 {
        self.min_score
    }

    pub(crate) fn describe(&self) -> String {
        match &self.backend {
            Backend::Local(model) => format!(
                "local {} ({} dims, {})",
                model.manifest.id,
                model.manifest.dims,
                model.dir.display()
            ),
            Backend::Remote {
                provider, model, ..
            } => format!("remote {}/{model}", provider.id),
        }
    }

    pub(crate) fn is_local(&self) -> bool {
        matches!(self.backend, Backend::Local(_))
    }

    pub(crate) async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        match &self.backend {
            Backend::Local(model) => worker::embed_via_worker(model, self.idle_unload, texts).await,
            Backend::Remote {
                provider,
                model,
                timeout,
            } => remote::embed_remote(provider, model, *timeout, texts).await,
        }
    }

    /// Queries may carry an instruction prefix (model-specific); documents
    /// never do.
    pub(crate) async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let text = format!("{}{}", self.query_prefix, text);
        let mut vectors = self.embed(&[text]).await?;
        vectors
            .pop()
            .ok_or_else(|| anyhow::anyhow!("embedding backend returned no vector"))
    }
}

/// Where the ONNX Runtime library would be loaded from, for diagnostics.
pub(crate) fn runtime_library() -> Result<std::path::PathBuf> {
    worker::runtime_lib_or_hint()
}
