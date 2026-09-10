//! manage_script 三个动作与目录刷新。

use super::{test_env, write_script};
use crate::tools::scripts::*;

fn parse(out: &str) -> Value {
    serde_json::from_str(out).unwrap()
}

fn scan_roots(config: &crate::config::AppConfig, paths: &MiyuPaths) -> ScriptScanResult {
    let roots = script_scan_roots(config, paths);
    let dirs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    scan_scripts(&dirs).unwrap()
}

const LOOKUP: &str = "#!/usr/bin/env python3\n\
# -*- coding: utf-8 -*-\n\
# Description: Look things up\n\
# Parameters: {\"type\":\"object\",\"properties\":{\"q\":{\"type\":\"string\"}}}\n\
print(1)\n";

/// 痛点 1 的正面路径:AI 把脚本写在工作区里,register 直接给绝对路径,文件被
/// 复制进当前人格目录,index 只记 id/path,描述与 schema 来自头部。
#[tokio::test]
async fn register_copies_external_script_into_persona_layer() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    let source = write_script(&temp.path().join("work"), "lookup.py", LOOKUP);

    let out = register_script_handler(
        json!({"action": "register", "path": source.display().to_string()}),
        &config,
        &paths,
    )
    .await
    .unwrap();
    let out = parse(&out);
    assert_eq!(out["ok"], true);
    assert_eq!(out["id"], "lookup");
    assert_eq!(out["scope"], "persona");
    assert_eq!(out["copied"], true);
    assert_eq!(out["parameters_source"], "header");
    assert_eq!(out["description_source"], "header");

    let persona_dir = config.active_persona_scripts_dir(&paths);
    let copied = persona_dir.join("lookup.py");
    assert!(copied.is_file());
    assert!(source.is_file(), "复制不是搬走");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            std::fs::metadata(&copied).unwrap().permissions().mode() & 0o111,
            0
        );
    }
    let index = read_script_index_for_scan(&persona_dir.join("index.json")).unwrap();
    assert_eq!(index.scripts.len(), 1);
    assert_eq!(index.scripts[0].path, "lookup.py");
    assert!(
        index.scripts[0].description.is_empty(),
        "index 只存覆盖,描述留给头部"
    );
    assert!(index.scripts[0].parameters.is_null());

    let scan = scan_roots(&config, &paths);
    let entry = scan.entries.iter().find(|e| e.id == "lookup").unwrap();
    assert_eq!(entry.description, "Look things up");
    assert_eq!(entry.parameters["properties"]["q"]["type"], "string");
}

#[tokio::test]
async fn register_in_place_when_file_already_lives_in_a_user_layer() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    let script = write_script(
        &paths.scripts_dir,
        "tool.sh",
        "#!/bin/sh\n# Description: T\necho\n",
    );

    let out = parse(
        &register_script_handler(
            json!({"action": "register", "path": script.display().to_string()}),
            &config,
            &paths,
        )
        .await
        .unwrap(),
    );
    assert_eq!(out["scope"], "global");
    assert_eq!(out["copied"], false);
    assert!(!config
        .active_persona_scripts_dir(&paths)
        .join("tool.sh")
        .exists());

    // 相对文件名、没给 scope:persona 层没有就退到 global 层找。
    let out = parse(
        &register_script_handler(
            json!({"action": "register", "path": "tool.sh", "timeout_seconds": 30}),
            &config,
            &paths,
        )
        .await
        .unwrap(),
    );
    assert_eq!(out["scope"], "global");
    let index = read_script_index_for_scan(&paths.scripts_dir.join("index.json")).unwrap();
    assert_eq!(index.scripts[0].timeout_seconds, Some(30));
}

#[tokio::test]
async fn register_rejects_scripts_without_shebang_or_description() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    let no_shebang = write_script(&temp.path().join("w"), "plain.sh", "echo hi\n");
    let error = register_script_handler(
        json!({"action": "register", "path": no_shebang.display().to_string()}),
        &config,
        &paths,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("shebang"), "{error}");

    let no_description = write_script(&temp.path().join("w"), "quiet.sh", "#!/bin/sh\necho hi\n");
    let error = register_script_handler(
        json!({"action": "register", "path": no_description.display().to_string()}),
        &config,
        &paths,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("description is required"),
        "{error}"
    );
    assert!(error.to_string().contains("script-creator"), "{error}");
}

#[tokio::test]
async fn register_derives_id_from_file_name_and_validates_schema() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    let script = write_script(
        &temp.path().join("w"),
        "battery-care.sh",
        "#!/bin/sh\n# Description: Battery\necho\n",
    );
    let out = parse(
        &register_script_handler(
            json!({"action": "register", "path": script.display().to_string()}),
            &config,
            &paths,
        )
        .await
        .unwrap(),
    );
    assert_eq!(out["id"], "battery_care");

    let error = register_script_handler(
        json!({
            "action": "register",
            "id": "battery_care",
            "parameters": {"type": "array"}
        }),
        &config,
        &paths,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("parameters.type"), "{error}");
}

/// register 是合并更新:第二次只给超时,第一次给的描述不丢。
#[tokio::test]
async fn register_merges_updates_instead_of_replacing_the_entry() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    let script = write_script(&temp.path().join("w"), "tool.sh", "#!/bin/sh\necho\n");
    register_script_handler(
        json!({"action": "register", "path": script.display().to_string(), "description": "First"}),
        &config,
        &paths,
    )
    .await
    .unwrap();
    register_script_handler(
        json!({"action": "register", "id": "tool", "timeout_seconds": 30}),
        &config,
        &paths,
    )
    .await
    .unwrap();

    let persona_dir = config.active_persona_scripts_dir(&paths);
    let index = read_script_index_for_scan(&persona_dir.join("index.json")).unwrap();
    assert_eq!(index.scripts.len(), 1);
    assert_eq!(index.scripts[0].description, "First");
    assert_eq!(index.scripts[0].timeout_seconds, Some(30));
}

#[tokio::test]
async fn register_rejects_reserved_tool_names_before_writing_index() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    write_script(
        &paths.scripts_dir,
        "weather.sh",
        "#!/bin/bash\n# Description: Fake weather\n\necho fake",
    );

    let error = register_script_handler(
        json!({"action": "register", "id": "run_command", "path": "weather.sh"}),
        &config,
        &paths,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("reserved tool name"));
    assert!(!paths.scripts_dir.join("index.json").exists());
}

#[tokio::test]
async fn register_refuses_to_overwrite_unless_asked() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    let persona_dir = config.active_persona_scripts_dir(&paths);
    write_script(
        &persona_dir,
        "tool.sh",
        "#!/bin/sh\n# Description: Old\necho old\n",
    );
    let source = write_script(
        &temp.path().join("w"),
        "tool.sh",
        "#!/bin/sh\n# Description: New\necho new\n",
    );

    let error = register_script_handler(
        json!({"action": "register", "path": source.display().to_string()}),
        &config,
        &paths,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("overwrite=true"), "{error}");

    register_script_handler(
        json!({"action": "register", "path": source.display().to_string(), "overwrite": true}),
        &config,
        &paths,
    )
    .await
    .unwrap();
    assert!(std::fs::read_to_string(persona_dir.join("tool.sh"))
        .unwrap()
        .contains("New"));
}

#[tokio::test]
async fn unregister_disables_auto_detected_script_and_can_delete_it() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    write_script(
        &paths.scripts_dir,
        "hello.sh",
        "#!/bin/bash\n# Description: Say hello\n\necho hello",
    );

    let out = parse(
        &unregister_script_handler(
            json!({"action": "unregister", "id": "hello"}),
            &config,
            &paths,
        )
        .await
        .unwrap(),
    );
    assert_eq!(out["scope"], "global");
    assert_eq!(out["state"], "disabled");
    assert!(paths.scripts_dir.join("hello.sh").is_file());
    let index = read_script_index_for_scan(&paths.scripts_dir.join("index.json")).unwrap();
    assert_eq!(index.disabled.len(), 1);
    let scan = scan_roots(&config, &paths);
    assert!(scan.entries.is_empty());
    assert!(scan.unregistered.is_empty());

    // 重新注册会清掉 disabled 记录。
    register_script_handler(
        json!({"action": "register", "path": "hello.sh"}),
        &config,
        &paths,
    )
    .await
    .unwrap();
    let index = read_script_index_for_scan(&paths.scripts_dir.join("index.json")).unwrap();
    assert!(index.disabled.is_empty());
    assert_eq!(scan_roots(&config, &paths).entries.len(), 1);

    let out = parse(
        &unregister_script_handler(
            json!({"action": "unregister", "id": "hello", "delete_file": true}),
            &config,
            &paths,
        )
        .await
        .unwrap(),
    );
    assert_eq!(out["state"], "deleted");
    assert!(!paths.scripts_dir.join("hello.sh").exists());
}

/// 内置脚本只能按 id 屏蔽,记在 persona 层,文件不动。
#[tokio::test]
async fn unregister_hides_builtin_script_by_id_in_persona_layer() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    let builtin_dir = config.active_persona_system_scripts_dir(&paths);
    write_script(
        &builtin_dir,
        "bat.sh",
        "#!/bin/sh\n# Description: Battery\necho\n",
    );
    assert_eq!(scan_roots(&config, &paths).entries.len(), 1);

    let error = unregister_script_handler(
        json!({"action": "unregister", "id": "bat", "delete_file": true}),
        &config,
        &paths,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("built-in"), "{error}");

    let out = parse(
        &unregister_script_handler(
            json!({"action": "unregister", "id": "bat"}),
            &config,
            &paths,
        )
        .await
        .unwrap(),
    );
    assert_eq!(out["scope"], "persona");
    assert!(builtin_dir.join("bat.sh").is_file());
    let persona_index =
        read_script_index_for_scan(&config.active_persona_scripts_dir(&paths).join("index.json"))
            .unwrap();
    assert_eq!(persona_index.disabled[0].id, "bat");
    assert!(scan_roots(&config, &paths).entries.is_empty());

    let error = unregister_script_handler(
        json!({"action": "unregister", "id": "nope"}),
        &config,
        &paths,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("action=list"), "{error}");
}

#[tokio::test]
async fn lifecycle_mutations_preserve_malformed_sibling_entries() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    let scripts_dir = paths.scripts_dir.clone();
    write_script(
        &scripts_dir,
        "existing.sh",
        "#!/bin/bash\n# Description: Existing\n\necho existing",
    );
    write_script(
        &scripts_dir,
        "new.sh",
        "#!/bin/bash\n# Description: New\n\necho new",
    );
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

    register_script_handler(
        json!({"action": "register", "id": "new_script", "path": "new.sh"}),
        &config,
        &paths,
    )
    .await
    .unwrap();
    unregister_script_handler(
        json!({"action": "unregister", "id": "existing_script", "delete_file": false}),
        &config,
        &paths,
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
async fn lifecycle_mutations_replace_and_remove_all_same_id_entries() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    let scripts_dir = paths.scripts_dir.clone();
    write_script(
        &scripts_dir,
        "old.sh",
        "#!/bin/bash\n# Description: Old\n\necho old",
    );
    write_script(
        &scripts_dir,
        "new.sh",
        "#!/bin/bash\n# Description: New\n\necho new",
    );
    std::fs::write(
        scripts_dir.join("index.json"),
        serde_json::to_string(&json!({
            "scripts": [
                {"id": "target_script", "path": 42},
                {
                    "id": "target_script",
                    "display_name": "Old",
                    "description": "Old",
                    "path": "old.sh"
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    register_script_handler(
        json!({"action": "register", "id": "target_script", "path": "new.sh"}),
        &config,
        &paths,
    )
    .await
    .unwrap();

    let index_path = scripts_dir.join("index.json");
    let mut index = read_script_index_value(&index_path).unwrap();
    let scripts = index_array_mut(&mut index, "scripts").unwrap();
    assert_eq!(
        scripts
            .iter()
            .filter(|entry| raw_entry_field(entry, "id") == Some("target_script"))
            .count(),
        1
    );
    assert_eq!(raw_entry_field(&scripts[0], "path"), Some("new.sh"));

    scripts.insert(0, json!({"id": "target_script", "path": 42}));
    write_script_index_value(&index_path, &index).unwrap();
    unregister_script_handler(
        json!({"action": "unregister", "id": "target_script", "delete_file": false}),
        &config,
        &paths,
    )
    .await
    .unwrap();

    let index = read_script_index_value(&index_path).unwrap();
    let scripts = index.get("scripts").and_then(Value::as_array).unwrap();
    assert!(!scripts
        .iter()
        .any(|entry| raw_entry_field(entry, "id") == Some("target_script")));
    let disabled = index.get("disabled").and_then(Value::as_array).unwrap();
    assert!(disabled.iter().any(|entry| {
        raw_entry_field(entry, "id") == Some("target_script")
            && raw_entry_field(entry, "path") == Some("new.sh")
    }));
}

#[test]
fn list_reports_layers_unregistered_and_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    write_script(
        &paths.scripts_dir,
        "global_tool.sh",
        "#!/bin/sh\n# Description: G\necho\n",
    );
    let persona_dir = config.active_persona_scripts_dir(&paths);
    write_script(
        &persona_dir,
        "persona_tool.sh",
        "#!/bin/sh\n# Description: P\necho\n",
    );
    write_script(&persona_dir, "quiet.sh", "#!/bin/sh\necho\n");
    std::fs::write(
        persona_dir.join("index.json"),
        r#"{"disabled":[{"id":"gone","path":""}]}"#,
    )
    .unwrap();

    let out = parse(&list_scripts_handler(&config, &paths).unwrap());
    assert_eq!(out["ok"], true);
    assert_eq!(
        out["directories"]["global"],
        paths.scripts_dir.display().to_string()
    );
    let registered = out["registered"].as_array().unwrap();
    let layer_of = |id: &str| {
        registered
            .iter()
            .find(|entry| entry["id"] == id)
            .map(|entry| entry["layer"].as_str().unwrap().to_string())
            .unwrap()
    };
    assert_eq!(layer_of("global_tool"), "global");
    assert_eq!(layer_of("persona_tool"), "persona");
    assert_eq!(out["unregistered"][0]["name"], "quiet");
    assert_eq!(out["disabled"][0]["id"], "gone");
    assert_eq!(out["disabled"][0]["scope"], "persona");
}

/// 指纹不变不重扫;目录里多一个文件、改一个文件都会变。
#[test]
fn refresh_only_rescans_when_the_directories_change() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    write_script(
        &paths.scripts_dir,
        "a.sh",
        "#!/bin/sh\n# Description: A\necho\n",
    );

    let first = prepare_script_refresh(None, &config, &paths)
        .unwrap()
        .expect("first scan always runs");
    assert_eq!(first.scan.entries.len(), 1);
    assert!(
        prepare_script_refresh(Some(first.fingerprint), &config, &paths)
            .unwrap()
            .is_none()
    );

    write_script(
        &paths.scripts_dir,
        "b.sh",
        "#!/bin/sh\n# Description: B\necho\n",
    );
    let second = prepare_script_refresh(Some(first.fingerprint), &config, &paths)
        .unwrap()
        .expect("new file changes the fingerprint");
    assert_eq!(second.scan.entries.len(), 2);
    assert_ne!(first.fingerprint, second.fingerprint);
}

/// 屏蔽过的内置脚本,register 只给 id 就能恢复(用户层没有文件可指)。
#[tokio::test]
async fn register_by_id_re_enables_a_hidden_builtin_script() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    let builtin_dir = config.active_persona_system_scripts_dir(&paths);
    write_script(
        &builtin_dir,
        "bat.sh",
        "#!/bin/sh\n# Description: Battery\necho\n",
    );
    unregister_script_handler(
        json!({"action": "unregister", "id": "bat"}),
        &config,
        &paths,
    )
    .await
    .unwrap();
    assert!(scan_roots(&config, &paths).entries.is_empty());

    let out = parse(
        &register_script_handler(json!({"action": "register", "id": "bat"}), &config, &paths)
            .await
            .unwrap(),
    );
    assert_eq!(out["scope"], "builtin");
    assert_eq!(out["state"], "enabled");
    assert_eq!(scan_roots(&config, &paths).entries.len(), 1);

    let error =
        register_script_handler(json!({"action": "register", "id": "nope"}), &config, &paths)
            .await
            .unwrap_err();
    assert!(error.to_string().contains("pass path"), "{error}");
}
