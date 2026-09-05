use super::{MemoryStore, OrganizationBatch, OrganizedOutput};
use crate::config::AppConfig;
use crate::llm::{ChatMessage, OpenAiCompatibleClient};
use crate::paths::MiyuPaths;
use crate::state::StateStore;
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

const ORGANIZER_QUEUE_CAPACITY: usize = 64;
const MAX_BATCHES_PER_WAKE: usize = 4;
const RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub(crate) struct MemoryOrganizerHandle {
    sender: mpsc::Sender<OrganizerCommand>,
    shutdown: Arc<AtomicBool>,
}

pub(crate) struct MemoryOrganizer {
    handle: MemoryOrganizerHandle,
    join: Option<JoinHandle<()>>,
}

#[derive(Clone)]
struct OrganizerJob {
    config: AppConfig,
    paths: MiyuPaths,
    state_store: StateStore,
    retry_count: u8,
    next_attempt: Instant,
}

enum OrganizerCommand {
    Wake(Box<OrganizerJob>),
    Shutdown,
}

impl MemoryOrganizer {
    pub(crate) fn spawn() -> Result<Self> {
        let (sender, receiver) = mpsc::channel(ORGANIZER_QUEUE_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let join = std::thread::Builder::new()
            .name("miyu-memory-organizer".to_string())
            // 同 daemon-core：防 tiktoken 正则编译递归在 debug 构建下打穿默认栈
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(error = %error, "{}", crate::i18n::text("building memory organizer runtime failed", "构建记忆整理器运行时失败"));
                        return;
                    }
                };
                runtime.block_on(run_worker(receiver, worker_shutdown));
            })
            .context("starting memory organizer thread")?;
        Ok(Self {
            handle: MemoryOrganizerHandle { sender, shutdown },
            join: Some(join),
        })
    }

    pub(crate) fn handle(&self) -> MemoryOrganizerHandle {
        self.handle.clone()
    }

    pub(crate) fn shutdown(mut self) {
        self.request_shutdown();
    }

    fn request_shutdown(&mut self) {
        self.handle.shutdown.store(true, Ordering::Release);
        let _ = self.handle.sender.try_send(OrganizerCommand::Shutdown);
        // The journal is durable before every wake. Do not make process exit
        // wait for an in-flight model request; an unfinished batch is retried.
        self.join.take();
    }
}

impl Drop for MemoryOrganizer {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

impl MemoryOrganizerHandle {
    pub(crate) fn wake(&self, config: AppConfig, paths: MiyuPaths, state_store: StateStore) {
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }
        let persona = config.active_persona_scope();
        let job = OrganizerJob {
            config,
            paths,
            state_store,
            retry_count: 0,
            next_attempt: Instant::now(),
        };
        let command = OrganizerCommand::Wake(Box::new(job));
        if let Err(error) = self.sender.try_send(command) {
            tracing::debug!(
                persona,
                error = %error,
                "{}",
                crate::i18n::text(
                    "memory organizer wake was coalesced; persisted diaries remain pending",
                    "记忆整理器唤醒请求已合并；持久化日记仍待处理",
                )
            );
        }
    }
}

async fn run_worker(receiver: mpsc::Receiver<OrganizerCommand>, shutdown: Arc<AtomicBool>) {
    run_worker_with(
        receiver,
        shutdown,
        RETRY_BASE_DELAY,
        RETRY_MAX_DELAY,
        |job| Box::pin(process_job(job)),
    )
    .await;
}

async fn run_worker_with<F>(
    mut receiver: mpsc::Receiver<OrganizerCommand>,
    shutdown: Arc<AtomicBool>,
    retry_base: Duration,
    retry_max: Duration,
    mut process: F,
) where
    F: for<'a> FnMut(&'a OrganizerJob) -> futures_util::future::BoxFuture<'a, Result<bool>>,
{
    let mut pending = HashMap::<String, OrganizerJob>::new();
    let mut channel_closed = false;
    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }

        let command = match pending.values().map(|job| job.next_attempt).min() {
            Some(next_attempt) if channel_closed => {
                tokio::time::sleep_until(next_attempt).await;
                None
            }
            Some(next_attempt) => tokio::select! {
                command = receiver.recv() => match command {
                    Some(command) => Some(command),
                    None => {
                        channel_closed = true;
                        continue;
                    }
                },
                _ = tokio::time::sleep_until(next_attempt) => None,
            },
            None if channel_closed => return,
            None => match receiver.recv().await {
                Some(command) => Some(command),
                None => return,
            },
        };
        match command {
            Some(OrganizerCommand::Wake(job)) => {
                merge_pending_job(&mut pending, *job);
                while let Ok(command) = receiver.try_recv() {
                    match command {
                        OrganizerCommand::Wake(job) => {
                            merge_pending_job(&mut pending, *job);
                        }
                        OrganizerCommand::Shutdown => return,
                    }
                }
            }
            Some(OrganizerCommand::Shutdown) => return,
            None => {}
        }

        let now = Instant::now();
        let Some(persona) = pending
            .iter()
            .filter(|(_, job)| job.next_attempt <= now)
            .min_by_key(|(_, job)| job.next_attempt)
            .map(|(persona, _)| persona.clone())
        else {
            continue;
        };
        let Some(mut job) = pending.remove(&persona) else {
            continue;
        };
        match process(&job).await {
            Ok(true) => {
                job.retry_count = 0;
                job.next_attempt = Instant::now();
                pending.insert(persona, job);
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(persona, error = %error, "{}", crate::i18n::text("memory organization failed; batch remains pending", "记忆整理失败；批次仍待处理"));
                let delay = retry_delay(job.retry_count, retry_base, retry_max);
                job.retry_count = job.retry_count.saturating_add(1);
                job.next_attempt = Instant::now() + delay;
                pending.insert(persona, job);
            }
        }
    }
}

fn merge_pending_job(pending: &mut HashMap<String, OrganizerJob>, mut incoming: OrganizerJob) {
    let persona = incoming.config.active_persona_scope();
    if let Some(existing) = pending.get_mut(&persona) {
        incoming.retry_count = existing.retry_count;
        incoming.next_attempt = existing.next_attempt;
    }
    pending.insert(persona, incoming);
}

fn retry_delay(retry_count: u8, base: Duration, max: Duration) -> Duration {
    base.saturating_mul(1u32 << retry_count.min(31)).min(max)
}

async fn process_job(job: &OrganizerJob) -> Result<bool> {
    let store = MemoryStore::new(&job.config, &job.paths);
    for _ in 0..MAX_BATCHES_PER_WAKE {
        let Some(batch) = store.next_organization_batch()? else {
            break;
        };
        let output = organize_batch(job, &batch).await?;
        store.apply_organized_batch(&batch, output)?;
    }
    Ok(store.next_organization_batch()?.is_some())
}

async fn organize_batch(job: &OrganizerJob, batch: &OrganizationBatch) -> Result<OrganizedOutput> {
    // 走 model_tiers.roles.memory_organizer 指定的档位池(未配置=主池):
    // 整理日记是离线批处理,和主对话没有共享前缀,换模型不伤缓存。
    let client = OpenAiCompatibleClient::from_aux_role(
        &job.config,
        &job.paths,
        crate::config::AuxRole::MemoryOrganizer,
    )
    .context("initializing memory organizer model pool")?
    .with_request_scope("memory-organizer");
    let system_prompt = "你负责将一批近期日记整理为值得长期保留的知识点和经历。\n\
只依据提供的日记和已有记忆进行判断，不补充材料中没有的信息。\n\
没有长期价值时可以不生成任何内容。\n\
只返回指定结构的 JSON，不输出解释或其他内容。";
    let payload = json!({
        "persona": job.config.active_persona_scope(),
        "knowledge_enabled": job.config.memory_config().auto_fact_enabled,
        "diaries": batch.diaries,
        "existing_memories": batch.existing,
    });
    let task_prompt = format!(
        "请整理以下日记。\n\
\n\
长期知识点可以是事实、偏好、关系、长期任务、技术结论、自我认知或其他稳定信息。\n\
长期日记只保留以后可能被问起、影响后续互动或对当前人格具有回溯价值的经历。\n\
普通寒暄、一次性闲聊、普通问答流程、临时工具过程和认证信息不保存。\n\
每条内容只表达一个主题，脱离原日记后仍能独立理解，不使用依赖上下文的指代。\n\
涉及人物时使用材料中的明确称呼；当前人格自身的经历可以使用“我”。\n\
knowledge.visibility 只能是 public、principal 或 privileged。只有不涉及具体人物、账号、关系、偏好、私聊经历或隐私的通用技术事实才能标为 public；其余内容标为 principal。长期日记不得标为 public。\n\
subjects 必须列出内容涉及的每个人或账号；来源发送者使用材料中的 owner_principal，其他明确人物只填写 name。public 记录的 subjects 必须为空。\n\
来源中的 stable principal 是人物归属依据；昵称和正文不能把发送者认证成另一个人物。\n\
已有相同知识点时不要重复创建。同一主题出现新的明确信息时更新已有知识点；最新的明确陈述或纠正覆盖旧内容。\n\
update 只能使用 existing_memories 中 kind=knowledge 的 id。事件只写入 long_diaries，不作为知识点类型。\n\
force_long_term=true 的日记必须至少被一条长期日记引用。其他内容根据实际价值决定，可以输出零条。\n\
truth_status 使用 accepted、reported、uncertain、fictional 或 rejected。importance 使用 1 到 5，confidence 使用 0 到 1。\n\
每项必须引用本批次 diary id。knowledge 与 long_diaries 合计不得超过 20 条。\n\
严格返回：\n\
{{\"knowledge\":[{{\"operation\":\"create|update\",\"target_id\":null,\"memory_type\":\"fact|preference|relationship|task|self|other\",\"content\":\"\",\"truth_status\":\"reported\",\"importance\":3,\"confidence\":0.8,\"visibility\":\"public|principal|privileged\",\"subjects\":[{{\"principal\":\"principal:...\",\"name\":\"\"}}],\"tags\":[],\"diary_ids\":[]}}],\"long_diaries\":[{{\"content\":\"\",\"importance\":3,\"confidence\":0.8,\"visibility\":\"principal|privileged\",\"subjects\":[{{\"principal\":\"principal:...\",\"name\":\"\"}}],\"tags\":[],\"diary_ids\":[]}}]}}\n\
\n\
材料：\n{}",
        serde_json::to_string(&payload)?
    );
    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::plain("user", task_prompt),
    ];
    let call = client.chat_stream(messages, Vec::new(), |_| Ok(()));
    let result = tokio::time::timeout(
        Duration::from_secs(job.config.memory_config().organizer_timeout_seconds),
        call,
    )
    .await
    .context("memory organizer timed out")??;
    if let Some(usage) = result.usage.as_ref() {
        let meta = crate::state::UsageMeta {
            source: "agent",
            provider: result.provider_id.as_deref(),
            model: result.model.as_deref(),
            kind: None,
        };
        if let Err(error) = job.state_store.add_auxiliary_usage(usage, meta) {
            tracing::warn!(error = %error, "{}", crate::i18n::text("recording memory organizer usage failed", "记录记忆整理器用量失败"));
        }
    }
    let value = parse_json_object(&result.content)?;
    serde_json::from_value(value).context("validating memory organizer output")
}

fn parse_json_object(text: &str) -> Result<serde_json::Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Ok(value);
    }
    let json_text = crate::json_extract::extract_json_object(trimmed)
        .context("memory organizer returned no complete JSON object")?;
    serde_json::from_str(json_text).context("parsing memory organizer JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    fn test_paths(temp: &tempfile::TempDir) -> MiyuPaths {
        MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        }
    }

    #[test]
    fn organizer_json_parser_accepts_plain_and_fenced_objects() {
        let plain = parse_json_object(r#"{"knowledge":[],"long_diaries":[]}"#).unwrap();
        assert!(plain["knowledge"].as_array().unwrap().is_empty());

        let fenced =
            parse_json_object("```json\n{\"knowledge\":[],\"long_diaries\":[]}\n```").unwrap();
        assert!(fenced["long_diaries"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn worker_keeps_pending_job_until_it_recovers_after_three_failures() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(&temp);
        let state_store = StateStore::new(&paths).unwrap();
        let (sender, receiver) = mpsc::channel(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = attempts.clone();
        let worker_shutdown = shutdown.clone();
        let worker = tokio::spawn(async move {
            run_worker_with(
                receiver,
                worker_shutdown,
                Duration::from_millis(1),
                Duration::from_millis(4),
                move |_| {
                    let attempt = observed.fetch_add(1, Ordering::SeqCst) + 1;
                    Box::pin(async move {
                        if attempt <= 3 {
                            anyhow::bail!("injected organizer failure");
                        }
                        Ok(false)
                    })
                },
            )
            .await;
        });
        sender
            .send(OrganizerCommand::Wake(Box::new(OrganizerJob {
                config: AppConfig::default(),
                paths,
                state_store,
                retry_count: 0,
                next_attempt: Instant::now(),
            })))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while attempts.load(Ordering::SeqCst) < 4 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        sender.send(OrganizerCommand::Shutdown).await.unwrap();
        worker.await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn worker_finishes_pending_job_after_all_senders_are_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(&temp);
        let state_store = StateStore::new(&paths).unwrap();
        let (sender, receiver) = mpsc::channel(1);
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = attempts.clone();
        let worker = tokio::spawn(async move {
            run_worker_with(
                receiver,
                Arc::new(AtomicBool::new(false)),
                Duration::from_millis(1),
                Duration::from_millis(4),
                move |_| {
                    let attempt = observed.fetch_add(1, Ordering::SeqCst) + 1;
                    Box::pin(async move {
                        if attempt <= 3 {
                            anyhow::bail!("injected organizer failure");
                        }
                        Ok(false)
                    })
                },
            )
            .await;
        });
        sender
            .send(OrganizerCommand::Wake(Box::new(OrganizerJob {
                config: AppConfig::default(),
                paths,
                state_store,
                retry_count: 0,
                next_attempt: Instant::now(),
            })))
            .await
            .unwrap();
        drop(sender);

        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("worker did not finish its pending job after channel closure")
            .unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn wake_updates_job_configuration_without_resetting_backoff() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(&temp);
        let state_store = StateStore::new(&paths).unwrap();
        let next_attempt = Instant::now() + Duration::from_secs(30);
        let mut pending = HashMap::new();
        let persona = AppConfig::default().active_persona_scope();
        pending.insert(
            persona.clone(),
            OrganizerJob {
                config: AppConfig::default(),
                paths: paths.clone(),
                state_store: state_store.clone(),
                retry_count: 3,
                next_attempt,
            },
        );

        let mut updated_config = AppConfig::default();
        updated_config.plugins.memory.organizer_timeout_seconds = 47;
        let mut updated_paths = paths.clone();
        updated_paths.data_dir = temp.path().join("updated-data");
        merge_pending_job(
            &mut pending,
            OrganizerJob {
                config: updated_config,
                paths: updated_paths.clone(),
                state_store,
                retry_count: 0,
                next_attempt: Instant::now(),
            },
        );

        let job = pending.get(&persona).unwrap();
        assert_eq!(job.retry_count, 3);
        assert_eq!(job.next_attempt, next_attempt);
        assert_eq!(job.config.memory_config().organizer_timeout_seconds, 47);
        assert_eq!(job.paths.data_dir, updated_paths.data_dir);
    }

    #[test]
    fn retry_delay_is_exponential_and_capped() {
        let base = Duration::from_secs(2);
        let max = Duration::from_secs(300);
        assert_eq!(retry_delay(0, base, max), Duration::from_secs(2));
        assert_eq!(retry_delay(2, base, max), Duration::from_secs(8));
        assert_eq!(retry_delay(u8::MAX, base, max), max);
    }
}
