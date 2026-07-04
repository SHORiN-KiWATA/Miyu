use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmRecord {
    pub id: String,
    pub label: String,
    pub time: String,
    pub audio_file: Option<PathBuf>,
    pub due_at: i64,
    pub pid: Option<u32>,
    pub status: AlarmStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AlarmStatus {
    Scheduled,
    Ringing,
}

pub fn alarms_file(paths: &MiyuPaths) -> PathBuf {
    paths.state_dir.join("alarms.json")
}

pub fn alarm_log_file(paths: &MiyuPaths) -> PathBuf {
    paths.state_dir.join("alarm.log")
}

pub fn parse_alarm_seconds(value: &str) -> Result<u64> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if parts.len() == 1 && parts[0].contains(':') {
        return seconds_until_clock(parts[0]);
    }
    let mut total = 0u64;
    for part in parts {
        if part.len() < 2 {
            bail!("invalid alarm time: {value}")
        }
        let (number, unit) = part.split_at(part.len() - 1);
        let amount = number.parse::<u64>()?;
        total += match unit.to_ascii_lowercase().as_str() {
            "h" => amount * 3600,
            "m" => amount * 60,
            "s" => amount,
            _ => bail!("invalid alarm time unit: {unit}"),
        };
    }
    if total == 0 {
        bail!("alarm time must be greater than zero")
    }
    Ok(total)
}

pub fn due_at_from_time(value: &str) -> Result<i64> {
    Ok(Local::now().timestamp() + parse_alarm_seconds(value)? as i64)
}

pub fn load(paths: &MiyuPaths) -> Result<Vec<AlarmRecord>> {
    let file = alarms_file(paths);
    if !file.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(file)?;
    if content.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&content)?)
}

pub fn save(paths: &MiyuPaths, records: &[AlarmRecord]) -> Result<()> {
    std::fs::create_dir_all(&paths.state_dir)?;
    let file = alarms_file(paths);
    let temp = tempfile::NamedTempFile::new_in(&paths.state_dir)?;
    std::fs::write(temp.path(), serde_json::to_vec_pretty(records)?)?;
    temp.persist(file)?;
    Ok(())
}

pub fn upsert(paths: &MiyuPaths, record: AlarmRecord) -> Result<()> {
    let mut records = load(paths)?;
    records.retain(|existing| existing.id != record.id);
    records.push(record);
    save(paths, &records)
}

pub fn update_status(paths: &MiyuPaths, id: &str, status: AlarmStatus) -> Result<()> {
    let mut records = load(paths)?;
    if let Some(record) = records.iter_mut().find(|record| record.id == id) {
        record.status = status;
    }
    save(paths, &records)
}

pub fn remove(paths: &MiyuPaths, id: &str) -> Result<Option<AlarmRecord>> {
    let mut records = load(paths)?;
    let mut removed = None;
    records.retain(|record| {
        if record.id == id {
            removed = Some(record.clone());
            false
        } else {
            true
        }
    });
    save(paths, &records)?;
    Ok(removed)
}

pub fn cleanup_dead(paths: &MiyuPaths) -> Result<Vec<AlarmRecord>> {
    let records = load(paths)?;
    let active = records
        .into_iter()
        .filter(|record| record.pid.is_none_or(process_exists))
        .collect::<Vec<_>>();
    save(paths, &active)?;
    Ok(active)
}

pub fn stop_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let status = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if status != 0 && process_exists(pid) {
            bail!("failed to stop alarm process {pid}")
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        bail!("alarm cancellation is not supported on this platform")
    }
    Ok(())
}

pub fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn seconds_until_clock(value: &str) -> Result<u64> {
    let Some((hour, minute)) = value.split_once(':') else {
        bail!("invalid clock time: {value}")
    };
    let hour = hour.parse::<u32>()?;
    let minute = minute.parse::<u32>()?;
    if hour > 23 || minute > 59 {
        bail!("invalid clock time: {value}")
    }
    let now = Local::now();
    let today = now.date_naive();
    let target_time = chrono::NaiveTime::from_hms_opt(hour, minute, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid clock time: {value}"))?;
    let mut target = today.and_time(target_time);
    if target <= now.naive_local() {
        target += chrono::Duration::days(1);
    }
    Ok((target - now.naive_local()).num_seconds().max(1) as u64)
}

pub fn format_due_at(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(state_dir: PathBuf) -> MiyuPaths {
        MiyuPaths {
            config_dir: PathBuf::new(),
            config_file: PathBuf::new(),
            secrets_file: PathBuf::new(),
            skills_dir: PathBuf::new(),
            data_dir: PathBuf::new(),
            cache_dir: PathBuf::new(),
            state_dir,
            pictures_dir: PathBuf::new(),
            fish_hook_file: PathBuf::new(),
            bash_hook_file: PathBuf::new(),
            zsh_hook_file: PathBuf::new(),
            scripts_dir: PathBuf::new(),
            system_scripts_dir: PathBuf::new(),
        }
    }

    #[test]
    fn parses_alarm_durations() {
        assert_eq!(parse_alarm_seconds("30s").unwrap(), 30);
        assert_eq!(parse_alarm_seconds("10m").unwrap(), 600);
        assert_eq!(parse_alarm_seconds("1h 2m 3s").unwrap(), 3723);
        assert!(parse_alarm_seconds("0s").is_err());
    }

    #[test]
    fn saves_updates_and_removes_alarm_records() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path().to_path_buf());
        let record = AlarmRecord {
            id: "alarm-test".to_string(),
            label: "test".to_string(),
            time: "30s".to_string(),
            audio_file: None,
            due_at: 123,
            pid: None,
            status: AlarmStatus::Scheduled,
        };
        upsert(&paths, record).unwrap();
        assert_eq!(load(&paths).unwrap().len(), 1);
        update_status(&paths, "alarm-test", AlarmStatus::Ringing).unwrap();
        assert_eq!(load(&paths).unwrap()[0].status, AlarmStatus::Ringing);
        assert!(remove(&paths, "alarm-test").unwrap().is_some());
        assert!(load(&paths).unwrap().is_empty());
    }
}
