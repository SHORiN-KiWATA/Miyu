//! 命令行参数与子命令的解析。

// 被测的东西散在 cli::mod 与 repl 的兄弟模块里，这里全都要够到。
use super::shared::*;
use crate::cli::*;
/// REPL 的 `/models` 收一整串自由文本,`--global` / `-g` 要能从里面摘
/// 出来,并且不能把 `-g` 开头的模型名(如 `-gpt`)误当成开关。
#[test]
fn models_argument_parses_the_global_switch() {
    let plain = parse_models_argument("  gpt-5  ");
    assert!(!plain.global);
    assert_eq!(plain.target.as_deref(), Some("gpt-5"));

    let bare = parse_models_argument("--global");
    assert!(bare.global);
    assert!(bare.target.is_none());

    for input in ["-g gpt-5", "--global gpt-5", "-g --global gpt-5"] {
        let parsed = parse_models_argument(input);
        assert!(parsed.global, "{input}");
        assert_eq!(parsed.target.as_deref(), Some("gpt-5"), "{input}");
    }

    // `-gpt` 是模型名,不是开关。
    let lookalike = parse_models_argument("-gpt-image");
    assert!(!lookalike.global);
    assert_eq!(lookalike.target.as_deref(), Some("-gpt-image"));

    assert!(parse_models_argument("").target.is_none());
}

#[test]
fn variant_is_a_cli_subcommand_with_an_optional_name() {
    let cli = parse_args(["miyu", "variant"].map(OsString::from).to_vec()).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Variant(VariantArgs { name: None }))
    ));

    let cli = parse_args(["miyu", "variant", "high"].map(OsString::from).to_vec()).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Variant(VariantArgs { name })) if name.as_deref() == Some("high")
    ));

    assert!(parse_args(
        ["miyu", "variant", "high", "extra"]
            .map(OsString::from)
            .to_vec()
    )
    .is_err());
}

#[test]
fn continue_and_session_flags_are_mutually_exclusive() {
    let cli = parse_args(["miyu", "-c", "hello"].map(OsString::from).to_vec()).unwrap();
    assert!(cli.turn.continue_session);
    assert_eq!(cli.message, vec!["hello".to_string()]);

    let cli = parse_args(
        ["miyu", "--session", "2", "hello"]
            .map(OsString::from)
            .to_vec(),
    )
    .unwrap();
    assert!(!cli.turn.continue_session);
    assert_eq!(cli.turn.session.as_deref(), Some("2"));

    assert!(parse_args(
        ["miyu", "-c", "--session", "2", "hello"]
            .map(OsString::from)
            .to_vec()
    )
    .is_err());
}

#[test]
fn picker_keys_reach_delete_only_through_a_modifier() {
    use crossterm::event::{KeyCode, KeyModifiers};
    let plain = KeyModifiers::NONE;
    let control = KeyModifiers::CONTROL;

    // Every printable character is search input, so a bare `d` must never
    // be a shortcut — deletion needs Ctrl+D (or the Delete key).
    assert_eq!(
        inline_select_key(KeyCode::Char('d'), plain, true),
        InlineSelectKey::Char('d')
    );
    assert_eq!(
        inline_select_key(KeyCode::Char('d'), control, true),
        InlineSelectKey::DeleteRequest
    );
    assert_eq!(
        inline_select_key(KeyCode::Delete, plain, true),
        InlineSelectKey::DeleteRequest
    );

    // Pickers that did not opt in stay exactly as they were.
    assert_eq!(
        inline_select_key(KeyCode::Char('d'), control, false),
        InlineSelectKey::Ignore
    );
    assert_eq!(
        inline_select_key(KeyCode::Delete, plain, false),
        InlineSelectKey::Ignore
    );

    assert_eq!(
        inline_select_key(KeyCode::Char('c'), control, true),
        InlineSelectKey::Cancel
    );
    assert_eq!(
        inline_select_key(KeyCode::Esc, plain, true),
        InlineSelectKey::Cancel
    );
    assert_eq!(
        inline_select_key(KeyCode::Enter, plain, true),
        InlineSelectKey::Accept
    );
    assert_eq!(
        inline_select_key(KeyCode::Char('j'), plain, true),
        InlineSelectKey::Down
    );
    assert_eq!(
        inline_select_key(KeyCode::Char('k'), plain, true),
        InlineSelectKey::Up
    );
}

#[test]
fn web_is_a_cli_subcommand_with_local_server_options() {
    let cli = parse_args(
        ["miyu", "web", "--port", "4100"]
            .map(OsString::from)
            .to_vec(),
    )
    .unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Web(WebArgs {
            port: 4100,
            bind: None,
            port_explicit: true,
        }))
    ));

    for arg in ["stop", "status", "restart", "--status", "--stop"] {
        assert!(parse_args(["miyu", "web", arg].map(OsString::from).to_vec()).is_err());
    }

    let cli = parse_args(["miyu", "web"].map(OsString::from).to_vec()).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Web(WebArgs {
            port: 8300,
            bind: None,
            port_explicit: false,
        }))
    ));

    for args in [
        vec!["miyu", "web", "-p"],
        vec!["miyu", "web", "--password-file", "/tmp/x"],
    ] {
        assert!(parse_args(args.into_iter().map(OsString::from).collect()).is_err());
    }

    assert!(parse_args(["miyu", "web", "--public"].map(OsString::from).to_vec(),).is_err());
}

#[test]
fn bare_web_does_not_override_the_persisted_launch_config() {
    let temp = tempfile::tempdir().unwrap();
    let paths = pop_test_paths(temp.path());
    let args = WebArgs {
        port: ipc::DEFAULT_WEB_PORT,
        bind: None,
        port_explicit: false,
    };

    assert!(web_launch_config(&paths, &args).unwrap().is_none());
}

#[test]
fn daemon_owns_lifecycle_and_log_commands() {
    for (arg, expected) in [
        ("start", "start"),
        ("stop", "stop"),
        ("restart", "restart"),
        ("status", "status"),
    ] {
        let cli = parse_args(["miyu", "daemon", arg].map(OsString::from).to_vec()).unwrap();
        let actual = match cli.command {
            Some(Command::Daemon(DaemonArgs {
                command: Some(DaemonCommand::Start),
                ..
            })) => "start",
            Some(Command::Daemon(DaemonArgs {
                command: Some(DaemonCommand::Stop),
                ..
            })) => "stop",
            Some(Command::Daemon(DaemonArgs {
                command: Some(DaemonCommand::Restart),
                ..
            })) => "restart",
            Some(Command::Daemon(DaemonArgs {
                command: Some(DaemonCommand::Status),
                ..
            })) => "status",
            other => panic!("unexpected command: {other:?}"),
        };
        assert_eq!(actual, expected);
    }

    let cli = parse_args(["miyu", "daemon", "logs"].map(OsString::from).to_vec()).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Daemon(DaemonArgs {
            command: Some(DaemonCommand::Logs(DaemonLogsArgs { lines: None, .. })),
            ..
        }))
    ));

    let cli = parse_args(
        ["miyu", "daemon", "logs", "-n", "25"]
            .map(OsString::from)
            .to_vec(),
    )
    .unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Daemon(DaemonArgs {
            command: Some(DaemonCommand::Logs(DaemonLogsArgs {
                lines: Some(25),
                ..
            })),
            ..
        }))
    ));
}

#[test]
fn reload_is_a_top_level_command() {
    let cli = parse_args(["miyu", "reload"].map(OsString::from).to_vec()).unwrap();
    assert!(matches!(cli.command, Some(Command::Reload)));
    assert!(parse_args(["miyu", "reload", "extra"].map(OsString::from).to_vec()).is_err());
}

#[test]
fn daemon_accepts_a_port_and_defaults_to_start() {
    let cli = parse_args(
        ["miyu", "daemon", "--port", "9412"]
            .map(OsString::from)
            .to_vec(),
    )
    .unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Daemon(DaemonArgs {
            port: Some(9412),
            command: None,
        }))
    ));

    let cli = parse_args(
        ["miyu", "daemon", "--port", "9412", "restart"]
            .map(OsString::from)
            .to_vec(),
    )
    .unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Daemon(DaemonArgs {
            port: Some(9412),
            command: Some(DaemonCommand::Restart),
        }))
    ));

    let cli = parse_args(
        ["miyu", "daemon", "start", "--port", "9412"]
            .map(OsString::from)
            .to_vec(),
    )
    .unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Daemon(DaemonArgs {
            port: Some(9412),
            command: Some(DaemonCommand::Start),
        }))
    ));

    assert!(parse_args(
        ["miyu", "daemon", "--password"]
            .map(OsString::from)
            .to_vec(),
    )
    .is_err());
}

#[test]
fn daemon_web_urls_are_rendered_on_separate_aligned_lines() {
    let urls = vec![
        "http://127.0.0.1:8300".to_string(),
        "http://192.168.1.2:8300".to_string(),
    ];
    assert_eq!(
        daemon_web_status_lines("WebUI:", &urls),
        [
            "WebUI: http://127.0.0.1:8300",
            "       http://192.168.1.2:8300",
        ]
    );
    assert_eq!(
        daemon_web_status_lines("WebUI：", &urls),
        [
            "WebUI： http://127.0.0.1:8300",
            "        http://192.168.1.2:8300",
        ]
    );
}

#[test]
fn pop_is_a_cli_subcommand_with_an_optional_count() {
    let cli = parse_args(["miyu", "pop"].map(OsString::from).to_vec()).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Pop(PopArgs { count: None, .. }))
    ));

    let cli = parse_args(["miyu", "pop", "3"].map(OsString::from).to_vec()).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Pop(PopArgs { count: Some(3), .. }))
    ));
    assert!(parse_args(["miyu", "pop", "0"].map(OsString::from).to_vec()).is_err());
    assert!(parse_args(["miyu", "pop", "nope"].map(OsString::from).to_vec()).is_err());
}

#[test]
fn debug_is_a_global_cli_option() {
    for args in [
        &["miyu", "--debug", "models", "1"][..],
        &["miyu", "models", "--debug", "1"][..],
        &["miyu", "hello", "--debug"][..],
        &["miyu", "ask", "hello", "--debug"][..],
    ] {
        let cli = parse_args(args.iter().map(OsString::from).collect()).unwrap();
        assert!(cli.debug);
    }

    let cli = parse_args(["miyu", "hello", "--debug"].map(OsString::from).to_vec()).unwrap();
    assert_eq!(cli.message, ["hello"]);

    let cli = parse_args(["miyu", "--", "--debug"].map(OsString::from).to_vec()).unwrap();
    assert!(!cli.debug);
    assert_eq!(cli.message, ["--debug"]);
}

#[test]
fn session_selection_defaults_to_the_current_entry() {
    let entry = |id: &str, is_current: bool| SessionListEntry {
        id: id.to_string(),
        name: id.to_string(),
        is_current,
        turns: 0,
        snippet: String::new(),
        workspace: None,
        mode: "normal".to_string(),
    };
    let entries = vec![entry("default", true), entry("active", false)];

    assert_eq!(session_initial_selection(&entries, Some("active")), 1);
    assert_eq!(session_initial_selection(&entries, None), 0);
    assert!(matches!(
        session_ref_from_index(&entries, 2),
        Some(crate::ipc::SessionRef::Id { id }) if id == "active"
    ));
    assert_eq!(session_initial_selection(&[entry("only", false)], None), 0);
}
