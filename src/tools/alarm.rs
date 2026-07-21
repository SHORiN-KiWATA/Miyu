use super::{ToolRegistry, ToolSpec};
use crate::alarm::{self, AlarmRecord, AlarmStatus};
use crate::i18n::agent_text as t;
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use chrono::Local;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

pub fn register(registry: &mut ToolRegistry, paths: MiyuPaths) {
    let set_paths = paths.clone();
    registry.register(ToolSpec::new(
        "set_alarm",
        t(
            "Set a local alarm or countdown. Accepts duration like 30s, 10m, 1h 30m, or a time like 14:30. The alarm runs in a background Miyu process and uses Miyu's embedded sound.",
            "设置本地闹钟或倒计时。支持 30s、10m、1h 30m 或 14:30。闹钟在后台 Miyu 进程运行，并使用 Miyu 内置声音。",
        ),
        json!({
            "type": "object",
            "properties": {
                "time": { "type": "string", "description": t("Duration or clock time.", "时长或时钟时间。") },
                "label": { "type": "string", "description": t("Optional alarm label.", "可选闹钟标签。") },
                "audio_file": { "type": "string", "description": t("Optional local .wav or .mp3 audio file to play instead of Miyu's built-in alarm sound.", "可选本地 .wav 或 .mp3 音频文件，用它替代 Miyu 内置闹钟音。") }
            },
            "required": ["time"],
            "additionalProperties": false
        }),
        move |args| {
            let paths = set_paths.clone();
            async move { set_alarm(args, paths).await }
        },
    ).writes());
    let list_paths = paths.clone();
    registry.register(ToolSpec::new(
        "list_alarms",
        t(
            "List currently scheduled or ringing local alarms.",
            "列出当前已设定或正在响的本地闹钟。",
        ),
        json!({"type":"object","properties":{},"additionalProperties":false}),
        move |_args| {
            let paths = list_paths.clone();
            async move { list_alarms(paths).await }
        },
    ));
    let cancel_paths = paths.clone();
    registry.register(ToolSpec::new(
        "cancel_alarm",
        t(
            "Cancel a scheduled or ringing alarm by id. Use list_alarms first if the id is unknown.",
            "按 id 取消已设定或正在响的闹钟。不知道 id 时先用 list_alarms。",
        ),
        json!({"type":"object","properties":{"id":{"type":"string","description":t("Alarm id from set_alarm or list_alarms.","set_alarm 或 list_alarms 返回的闹钟 id。")}},"required":["id"],"additionalProperties":false}),
        move |args| {
            let paths = cancel_paths.clone();
            async move { cancel_alarm(args, paths).await }
        },
    ).writes());
}

async fn set_alarm(args: Value, paths: MiyuPaths) -> Result<String> {
    let time = args
        .get("time")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if time.is_empty() {
        bail!("time is required")
    }
    let label = args
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("Miyu alarm")
        .trim();
    let audio_file = args
        .get("audio_file")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(resolve_audio_file)
        .transpose()?;
    let due_at = alarm::due_at_from_time(time)?;
    let id = format!(
        "alarm-{}-{}",
        Local::now().timestamp_millis(),
        std::process::id()
    );
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .arg("__alarm-worker")
        .arg("--id")
        .arg(&id)
        .arg("--time")
        .arg(time)
        .arg("--label")
        .arg(label)
        .arg("--state-dir")
        .arg(&paths.state_dir)
        .arg("--cache-dir")
        .arg(&paths.cache_dir);
    if let Some(path) = &audio_file {
        command.arg("--audio-file").arg(path);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let pid = child.id();
    alarm::upsert(
        &paths,
        AlarmRecord {
            id: id.clone(),
            label: label.to_string(),
            time: time.to_string(),
            audio_file: audio_file.clone(),
            due_at,
            pid,
            status: AlarmStatus::Scheduled,
        },
    )?;
    Ok(json!({
        "ok": true,
        "id": id,
        "time": time,
        "label": label,
        "audio_file": audio_file,
        "due_at": due_at,
        "due_at_local": alarm::format_due_at(due_at),
        "pid": pid,
    })
    .to_string())
}

async fn list_alarms(paths: MiyuPaths) -> Result<String> {
    let records = alarm::cleanup_dead(&paths)?;
    let alarms = records
        .into_iter()
        .map(|record| {
            json!({
                "id": record.id,
                "label": record.label,
                "time": record.time,
                "audio_file": record.audio_file,
                "due_at": record.due_at,
                "due_at_local": alarm::format_due_at(record.due_at),
                "pid": record.pid,
                "status": record.status,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({"ok": true, "alarms": alarms}).to_string())
}

async fn cancel_alarm(args: Value, paths: MiyuPaths) -> Result<String> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if id.is_empty() {
        bail!("id is required")
    }
    let removed = alarm::remove(&paths, id)?;
    if let Some(record) = &removed {
        if let Some(pid) = record.pid {
            if alarm::process_exists(pid) {
                alarm::stop_process(pid)?;
            }
        }
    }
    Ok(json!({"ok": removed.is_some(), "id": id, "removed": removed.is_some()}).to_string())
}

fn resolve_audio_file(value: &str) -> Result<PathBuf> {
    let path = expand_path(value.trim());
    let canonical = path.canonicalize()?;
    if !canonical.is_file() {
        bail!("audio_file is not a regular file: {}", path.display())
    }
    let extension = canonical
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "wav" | "mp3") {
        bail!("audio_file must be a .wav or .mp3 file")
    }
    Ok(canonical)
}

fn expand_path(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}
