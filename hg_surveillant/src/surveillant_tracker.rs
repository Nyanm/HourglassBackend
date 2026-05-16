use hg_common::{EventType, HgConfig, AppSnapshotInfo, DbHandlerWriter};

use std::path::Path;
use std::sync::Arc;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::time::Duration;
use anyhow::{bail, Context};
use tracing::{debug, trace};
use chrono::Utc;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId};

#[derive(Debug)]
pub struct UserTracker {
    arc_config: Arc<HgConfig>,
    arc_db_handler: Arc<DbHandlerWriter>,

    // current segment info
    state: EventType,
    opt_app_info: Option<AppSnapshotInfo>,
}

impl Drop for UserTracker {
    fn drop(&mut self) {
        // close last segment and start a new offline segment
        let now_ms = Utc::now().timestamp_millis();
        self.state = EventType::Offline;
        self.register_database(now_ms, true).expect("fail to register drop segment");

        debug!("see you next time");
    }
}

impl UserTracker {
    pub fn new(arc_config: Arc<HgConfig>, arc_db_handler: Arc<DbHandlerWriter>) -> Self {
        Self {
            arc_config,
            arc_db_handler,
            state: EventType::Online,
            opt_app_info: None,
        }
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        let interval = Duration::from_millis(self.arc_config.pool_interval_ms as u64);
        loop {
            self.tick()?;
            tokio::time::sleep(interval).await;
        }
    }

    fn tick(&mut self) -> anyhow::Result<()> {
        let now_ms = Utc::now().timestamp_millis();
        trace!("tracker ticks at [{}]", now_ms);

        // get current state
        let last_input_ms = Self::get_last_input_time(now_ms).context("Failed to get last input info")?;
        let legacy_state = self.state;
        self.state = if now_ms - last_input_ms >= self.arc_config.idle_timeout_ms { EventType::Idle } else { EventType::Active };
        let flag_state_change = legacy_state != self.state;
        trace!("idle for {} ms", now_ms - last_input_ms);

        // get current foreground info
        let opt_foreground = Self::get_current_foreground_snapshot();
        let flag_foreground_switch = self.opt_app_info != opt_foreground;
        self.opt_app_info = opt_foreground;

        let timestamp = if legacy_state == EventType::Active && self.state == EventType::Idle { last_input_ms } else { now_ms };
        // state or foreground app changed or first time for registration
        if flag_state_change || flag_foreground_switch || legacy_state == EventType::Online {
            self.register_database(timestamp, flag_state_change)?;
            return Ok(())
        }

        trace!("keep on [{}]", self.state);
        Ok(())
    }

    fn register_database(&self, timestamp: i64, flag_switch: bool) -> anyhow::Result<()> {
        self.arc_db_handler.update_segment(timestamp)?;
        self.arc_db_handler.register_raw_event(self.state, timestamp, flag_switch, &self.opt_app_info)?;
        self.arc_db_handler.register_segment(self.state, timestamp, &self.opt_app_info)?;

        debug!("register segment with state [{}], foreground switch [{}]", self.state, flag_switch);
        Ok(())
    }

    fn get_last_input_time(now_ms: i64) -> anyhow::Result<i64> {
        let mut last_input_info = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };

        unsafe {
            if GetLastInputInfo(&mut last_input_info) == 0 { bail!("GetLastInputInfo failed"); }

            let tick_ms_32 = GetTickCount64() as u32;
            let idle_ms = tick_ms_32.wrapping_sub(last_input_info.dwTime) as i64;

            Ok(now_ms - idle_ms)
        }
    }

    fn get_current_foreground_snapshot() -> Option<AppSnapshotInfo> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd == std::ptr::null_mut() { return None; }

            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == 0 { return None; }
            trace!("get foreground process id [{}]", pid);

            // get window name (title)
            let mut str_window_title = String::new();
            let len_title = GetWindowTextLengthW(hwnd);
            trace!("get window text length [{}], 0 as default", len_title);

            if len_title > 0 {
                // copy memory from windows
                let mut vec_buf_title = vec![0u16; len_title as usize + 1];
                let num_copied = GetWindowTextW(hwnd, vec_buf_title.as_mut_ptr(), len_title + 1);
                trace!("copy [{}] bytes from windows api", len_title);
                if num_copied > 0 {
                    str_window_title = String::from_utf16_lossy(&vec_buf_title[..num_copied as usize]);
                }
            }
            trace!("window title: {}", str_window_title);

            // get process path
            let mut str_process_path = "<unknown>".to_string();
            let process_handler = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process_handler != std::ptr::null_mut() {
                let mut num_buf_size: u32 = 32768;
                let mut vec_buf = vec![0u16; num_buf_size as usize];
                let flag_ok = QueryFullProcessImageNameW(process_handler, 0, vec_buf.as_mut_ptr(), &mut num_buf_size);
                CloseHandle(process_handler);
                trace!("get process length [{}], 32768 as default", num_buf_size);
                if flag_ok != 0 { str_process_path = OsString::from_wide(&vec_buf[..num_buf_size as usize]).to_string_lossy().to_string(); }
            }
            trace!("process path: {}", str_process_path);

            // depack process name
            let str_process_name = Path::new(&str_process_path).file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            trace!("process name: {}", str_process_name);

            let ret_snapshot = AppSnapshotInfo {
                win_pid: pid,
                process_name: str_process_name,
                exe_path: str_process_path,
                window_title: str_window_title,
                opt_web_info: None};

            trace!("record app snapshot: {:?}", ret_snapshot);
            Some(ret_snapshot)
        }
    }
}