//! 执行、输出截断与注册。

use crate::tools::scripts::*;

#[test]
fn explicit_schema_defaults_to_lazy_loading() {
    let entry = ScriptEntry {
        id: "search_game".to_string(),
        display_name: "Search game".to_string(),
        description: "Search game status".to_string(),
        path: "search-game".to_string(),
        parameters: json!({"type":"object","properties":{"query":{"type":"string"}}}),
        timeout_seconds: None,
        always_loaded: None,
        load_policy: LoadPolicy::Summary,
        groups: Vec::new(),
    };
    let spec = entry_to_spec(&entry, Path::new("."), Path::new(".")).unwrap();
    assert!(!spec.always_loaded);
    assert!(spec.is_script);
}

#[tokio::test]
async fn lifecycle_mutations_preserve_malformed_sibling_entries() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("existing.sh"),
        "#!/bin/bash\n# Description: Existing\n\necho existing",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("new.sh"),
        "#!/bin/bash\n# Description: New\n\necho new",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("index.json"),
        serde_json::to_string(&json!({
            "scripts": [
                "broken entry",
                {
                    "id": "existing_script",
                    "display_name": "Existing",
                    "description": "Existing",
                    "path": "existing.sh"
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    register_script_handler(json!({"id":"new_script","path":"new.sh"}), scripts_dir)
        .await
        .unwrap();
    unregister_script_handler(
        json!({"id":"existing_script","delete_file":false}),
        scripts_dir,
    )
    .await
    .unwrap();

    let index = read_script_index_value(&scripts_dir.join("index.json")).unwrap();
    let scripts = index.get("scripts").and_then(Value::as_array).unwrap();
    assert!(scripts.iter().any(|entry| entry == "broken entry"));
    assert!(scripts
        .iter()
        .any(|entry| raw_entry_field(entry, "id") == Some("new_script")));
    assert!(!scripts
        .iter()
        .any(|entry| raw_entry_field(entry, "id") == Some("existing_script")));
    let disabled = index.get("disabled").and_then(Value::as_array).unwrap();
    assert!(disabled
        .iter()
        .any(|entry| raw_entry_field(entry, "id") == Some("existing_script")));
}

#[tokio::test]
async fn unregister_keeps_file_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("hello.sh"),
        "#!/bin/bash\n# Description: Say hello\n\necho hello",
    )
    .unwrap();

    unregister_script_handler(json!({"id":"hello","delete_file":false}), scripts_dir)
        .await
        .unwrap();

    assert!(scripts_dir.join("hello.sh").is_file());
    let index = read_script_index_for_scan(&scripts_dir.join("index.json")).unwrap();
    assert_eq!(index.disabled.len(), 1);
    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert!(scan.entries.is_empty());
    assert!(scan.unregistered.is_empty());
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let out = super::super::run_script(
        "echo-cache",
        &scripts_dir,
        &cache_dir,
        &serde_json::json!({}),
        30,
    )
    .await
    .unwrap();

    assert!(
        out.contains(cache_dir.to_str().unwrap()),
        "脚本没拿到 Miyu 的缓存目录：{out}"
    );
}
