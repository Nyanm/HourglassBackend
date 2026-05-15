use std::fs::File;
use std::io::Read;
use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum_macros::Display)]
pub enum EventType {
    Active,
    Idle,
    Online,
    Offline,
}

#[derive(Debug, Clone)]
pub struct WebSnapshotInfo {
    pub tab_title: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct AppSnapshotInfo {
    pub win_pid: u32,
    pub process_name: String,
    pub exe_path: String,
    pub window_title: String,
    pub opt_web_info: Option<WebSnapshotInfo>,
}
impl PartialEq for AppSnapshotInfo {
    fn eq(&self, other: &Self) -> bool {
        // same process should have same pid and same path
        self.win_pid == other.win_pid && self.exe_path == other.exe_path
    }
}
impl Eq for AppSnapshotInfo {}

#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub row_id: i64,
    pub event_type: EventType,
    pub start_ms: i64,
    pub end_ms: i64,
    pub app_snapshot_info: AppSnapshotInfo,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HgConfig {
    pub db_path: String,
    pub pool_interval_ms: i64,
    pub idle_timeout_ms: i64,
}
impl HgConfig {
    pub fn new(str_path: &str) -> anyhow::Result<Self> {
        let mut config_file = File::open(str_path).with_context(|| format!("fail to read config file at {}", str_path))?;
        let mut config_string = String::new();
        config_file.read_to_string(&mut config_string)?;
        Ok(serde_yaml::from_str(&config_string)?)
    }
}
