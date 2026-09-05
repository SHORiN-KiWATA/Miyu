//! 控制台脚本面板的数据面:概览分层与覆盖标记、源码预览边界、四个写操作。

use super::{test_env, write_script};
use crate::tools::scripts::*;

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn overview_reports_layers_counts_overrides_unregistered_and_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    let builtin_dir = config.active_persona_system_scripts_dir(&paths);
    write_script(
        &builtin_dir,
        "bat.sh",
        "#!/bin/sh\n# Description: Battery\n# Parameters: {\"type\":\"object\",\"properties\":{\"cmd\":{\"type\":\"string\"}}}\necho\n",
    );
    write_script(
        &paths.scripts_dir,
        "light.py",
        "#!/usr/bin/env python3\n# Description: Header text\n# Argv: flags\nprint(1)\n",
    );
    std::fs::write(
        paths.scripts_dir.join("index.json"),
        serde_json::to_string(&json!({
            "scripts": [{ "id": "light", "path": "light.py", "description": "Index text", "always_loaded": true }],
            "disabled": [{ "id": "gone", "path": "gone.sh" }]
        }))
        .unwrap(),
    )
    .unwrap();
    let persona_dir = config.active_persona_scripts_dir(&paths);
    write_script(&persona_dir, "quiet.sh", "#!/bin/sh\necho\n");

    let overview = scripts_dashboard_overview(&config, &paths).unwrap();
    assert_eq!(overview["counts"]["registered"], 2);
    assert_eq!(overview["counts"]["builtin"], 1);
    assert_eq!(overview["counts"]["user"], 1);
    assert_eq!(overview["counts"]["unregistered"], 1);
    assert_eq!(overview["counts"]["disabled"], 1);
    assert_eq!(overview["counts"]["always_loaded"], 1);

    let scripts = overview["scripts"].as_array().unwrap();
    let bat = scripts.iter().find(|s| s["id"] == "bat").unwrap();
    assert_eq!(bat["layer"], "builtin-persona");
    assert_eq!(bat["builtin"], true);
    assert_eq!(bat["parameter_names"][0], "cmd");
    assert_eq!(bat["executable"], false, "夹具没设可执行位,面板要如实标出");
    assert_eq!(bat["timeout_default"], true);

    let light = scripts.iter().find(|s| s["id"] == "light").unwrap();
    assert_eq!(light["layer"], "global");
    assert_eq!(light["description"], "Index text");
    assert_eq!(light["argv"], "flags");
    assert_eq!(light["always_loaded"], true);
    assert_eq!(light["override_scope"], "global");
    let fields: Vec<&str> = light["overrides"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert_eq!(fields, vec!["description", "always_loaded"]);
    assert_eq!(light["header"]["has_description"], true);
    assert_eq!(light["header"]["argv"], "flags");

    assert_eq!(overview["unregistered"][0]["name"], "quiet");
    assert_eq!(overview["unregistered"][0]["layer"], "persona");
    assert_eq!(overview["disabled"][0]["id"], "gone");
    assert_eq!(overview["disabled"][0]["scope"], "global");
    assert_eq!(overview["disabled"][0]["builtin"], false);
    assert_eq!(
        overview["directories"]["persona"],
        persona_dir.display().to_string()
    );
}

#[test]
fn source_preview_reads_the_file_head_and_rejects_outside_paths() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    let mut body = String::from("#!/bin/sh\n# Description: Many lines\n");
    for i in 0..600 {
        body.push_str(&format!("echo {i}\n"));
    }
    write_script(&paths.scripts_dir, "long.sh", &body);
    let outside = write_script(&temp.path().join("elsewhere"), "x.sh", "#!/bin/sh\n");

    let by_id = scripts_dashboard_source(&config, &paths, "long", "", 80).unwrap();
    assert_eq!(by_id["shown"], 80);
    assert_eq!(by_id["truncated"], true);
    assert_eq!(by_id["lines"][1], "# Description: Many lines");

    let by_path = scripts_dashboard_source(
        &config,
        &paths,
        "",
        &paths.scripts_dir.join("long.sh").display().to_string(),
        10_000,
    )
    .unwrap();
    assert_eq!(by_path["shown"], 400, "行数封顶");

    let error = scripts_dashboard_source(&config, &paths, "", &outside.display().to_string(), 10)
        .unwrap_err();
    assert!(error.to_string().contains("outside"), "{error}");
    assert!(scripts_dashboard_source(&config, &paths, "nope", "", 10).is_err());
}

#[test]
fn disable_enable_delete_and_register_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let (config, paths) = test_env(temp.path());
    write_script(
        &paths.scripts_dir,
        "tool.sh",
        "#!/bin/sh\n# Description: Tool\necho\n",
    );
    let builtin_dir = config.active_persona_system_scripts_dir(&paths);
    write_script(
        &builtin_dir,
        "bat.sh",
        "#!/bin/sh\n# Description: Battery\necho\n",
    );
    let quiet = write_script(&paths.scripts_dir, "quiet.sh", "#!/bin/sh\necho\n");

    // 禁用用户脚本与内置脚本
    let out = scripts_dashboard_disable(&config, &paths, "tool").unwrap();
    assert_eq!(out["state"], "disabled");
    let out = scripts_dashboard_disable(&config, &paths, "bat").unwrap();
    assert_eq!(out["scope"], "persona");
    let overview = scripts_dashboard_overview(&config, &paths).unwrap();
    assert!(ids(&overview["scripts"]).is_empty());
    assert_eq!(overview["counts"]["disabled"], 2);

    // 启用:清 disabled 记录即恢复;再次启用报告没变化
    assert_eq!(
        scripts_dashboard_enable(&config, &paths, "tool").unwrap()["enabled"],
        true
    );
    assert_eq!(
        scripts_dashboard_enable(&config, &paths, "bat").unwrap()["enabled"],
        true
    );
    assert_eq!(
        scripts_dashboard_enable(&config, &paths, "bat").unwrap()["enabled"],
        false
    );
    let overview = scripts_dashboard_overview(&config, &paths).unwrap();
    assert_eq!(ids(&overview["scripts"]), vec!["bat", "tool"]);

    // 补描述注册未注册文件
    let out = scripts_dashboard_register(
        &config,
        &paths,
        &quiet.display().to_string(),
        "Quiet helper",
        "",
    )
    .unwrap();
    assert_eq!(out["id"], "quiet");
    assert_eq!(out["copied"], false);
    let overview = scripts_dashboard_overview(&config, &paths).unwrap();
    assert_eq!(overview["counts"]["unregistered"], 0);
    assert_eq!(ids(&overview["scripts"]), vec!["bat", "quiet", "tool"]);
    let outside = write_script(
        &temp.path().join("w"),
        "o.sh",
        "#!/bin/sh\n# Description: o\n",
    );
    assert!(
        scripts_dashboard_register(&config, &paths, &outside.display().to_string(), "x", "")
            .is_err()
    );

    // 删除:用户脚本文件消失;内置脚本拒绝
    let out = scripts_dashboard_delete(&config, &paths, "quiet").unwrap();
    assert_eq!(out["state"], "deleted");
    assert!(!quiet.exists());
    let error = scripts_dashboard_delete(&config, &paths, "bat").unwrap_err();
    assert!(error.to_string().contains("built-in"), "{error}");
}
