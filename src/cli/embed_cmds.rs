//! `miyu embed`：语义检索的诊断与维护。
//!
//! `status` 一眼看出为什么语义检索没生效（没启用/没模型/没运行库/worker 起不来），
//! `reindex` 把当前人格记忆、当前表情库、知识库缺的向量一次补齐。

use crate::config::{AppConfig, EmbeddingBackend};
use crate::embedding::{installed_local_models, runtime_library, shutdown_worker, Embedder};
use crate::memory::MemoryStore;
use crate::paths::MiyuPaths;
use crate::tools;
use anyhow::Result;
use clap::{Args, Subcommand};
use std::time::Instant;

#[derive(Debug, Args)]
pub struct EmbedArgs {
    #[command(subcommand)]
    pub command: EmbedCommand,
}

#[derive(Debug, Subcommand)]
pub enum EmbedCommand {
    /// Show the configured backend, runtime library and a live probe.
    Status,
    /// List local models found along the search chain.
    Models,
    /// Build missing vectors for memory, memes and the knowledge base.
    Reindex(EmbedReindexArgs),
}

#[derive(Debug, Args)]
pub struct EmbedReindexArgs {
    #[arg(long)]
    pub quiet: bool,
}

pub(in crate::cli) async fn run_embed(paths: &MiyuPaths, args: EmbedArgs) -> Result<()> {
    let config = AppConfig::load(paths)?;
    match args.command {
        EmbedCommand::Status => status(&config, paths).await,
        EmbedCommand::Models => {
            let models = installed_local_models();
            if models.is_empty() {
                println!("no local embedding models found");
            }
            for model in models {
                println!(
                    "{}  {} dims  {}",
                    model.manifest.id,
                    model.manifest.dims,
                    model.dir.display()
                );
            }
            Ok(())
        }
        EmbedCommand::Reindex(args) => reindex(&config, paths, args.quiet).await,
    }
}

async fn status(config: &AppConfig, paths: &MiyuPaths) -> Result<()> {
    let embedding = &config.embedding;
    let backend = match embedding.resolved_backend() {
        EmbeddingBackend::Local => "local",
        EmbeddingBackend::Remote => "remote",
        EmbeddingBackend::Auto => "auto",
    };
    println!(
        "enabled: {}  backend: {backend}  local_model: {}  remote: {}",
        embedding.enabled,
        embedding.local_model,
        if embedding.remote_is_configured() {
            format!("{}/{}", embedding.provider_id, embedding.model)
        } else {
            "-".to_string()
        }
    );
    let Some(embedder) = Embedder::from_config(config) else {
        println!(
            "semantic search: unavailable ({})",
            if !embedding.enabled {
                "disabled in config".to_string()
            } else {
                match crate::embedding::resolve_local_model(&embedding.local_model) {
                    Ok(_) => "remote provider/model not found".to_string(),
                    Err(error) => format!("{error:#}"),
                }
            }
        );
        return Ok(());
    };
    println!("model: {}", embedder.describe());
    if embedder.is_local() {
        match runtime_library() {
            Ok(path) => println!("runtime library: {}", path.display()),
            Err(error) => {
                println!("runtime library: missing\n  {error:#}");
                println!("semantic search: unavailable (keyword search still works)");
                return Ok(());
            }
        }
    }
    let started = Instant::now();
    match embedder.embed_query("你好，世界").await {
        Ok(vector) => {
            let cold = started.elapsed();
            let warm_started = Instant::now();
            let warm = embedder
                .embed_query("hello again")
                .await
                .map(|_| warm_started.elapsed());
            println!(
                "probe: {} dims, cold {} ms, warm {}",
                vector.len(),
                cold.as_millis(),
                warm.map(|d| format!("{:.1} ms", d.as_secs_f64() * 1000.0))
                    .unwrap_or_else(|e| format!("failed: {e:#}"))
            );
            println!("semantic search: available");
        }
        Err(error) => {
            println!("probe: failed\n  {error:#}");
            println!("semantic search: unavailable (keyword search still works)");
        }
    }
    let store = MemoryStore::new(config, paths);
    match store.embedding_coverage(embedder.model_id()) {
        Ok((done, missing)) => {
            println!("memory vectors (current persona): {done} current, {missing} missing")
        }
        Err(error) => println!("memory vectors: {error:#}"),
    }
    println!(
        "knowledge base: {}",
        if config.plugins.knowledge_base.embedding_enabled {
            "semantic enabled"
        } else {
            "semantic disabled (plugins.knowledge_base.embedding_enabled)"
        }
    );
    shutdown_worker().await;
    Ok(())
}

async fn reindex(config: &AppConfig, paths: &MiyuPaths, quiet: bool) -> Result<()> {
    let Some(embedder) = Embedder::from_config(config) else {
        println!("embedding is disabled or no model is available; nothing to do");
        return Ok(());
    };
    let store = MemoryStore::new(config, paths);
    let count = store.backfill_embeddings(embedder.model_id()).await?;
    if !quiet {
        println!("memory: embedded {count} rows");
    }
    let library = tools::memes::current_persona_library(config);
    match tools::memes::reindex_library(config, paths, &library).await {
        Ok(count) => {
            if !quiet {
                println!("memes ({library}): embedded {count} items");
            }
        }
        Err(error) => println!("memes ({library}): {error:#}"),
    }
    if config.plugins.knowledge_base.enabled && config.plugins.knowledge_base.embedding_enabled {
        let kb = tools::knowledge_base::KnowledgeBase::new(config.clone(), paths.clone())?;
        let count = kb.reindex_embeddings(quiet).await?;
        if !quiet {
            println!("knowledge base: embedded {count} chunks");
        }
    }
    shutdown_worker().await;
    Ok(())
}
