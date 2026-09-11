mod launch;
mod lifecycle;
mod protocol;
pub(crate) use launch::*;
pub(crate) use lifecycle::*;
pub(crate) use protocol::*;

use crate::paths::MiyuPaths;
use crate::question::QuestionAnswers;
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::{
    fs::File, fs::OpenOptions, os::fd::AsRawFd, os::unix::fs::OpenOptionsExt,
    os::unix::fs::PermissionsExt, os::unix::process::CommandExt, path::PathBuf, process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub const ADMIN_BUSY_MESSAGE: &str = "Miyu is busy with another operation";

/// Unique id of this build, stamped by build.rs. A daemon whose build id
/// differs from the client's is restarted transparently so a rebuild never
/// keeps serving stale code.
pub const BUILD_ID: &str = env!("MIYU_BUILD_ID");

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_with_logs(root: &std::path::Path) -> crate::paths::MiyuPaths {
        let paths = crate::paths::MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish/miyu.fish"),
            bash_hook_file: root.join("shell/bash-hook.sh"),
            zsh_hook_file: root.join("shell/zsh-hook.zsh"),
            scripts_dir: root.join("config/scripts"),
            system_scripts_dir: std::path::PathBuf::new(),
        };
        std::fs::create_dir_all(paths.logs_dir()).unwrap();
        paths
    }

    /// 启动失败时要给出**这次**写进 daemon.log 的内容。日志是所有启动共用
    /// 的追加文件:tail 固定行数会把上一次成功启动的 "Miyu WebUI: …" 当成这
    /// 次的输出,用户会以为起来了——比不给还糟(08-29 用户反馈现场)。
    #[test]
    fn the_daemon_log_excerpt_covers_only_this_launch() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_with_logs(temp.path());
        let log = paths.logs_dir().join("daemon.log");
        std::fs::write(&log, "Miyu WebUI: http://127.0.0.1:8300\n").unwrap();

        let offset = crate::ipc::lifecycle::daemon_log_len(&paths);
        // 一个字都没写就死了:不能把上一次的成功输出算进来。
        let quiet = crate::ipc::lifecycle::daemon_log_since(&paths, offset);
        assert!(!quiet.contains("Miyu WebUI"), "{quiet}");
        assert!(quiet.contains("没有留下任何输出"), "{quiet}");
        assert!(quiet.contains("daemon.log"), "{quiet}");

        std::fs::write(
            &log,
            "Miyu WebUI: http://127.0.0.1:8300\n错误: database disk image is malformed\n",
        )
        .unwrap();
        let excerpt = crate::ipc::lifecycle::daemon_log_since(&paths, offset);
        assert!(
            excerpt.contains("database disk image is malformed"),
            "{excerpt}"
        );
        assert!(!excerpt.contains("Miyu WebUI"), "{excerpt}");
    }

    #[test]
    fn request_protocol_is_explicitly_versioned() {
        let value = serde_json::to_value(Request::new(Command::Ping)).unwrap();
        assert_eq!(value["version"], PROTOCOL_VERSION);
        assert_eq!(value["command"], "ping");
    }

    #[test]
    fn ready_frame_exposes_daemon_web_state() {
        let value = serde_json::to_value(Frame::Ready {
            pid: 42,
            web_port: 4096,
            web_public: false,
            web_bind: None,
            build_id: "test-build".to_string(),
        })
        .unwrap();
        assert_eq!(value["type"], "ready");
        assert_eq!(value["pid"], 42);
        assert_eq!(value["web_port"], 4096);
        assert_eq!(value["web_public"].as_bool(), Some(false));
    }

    #[test]
    fn error_frames_keep_backward_compatibility_and_expose_codes() {
        let legacy: Frame = serde_json::from_value(serde_json::json!({
            "type": "error",
            "message": ADMIN_BUSY_MESSAGE,
        }))
        .unwrap();
        assert!(matches!(
            legacy,
            Frame::Error {
                code: None,
                message,
            } if message == ADMIN_BUSY_MESSAGE
        ));

        let coded =
            serde_json::to_value(Frame::coded_error(ErrorCode::Busy, ADMIN_BUSY_MESSAGE)).unwrap();
        assert_eq!(coded["type"], "error");
        assert_eq!(coded["code"], "busy");
        assert_eq!(coded["message"], ADMIN_BUSY_MESSAGE);

        let future: Frame = serde_json::from_value(serde_json::json!({
            "type": "error",
            "code": "future_error",
            "message": "future failure",
        }))
        .unwrap();
        assert!(matches!(
            future,
            Frame::Error {
                code: Some(ErrorCode::Unknown),
                ..
            }
        ));
    }

    #[test]
    fn daemon_process_prefers_the_default_web_port_unless_overridden() {
        let mut default = std::process::Command::new("miyu");
        append_daemon_process_args(&mut default, &DaemonLaunchConfig::default());
        let default_args = default
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(default_args, ["--port", "8300"]);

        let supplied = DaemonLaunchConfig {
            port: 9400,
            password_file: Some(PathBuf::from("/private/password")),
            bind: None,
        };
        let mut overridden = std::process::Command::new("miyu");
        append_daemon_process_args(&mut overridden, &supplied);
        let overridden_args = overridden
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        // 09-11 起口令不走命令行:旧 launch 状态里的 password_file 不再传给 daemon。
        assert_eq!(overridden_args, ["--port", "9400"]);
        assert!(overridden_args.iter().all(|arg| !arg.contains("secret")));
    }

    fn test_paths(root: &Path) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish/miyu.fish"),
            bash_hook_file: root.join("shell/bash-hook.sh"),
            zsh_hook_file: root.join("shell/zsh-hook.zsh"),
            scripts_dir: root.join("config/scripts"),
            system_scripts_dir: root.join("system/scripts"),
        }
    }

    #[test]
    fn daemon_launch_state_contains_only_port_and_password_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let password_path = stage_managed_web_password(&paths, "very-secret").unwrap();
        let config = DaemonLaunchConfig {
            port: 9400,
            password_file: Some(password_path.clone()),
            bind: None,
        };
        save_daemon_launch_config(&paths, &config).unwrap();

        assert_eq!(load_daemon_launch_config(&paths).unwrap(), config);
        let state = std::fs::read_to_string(paths.daemon_launch_state_file()).unwrap();
        assert!(!state.contains("very-secret"));
        assert_eq!(
            std::fs::metadata(password_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(paths.daemon_launch_state_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn directory_sync_failure_after_rename_keeps_the_commit_successful() {
        let error = std::io::Error::new(std::io::ErrorKind::Other, "injected sync failure");

        assert!(finish_private_state_commit(Path::new("/private/state"), Err(error)).is_ok());
    }

    #[test]
    fn bare_launch_restores_the_saved_port_and_password_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let password = stage_managed_web_password(&paths, "saved-secret").unwrap();
        let saved = DaemonLaunchConfig {
            port: 9412,
            password_file: Some(password.clone()),
            bind: None,
        };
        save_daemon_launch_config(&paths, &saved).unwrap();

        let restored = load_daemon_launch_config(&paths).unwrap();
        assert_eq!(restored, saved);
        let mut command = std::process::Command::new("miyu");
        append_daemon_process_args(&mut command, &restored);
        // password_file 只是旧字段:状态文件里存着,进程参数里不再出现。
        assert!(password.exists());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [std::ffi::OsStr::new("--port"), std::ffi::OsStr::new("9412"),]
        );
    }

    #[test]
    fn port_override_preserves_the_saved_password_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let password = stage_managed_web_password(&paths, "saved-secret").unwrap();
        let saved = DaemonLaunchConfig {
            port: 8300,
            password_file: Some(password.clone()),
            bind: None,
        };
        save_daemon_launch_config(&paths, &saved).unwrap();

        let overridden = daemon_launch_config_with_port(&paths, 9412).unwrap();

        assert_eq!(overridden.port, 9412);
        assert_eq!(overridden.password_file, Some(password));
    }

    #[test]
    fn persisted_launch_state_takes_precedence_over_proc_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let saved = DaemonLaunchConfig {
            port: 9412,
            password_file: None,
            bind: None,
        };
        save_daemon_launch_config(&paths, &saved).unwrap();

        assert!(recover_daemon_launch_if_missing(&paths, u32::MAX)
            .unwrap()
            .is_none());
        assert_eq!(load_daemon_launch_config(&paths).unwrap(), saved);
    }

    #[test]
    fn abandoned_password_candidate_does_not_replace_the_committed_password() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let old_password = stage_managed_web_password(&paths, "old-secret").unwrap();
        let old_launch = DaemonLaunchConfig {
            port: 8300,
            password_file: Some(old_password.clone()),
            bind: None,
        };
        commit_daemon_launch_config(&paths, &old_launch).unwrap();

        let candidate = stage_managed_web_password(&paths, "new-secret").unwrap();
        let failed_launch = DaemonLaunchConfig {
            port: 9400,
            password_file: Some(candidate.clone()),
            bind: None,
        };
        abandon_daemon_launch_candidate(&paths, &failed_launch);

        assert_eq!(load_daemon_launch_config(&paths).unwrap(), old_launch);
        assert_eq!(std::fs::read_to_string(old_password).unwrap(), "old-secret");
        assert!(!candidate.exists());
    }

    #[test]
    fn committing_a_new_password_cleans_the_previous_managed_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let old_password = stage_managed_web_password(&paths, "old-secret").unwrap();
        commit_daemon_launch_config(
            &paths,
            &DaemonLaunchConfig {
                port: 8300,
                password_file: Some(old_password.clone()),
                bind: None,
            },
        )
        .unwrap();
        let new_password = stage_managed_web_password(&paths, "new-secret").unwrap();
        let new_launch = DaemonLaunchConfig {
            port: 9400,
            password_file: Some(new_password.clone()),
            bind: None,
        };

        commit_daemon_launch_config(&paths, &new_launch).unwrap();

        assert!(!old_password.exists());
        assert_eq!(std::fs::read_to_string(new_password).unwrap(), "new-secret");
        assert_eq!(load_daemon_launch_config(&paths).unwrap(), new_launch);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_inline_password_and_port_are_recovered_into_managed_state() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let cmdline = b"/usr/bin/miyu\0__daemon\0--port\09412\0--password=legacy-secret\0";

        let recovered = recover_legacy_daemon_launch_from_cmdline(&paths, cmdline, None).unwrap();

        assert_eq!(recovered.port, 9412);
        let password = recovered.password_file.unwrap();
        let password_dir = paths.managed_web_password_dir();
        assert_eq!(password.parent(), Some(password_dir.as_path()));
        assert_eq!(std::fs::read_to_string(password).unwrap(), "legacy-secret");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_relative_password_file_is_copied_from_the_daemon_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        std::fs::write(temp.path().join("external-password"), "file-secret\n").unwrap();
        let cmdline = b"miyu\0__daemon\0--port=9500\0--password-file\0external-password\0";

        let recovered =
            recover_legacy_daemon_launch_from_cmdline(&paths, cmdline, Some(temp.path())).unwrap();

        assert_eq!(recovered.port, 9500);
        let password = recovered.password_file.unwrap();
        assert_ne!(password, temp.path().join("external-password"));
        assert_eq!(std::fs::read_to_string(password).unwrap(), "file-secret");
    }

    #[test]
    fn admin_commands_round_trip_with_explicit_state() {
        let request = Request::new(Command::ResetConversation {
            target: SessionRef::Id {
                id: "sess_local".to_string(),
            },
        });
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["command"], "reset_conversation");
        assert_eq!(value["target"]["kind"], "id");
        assert_eq!(value["target"]["id"], "sess_local");
        assert_eq!(PROTOCOL_VERSION, 3);

        let frame = Frame::AdminResult {
            state: SessionState {
                context_tokens: 12,
                context_window: Some(1000),
                context_window_assumed: false,
                cumulative_tokens: 34,
                cumulative_prompt_tokens: 20,
                cumulative_cache_read_tokens: 10,
                session_id: "default".to_string(),
                session_name: "终端集成会话".to_string(),
                workspace: None,
            },
            data: serde_json::json!({"ok": true}),
        };
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(value["type"], "admin_result");
        assert_eq!(value["state"]["cumulative_tokens"], 34);
    }

    /// 老客户端的 `reset_memory` 帧里没有 `scope` 字段，而它当年的语义是
    /// 全清。默认值必须落在 `All` 上——落在 `Session` 上就是升级 daemon 之后
    /// 老命令悄悄改了行为。
    #[test]
    fn a_reset_memory_frame_without_a_scope_still_means_wipe_everything() {
        let command: Command =
            serde_json::from_value(serde_json::json!({ "command": "reset_memory", "mode": null }))
                .unwrap();
        let Command::ResetMemory { scope, session, .. } = command else {
            panic!("expected a reset_memory command");
        };
        assert_eq!(scope, MemoryResetScope::All);
        assert!(session.is_none());

        let value = serde_json::to_value(Request::new(Command::ResetMemory {
            mode: None,
            scope: MemoryResetScope::Session,
            session: Some(SessionRef::Id {
                id: "sess_local".to_string(),
            }),
        }))
        .unwrap();
        assert_eq!(value["scope"], "session");
        assert_eq!(value["session"]["id"], "sess_local");
    }

    #[test]
    fn parses_protocol_version_from_daemon_rejection() {
        assert_eq!(
            expected_protocol_version("unsupported IPC protocol version 3; expected 2"),
            Some(2)
        );
        assert_eq!(expected_protocol_version("unrelated error"), None);
    }

    #[tokio::test]
    async fn framed_protocol_round_trips_over_unix_socket() {
        let (mut left, mut right) = UnixStream::pair().unwrap();
        let request = Request::new(Command::StartTurn {
            content: "hello".to_string(),
            mode: "normal".to_string(),
            images: vec![Some(ImageAttachment::Binary {
                mime: "image/png".to_string(),
                data: vec![1, 2, 3],
            })],
            cwd: Some(std::path::PathBuf::from("/tmp/workdir")),
            session_id: Some("sess_test".to_string()),
            origin_tty: Some(OriginTty {
                path: std::path::PathBuf::from("/dev/pts/7"),
                shell_pid: 4321,
            }),
            overrides: None,
        });
        let writer = tokio::spawn(async move { send(&mut left, &request).await });
        let received = receive::<Request>(&mut right).await.unwrap().unwrap();
        writer.await.unwrap().unwrap();

        assert_eq!(received.version, PROTOCOL_VERSION);
        match received.command {
            Command::StartTurn {
                content,
                mode,
                images,
                cwd,
                session_id,
                origin_tty,
                overrides,
            } => {
                assert_eq!(content, "hello");
                assert_eq!(mode, "normal");
                assert_eq!(images.len(), 1);
                assert_eq!(cwd, Some(std::path::PathBuf::from("/tmp/workdir")));
                assert_eq!(session_id.as_deref(), Some("sess_test"));
                assert!(overrides.is_none());
                let origin = origin_tty.expect("origin tty should round-trip");
                assert_eq!(origin.path, std::path::PathBuf::from("/dev/pts/7"));
                assert_eq!(origin.shell_pid, 4321);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_writing() {
        let (mut left, _right) = UnixStream::pair().unwrap();
        let request = Request::new(Command::StartTurn {
            content: "x".repeat(MAX_FRAME_BYTES),
            mode: "normal".to_string(),
            images: Vec::new(),
            cwd: None,
            session_id: None,
            origin_tty: None,
            overrides: None,
        });
        assert!(send(&mut left, &request).await.is_err());
    }

    #[test]
    fn direct_core_lease_is_exclusive() {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join("core.lock");
        let first = acquire_direct_core_at(lock.clone()).unwrap();
        assert!(acquire_direct_core_at(lock.clone()).is_err());
        drop(first);
        assert!(acquire_direct_core_at(lock).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn daemon_identity_uses_linux_process_start_time() {
        let identity = daemon_process_identity(std::process::id());
        assert!(identity.start_time.is_some());
        assert!(daemon_process_matches(identity));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn daemon_exit_wait_tracks_the_process_instead_of_ipc_files() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 0.1"])
            .spawn()
            .unwrap();
        let identity = daemon_process_identity(child.id());
        assert!(daemon_process_matches(identity));

        wait_for_daemon_exit(identity, Duration::from_secs(2))
            .await
            .unwrap();
        child.wait().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn daemon_exit_wait_times_out_while_the_same_process_is_alive() {
        let identity = daemon_process_identity(std::process::id());
        let error = wait_for_daemon_exit(identity, Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&std::process::id().to_string()));
    }
}
