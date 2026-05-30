use std::sync::{Arc, Mutex};
use rusqlite::{params, Connection};
use tracing::{debug, error, info};

use crate::common_struct::{EventType, AppSnapshotInfo, HgConfig};
use crate::{SegmentInfo, WebSnapshotInfo};


/*
    writer query sql
*/
const SQL_CREATE_TABLE: &str = include_str!("../../query/create_table.sql");
const SQL_INSERT_USAGE_SEGMENT: &str =
    "INSERT INTO usage_segments
        (start_ms, end_ms, seg_state, pid, process_name, exe_path, window_title, tab_title, url)
        VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";
const SQL_UPDATE_USAGE_SEGMENT: &str =
    "UPDATE usage_segments SET end_ms = ? WHERE rowid = (SELECT MAX(rowid) FROM usage_segments);";
// updates the latest segment's url/tab_title with pid check
const SQL_UPDATE_SEGMENT_WEB: &str =
    "UPDATE usage_segments SET url = ?1, tab_title = ?2 WHERE rowid = (SELECT MAX(rowid) FROM usage_segments) AND pid = ?3";

/*
    reader query sql
*/
const SQL_SELECT_BY_TIME: &str =
    "SELECT id, start_ms, end_ms, seg_state, pid, process_name, exe_path, window_title, tab_title, url
        FROM usage_segments
        WHERE start_ms < ?1 AND (end_ms IS NULL OR end_ms > ?2) ORDER BY start_ms ASC";


#[derive(Debug)]
pub struct DbHandlerWriter {
    mutex_conn: Mutex<Connection>,
}

impl DbHandlerWriter {
    pub fn new(arc_config: Arc<HgConfig>) -> anyhow::Result<Self> {
        let connection = Connection::open(&arc_config.db_path)?;  // read and write

        connection.execute_batch(SQL_CREATE_TABLE)?;

        // quick check of database status
        let cnt_segment_db: i64 = connection.query_row("SELECT COUNT(*) FROM usage_segments", [], |row| row.get(0))?;

        info!("{} records exist at table usage_segments", cnt_segment_db);
        Ok( Self { mutex_conn: Mutex::new(connection) } )
    }

    pub fn register_segment(&self, state: EventType, start_ms: i64, opt_info: &Option<AppSnapshotInfo>) -> anyhow::Result<()> {
        let conn = self.mutex_conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(SQL_INSERT_USAGE_SEGMENT, params![
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
        let conn = self.mutex_conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(SQL_UPDATE_USAGE_SEGMENT, params![end_ms])?;

        debug!("updated segment with end time {}", end_ms);
        Ok(())
    }

    pub fn update_segment_web(&self, str_url: &str, str_tab_title: &str, num_browser_pid: u32) -> anyhow::Result<bool> {
        let conn = self.mutex_conn.lock().unwrap_or_else(|p| p.into_inner());
        let cnt_changed = conn.execute(SQL_UPDATE_SEGMENT_WEB, params![str_url, str_tab_title, num_browser_pid])?;
        if cnt_changed == 0 { debug!("update_segment_web matched 0 rows for pid={}", num_browser_pid); }  // fail to find browser record
        else { debug!("update_segment_web set url for pid={}", num_browser_pid); }
        Ok(cnt_changed > 0)
    }
}

#[derive(Debug)]
struct UsageSegmentRowRaw {
    row_id:       i64,
    seg_start:    i64,
    seg_end:      Option<i64>,
    seg_state:    i64,
    pid:          Option<i64>,
    process_name: Option<String>,
    exe_path:     Option<String>,
    window_title: Option<String>,
    tab_title:    Option<String>,
    url:          Option<String>,
}

#[derive(Debug)]
pub struct DbHandlerReader {
    connection: Connection,
}

impl DbHandlerReader {
    pub fn new(arc_config: Arc<HgConfig>) -> anyhow::Result<Self> {
        let connection = Connection::open_with_flags(&arc_config.db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;  // read only

        Ok( Self { connection } )
    }

    pub fn query_by_time(&self, start_ms: i64, end_ms: i64) -> anyhow::Result<Vec<SegmentInfo>> {
        let mut statement = self.connection.prepare(SQL_SELECT_BY_TIME)?;
        let map_user_segment_raw = statement.query_map([end_ms, start_ms], |row| {
            let row_id:       i64            = row.get(0)?;
            let seg_start:    i64            = row.get(1)?;
            let seg_end:      Option<i64>    = row.get(2)?;
            let seg_state:    i64            = row.get(3)?;
            let pid:          Option<i64>    = row.get(4)?;
            let process_name: Option<String> = row.get(5)?;
            let exe_path:     Option<String> = row.get(6)?;
            let window_title: Option<String> = row.get(7)?;
            let tab_title:    Option<String> = row.get(8)?;
            let url:          Option<String> = row.get(9)?;

            Ok(UsageSegmentRowRaw {row_id, seg_start, seg_end, seg_state, pid, process_name, exe_path, window_title, tab_title, url} )
        })?;

        let mut vec_ret = Vec::new();
        for row_user_segment_raw in map_user_segment_raw {
            let _row_user_segment_raw = row_user_segment_raw?;

            // convert event type
            let event_type = match _row_user_segment_raw.seg_state {
                0 => EventType::Active,
                1 => EventType::Idle,
                2 => EventType::Online,
                3 => EventType::Offline,
                other => { error!("unknown event type: [{}]", other); continue },
            };

            let row_id = _row_user_segment_raw.row_id;
            let raw_end_ms = _row_user_segment_raw.seg_end.unwrap_or(end_ms);  // unfinished segment

            // fill web snapshot info if exists
            let opt_web_info = match _row_user_segment_raw.url.as_deref() {
                Some(_url) if !_url.is_empty() => Some(WebSnapshotInfo {tab_title: _row_user_segment_raw.tab_title.unwrap_or_default(), url: _url.to_string()}),
                _                              => None,
            };

            // fill app snapshot info
            let app_snapshot_info = AppSnapshotInfo {
                win_pid:      _row_user_segment_raw.pid.unwrap_or(0) as u32,
                process_name: _row_user_segment_raw.process_name.unwrap_or_default(),
                exe_path:     _row_user_segment_raw.exe_path.unwrap_or_default(),
                window_title: _row_user_segment_raw.window_title.unwrap_or_default(),
                opt_web_info,
            };

            vec_ret.push(SegmentInfo{
                row_id,
                event_type,
                start_ms: _row_user_segment_raw.seg_start,
                end_ms: raw_end_ms,
                app_snapshot_info})
        }

        info!("query_by_time return [{}] valid rows", vec_ret.len());
        Ok( vec_ret )
    }
}