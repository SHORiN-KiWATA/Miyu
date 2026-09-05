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
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        parsed["stdout"].as_str().unwrap(),
        "--json\n--limit=5\n--query=hello world"
    );
}
