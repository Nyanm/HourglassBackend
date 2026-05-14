use std::sync::Arc;
use rusqlite::{params, Connection};
use tracing::{debug, info};

use crate::common_struct::{EventType, AppSnapshotInfo, HgConfig};

const SQL_CREATE_TABLE: &str = include_str!("../../query/create_table.sql");
const SQL_INSERT_RAW_EVENTS: &str =
    "INSERT INTO raw_events
        (ts_ms, event_type, flag_switch, pid, process_name, exe_path, window_title, tab_title, url)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)";
const SQL_INSERT_USAGE_SEGMENT: &str =
    "INSERT INTO usage_segments
        (start_ms, end_ms, seg_state, pid, process_name, exe_path, window_title, tab_title, url)
        VALUES (?1, -1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";
const SQL_UPDATE_USAGE_SEGMENT: &str =
    "UPDATE usage_segments SET end_ms = ? WHERE rowid = (SELECT MAX(rowid) FROM usage_segments);";

#[derive(Debug)]
pub struct DbHandler {
    arc_config: Arc<HgConfig>,
    connection: Connection,
}

impl DbHandler {
    pub fn new(arc_config: Arc<HgConfig>) -> anyhow::Result<Self> {
        let connection = Connection::open(&arc_config.db_path)?;

        connection.execute_batch(SQL_CREATE_TABLE)?;

        // quick check of database status
        let cnt_raw_db: i64 = connection.query_row("SELECT COUNT(*) FROM raw_events", [], |row| row.get(0))?;
        let cnt_segment_db: i64 = connection.query_row("SELECT COUNT(*) FROM usage_segments", [], |row| row.get(0))?;

        info!("{} records exist at table raw_events", cnt_raw_db);
        info!("{} records exist at table usage_segments", cnt_segment_db);
        Ok( Self { arc_config, connection } )
    }

    pub fn register_raw_event(&self, event_type: EventType, timestamp: i64, flag_switch: bool, opt_info: &Option<AppSnapshotInfo>) -> anyhow::Result<()> {
        self.connection.execute(SQL_INSERT_RAW_EVENTS, params![
            timestamp,
            event_type as i32,
            if flag_switch { 1 } else { 0 },
            opt_info.as_ref().map(|info| info.win_pid),
            opt_info.as_ref().map(|info| info.process_name.as_str()),
            opt_info.as_ref().map(|info| info.exe_path.as_str()),
            opt_info.as_ref().map(|info| info.window_title.as_str()),
            opt_info.as_ref().map(|info| info.opt_web_info.as_ref().map(|web| web.tab_title.as_str())),
            opt_info.as_ref().map(|info| info.opt_web_info.as_ref().map(|web| web.url.as_str())),
        ])?;

        debug!("record raw event {} at time {}", event_type, timestamp);
        Ok(())
    }

    pub fn register_segment(&self, state: EventType, start_ms: i64, opt_info: &Option<AppSnapshotInfo>) -> anyhow::Result<()> {
        self.connection.execute(SQL_INSERT_USAGE_SEGMENT, params![
            start_ms,
            state as i32,
            opt_info.as_ref().map(|info| info.win_pid),
            opt_info.as_ref().map(|info| info.process_name.as_str()),
            opt_info.as_ref().map(|info| info.exe_path.as_str()),
            opt_info.as_ref().map(|info| info.window_title.as_str()),
            opt_info.as_ref().map(|info| info.opt_web_info.as_ref().map(|web| web.tab_title.as_str())),
            opt_info.as_ref().map(|info| info.opt_web_info.as_ref().map(|web| web.url.as_str())),
        ])?;

        debug!("record segment {} with start time {}", state, start_ms);
        Ok(())
    }

    pub fn update_segment(&self, end_ms: i64) -> anyhow::Result<()> {
        self.connection.execute(SQL_UPDATE_USAGE_SEGMENT, params![end_ms])?;

        debug!("updated segment with end time {}", end_ms);
        Ok(())
    }
}
