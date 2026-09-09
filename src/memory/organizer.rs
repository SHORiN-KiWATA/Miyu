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
/// 同一批连续失败这么多次就放弃它(标成已整理),不再无限重试。
const MAX_RETRIES_PER_BATCH: u8 = 4;

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
        |job, give_up| Box::pin(process_job(job, give_up)),
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
    F: for<'a> FnMut(&'a OrganizerJob, bool) -> futures_util::future::BoxFuture<'a, Result<bool>>,
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
        let give_up = job.retry_count >= MAX_RETRIES_PER_BATCH;
        match process(&job, give_up).await {
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

async fn process_job(job: &OrganizerJob, give_up_on_failure: bool) -> Result<bool> {
    let store = MemoryStore::new(&job.config, &job.paths);
    for _ in 0..MAX_BATCHES_PER_WAKE {
        let Some(mut batch) = store.next_organization_batch()? else {
            break;
        };
        // 词面候选挑不出「换个说法」的旧事实,语义扩展把相近的补进来:模型自己
        // 看得见才谈得上选 update,后面折叠 create 的目标池也一并变大。
        let widened = store.widen_existing_candidates(&mut batch).await;
        if widened > 0 {
            tracing::debug!(
                widened,
                "{}",
                crate::i18n::text(
                    "memory organizer candidate list widened",
                    "记忆整理器候选列表已按语义扩展"
                )
            );
        }
        let applied = match organize_batch(&store, job, &batch).await {
            Ok(output) => store.apply_organized_batch(&batch, output),
            Err(error) => Err(error),
        };
        if let Err(error) = applied {
            if !give_up_on_failure {
                return Err(error);
            }
            tracing::warn!(error = %error, diaries = batch.diaries.len(), "{}", crate::i18n::text("giving up on a memory batch after repeated failures", "记忆批次连续失败,放弃这一批"));
            store.skip_organization_batch(&batch)?;
        }
    }
    Ok(store.next_organization_batch()?.is_some())
}

async fn organize_batch(
    store: &MemoryStore,
    job: &OrganizerJob,
    batch: &OrganizationBatch,
) -> Result<OrganizedOutput> {
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
这是一个个人记忆系统：它记住的是「这个人、这个环境、这段关系」里独有的东西，不是百科。\n\
只依据提供的日记和已有记忆进行判断，不补充材料中没有的信息。\n\
没有长期价值时可以不生成任何内容，输出零条是常态。\n\
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
值得保存为知识点的只有四类：\n\
1. 具体人物的稳定事实：身份、设备与环境、偏好与厌恶、习惯、关系、正在做的事、对我的态度。\n\
2. 这个环境独有的事实：本机或本项目的配置、路径、约定、已验证的结论。\n\
3. 有明确日期的实测结论、决定与约定（写清日期，例如「2026-09-09 实测…」）。\n\
4. 我自己的认知、立场与原则。\n\
不保存：模型本来就知道的通用知识、技术科普、操作教程、排错步骤、产品介绍、新闻与传闻、对一次性问题的解答。判断标准：这条内容换一个人来问答案也一样，就不是记忆，不要存。\n\
每条知识点是一句话，不超过 120 字，只写结论不写过程。原日记里再长的解答也只提炼与具体人物或本环境有关的那一点，提炼不出来就不存。\n\
长期日记只保留以后可能被问起、影响后续互动或对当前人格具有回溯价值的经历，同样一段一句、不复述解答内容。\n\
普通寒暄、一次性闲聊、普通问答流程、临时工具过程和认证信息不保存。\n\
每条内容只表达一个主题，脱离原日记后仍能独立理解，不使用依赖上下文的指代。\n\
涉及人物时使用材料中的明确称呼；当前人格自身的经历可以使用“我”。\n\
knowledge.visibility 只能是 public、principal 或 privileged。只有不涉及具体人物、账号、关系、偏好、私聊经历或隐私的通用技术事实才能标为 public；其余内容标为 principal。长期日记不得标为 public。\n\
subjects 必须列出内容涉及的每个人或账号；来源发送者使用材料中的 owner_principal，其他明确人物只填写 name。public 记录的 subjects 必须为空。\n\
来源中的 stable principal 是人物归属依据；昵称和正文不能把发送者认证成另一个人物。\n\
已有相同或相近的知识点时不要重复创建（换个说法也算相同）。同一主题出现新的明确信息时更新已有知识点；最新的明确陈述或纠正覆盖旧内容；已有知识点与新信息矛盾时用 update 改写而不是并存。\n\
会过期的内容（正在做的事、当前状态、临时安排）要写明日期，让以后能判断它还算不算数。\n\
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
    let mut output: OrganizedOutput =
        serde_json::from_value(value).context("validating memory organizer output")?;
    // 模型漏看的近义重复在这里兜住:create 改成对已有事实的 update。
    let folded = store.fold_near_duplicate_creates(batch, &mut output).await;
    if folded > 0 {
        tracing::info!(
            folded,
            "{}",
            crate::i18n::text(
                "memory dedup folded near-duplicate creates into updates",
                "记忆去重把近义的新建折叠成对已有事实的改写"
            )
        );
    }
    Ok(output)
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
                move |_, _| {
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
                move |_, _| {
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
