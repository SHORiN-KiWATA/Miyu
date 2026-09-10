//! 执行、输出截断、环境变量与 argv 展开。

use crate::tools::scripts::*;

fn executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn explicit_schema_defaults_to_lazy_loading() {
    let mut entry = ScriptEntry::overlay("search_game".to_string(), "search-game".to_string());
    entry.description = "Search game status".to_string();
    entry.parameters = json!({"type":"object","properties":{"query":{"type":"string"}}});
    let spec = entry_to_spec(&entry, Path::new("."), Path::new(".")).unwrap();
    assert!(!spec.always_loaded);
    assert!(spec.is_script);
}

#[test]
fn generic_scripts_default_to_lazy_loading_too() {
    let mut entry = ScriptEntry::overlay("plain".to_string(), "plain".to_string());
    entry.description = "Plain".to_string();
    let spec = entry_to_spec(&entry, Path::new("."), Path::new(".")).unwrap();
    assert!(!spec.always_loaded);
    assert_eq!(spec.parameters["properties"]["stdin"]["type"], "string");
}

#[cfg(unix)]
#[test]
fn make_executable_sets_x_bit() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("test.sh");
    std::fs::write(&script, "#!/bin/bash\necho hi").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::metadata(&script).unwrap().permissions();
    assert_eq!(perms.mode() & 0o111, 0);
    make_executable(&script).unwrap();
    let perms = std::fs::metadata(&script).unwrap().permissions();
    assert_ne!(perms.mode() & 0o111, 0);
}

/// 脚本跑起来时必须带上 `MIYU_SCRIPT_CACHE_DIR`,指向 Miyu 自己的缓存目录。
///
/// 中间产物(登录 profile、会话快照、查询票据、二维码图)不该散落在用户的
/// ~/.cache 下——`miyu wipe` 清 ~/.miyu 时应当一并带走。脚本单独在终端跑时
/// 这个变量不存在,退回 XDG 默认。
#[tokio::test]
async fn a_script_run_points_the_cache_at_miyu() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path().join("scripts");
    let cache_dir = temp.path().join("cache");
    std::fs::create_dir_all(&scripts_dir).unwrap();

    let script = scripts_dir.join("echo-cache");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s' \"$MIYU_SCRIPT_CACHE_DIR\"\n",
    )
    .unwrap();
    executable(&script);

    let out = super::super::run_script(
        "echo-cache",
        &scripts_dir,
        &cache_dir,
        &serde_json::json!({}),
        30,
        ArgvMode::Off,
        &crate::tools::ToolProgress::default(),
    )
    .await
    .unwrap();

    assert!(
        out.contains(cache_dir.to_str().unwrap()),
        "脚本没拿到 Miyu 的缓存目录：{out}"
    );
}

/// `MIYU_ARGS_JSON` 与 stdin 是同一份 JSON:脚本读环境变量就不用写读管道那段。
#[tokio::test]
async fn args_json_env_mirrors_stdin() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path().join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let script = scripts_dir.join("echo-env");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s|' \"$MIYU_ARGS_JSON\"\ncat\n",
    )
    .unwrap();
    executable(&script);

    let out = super::super::run_script(
        "echo-env",
        &scripts_dir,
        temp.path(),
        &json!({"query": "x"}),
        30,
        ArgvMode::Off,
        &crate::tools::ToolProgress::default(),
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        parsed["stdout"].as_str().unwrap(),
        r#"{"query":"x"}|{"query":"x"}"#
    );
}

#[test]
fn argv_flags_expansion_is_deterministic_and_skips_stdin() {
    let flags = super::super::argv_flags(&json!({
        "query": "hello world",
        "limit": -5,
        "json": true,
        "dry": false,
        "nothing": null,
        "tags": ["a", "b"],
        "stdin": "raw"
    }));
    assert_eq!(
        flags,
        vec![
            "--json",
            "--limit=-5",
            "--query=hello world",
            "--tags=[\"a\",\"b\"]",
        ]
    );
}

#[tokio::test]
async fn flags_mode_passes_arguments_on_argv() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path().join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let script = scripts_dir.join("echo-argv");
    std::fs::write(
        &script,
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
    )
    .unwrap();
    executable(&script);

    let out = super::super::run_script(
        "echo-argv",
        &scripts_dir,
        temp.path(),
        &json!({"query": "hello world", "limit": 5, "json": true, "dry": false}),
        30,
        ArgvMode::Flags,
        &crate::tools::ToolProgress::default(),
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        parsed["stdout"].as_str().unwrap(),
        "--json\n--limit=5\n--query=hello world"
    );
}

/// `MIYU-IMAGE: 路径 | 说明` 行从 stdout 里摘掉,图片交给投递层;相对路径按
/// 脚本缓存目录解析,不存在的文件只记警告不进投递。
#[test]
fn attachment_lines_are_split_out_of_stdout() {
    let cache = Path::new("/tmp/miyu-cache");
    let (kept, images) = split_attachment_lines(
        "hello\nMIYU-IMAGE: /tmp/a.png | 天气图\n  MIYU-IMAGE: rel/b.png\nMIYU-IMAGE:\nworld",
        cache,
    );
    assert_eq!(kept, "hello\nworld");
    assert_eq!(
        images,
        vec![
            (PathBuf::from("/tmp/a.png"), "天气图".to_string()),
            (cache.join("rel/b.png"), String::new()),
        ]
    );
}

/// 真跑一个脚本:它写一张图并报 MIYU-IMAGE,进度通道里应收到 Image 事件,
/// 返回体的 stdout 不再带那一行。
#[tokio::test]
async fn a_script_can_hand_back_an_image() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path().join("scripts");
    let cache_dir = temp.path().join("cache");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    std::fs::create_dir_all(&cache_dir).unwrap();
    let image = cache_dir.join("shot.png");
    std::fs::write(&image, b"png").unwrap();
    let script = scripts_dir.join("shooter");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho before\necho 'MIYU-IMAGE: {} | shot'\necho after\n",
            image.display()
        ),
    )
    .unwrap();
    executable(&script);
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let progress = crate::tools::ToolProgress::new(sender);
    let out = super::super::run_script(
        "shooter",
        &scripts_dir,
        &cache_dir,
        &serde_json::json!({}),
        30,
        ArgvMode::Off,
        &progress,
    )
    .await
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["stdout"], "before\nafter");
    match receiver.try_recv().unwrap() {
        crate::tools::ToolProgressEvent::Image { path, alt, .. } => {
            assert_eq!(path, image);
            assert_eq!(alt, "shot");
        }
        other => panic!("expected an image event, got {other:?}"),
    }
}
