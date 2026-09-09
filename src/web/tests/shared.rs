//! 几个测试文件共用的构造辅助。

use crate::question::QuestionRequest;
use crate::question::{QuestionOption, QuestionPrompt};
use crate::web::*;

pub(super) fn test_paths(root: &FilePath) -> MiyuPaths {
    MiyuPaths {
        root_dir: root.to_path_buf(),
        config_dir: root.join("config"),
        config_file: root.join("config/config.jsonc"),
        skills_dir: root.join("config/skills"),
        data_dir: root.join("data"),
        cache_dir: root.join("cache"),
        state_dir: root.join("state"),
        pictures_dir: root.join("pictures"),
        fish_hook_file: root.join("fish"),
        bash_hook_file: root.join("bash"),
        zsh_hook_file: root.join("zsh"),
        scripts_dir: root.join("scripts"),
        system_scripts_dir: root.join("system-scripts"),
    }
}

pub(super) fn manager_with_run(
    run_id: &str,
) -> (Arc<Mutex<ManagerState>>, tokio::sync::watch::Receiver<bool>) {
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let manager = Arc::new(Mutex::new(ManagerState {
        config: AppConfig::default(),
        active_runs: HashMap::from([(
            run_id.to_string(),
            RunInfo {
                session_id: "default".into(),
                mode: AgentMode::Normal,
                audience: PromptAudience::Owner,
                cancel: cancel_tx,
                turn_id: None,
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup: None,
                operation: RunOperation::Create,
                job_wake: false,
                turn_origin: crate::tools::workspace::TurnOrigin::Human,
                job_wake_label: None,
            },
        )]),
        admin_busy: false,
        admin_session: None,
        context: ContextSnapshot {
            tokens: 0,
            window: None,
            window_assumed: false,
            cumulative_tokens: 0,
            cumulative_prompt_tokens: 0,
            cumulative_cache_read_tokens: 0,
        },
        persona_session_ids: HashMap::new(),
        runs_changed: Arc::new(tokio::sync::Notify::new()),
    }));
    (manager, cancel_rx)
}

pub(super) fn sample_question() -> QuestionRequest {
    QuestionRequest {
        questions: vec![QuestionPrompt {
            header: "Scope".to_string(),
            question: "Which scope?".to_string(),
            options: vec![QuestionOption {
                label: "All".to_string(),
                description: String::new(),
            }],
            multiple: false,
            custom: true,
        }],
    }
}

pub(super) fn test_daemon_with_actor(
    root: &FilePath,
) -> (DaemonState, std::thread::JoinHandle<Result<()>>) {
    DaemonState::for_test_with_actor(test_paths(root), 8300).unwrap()
}
