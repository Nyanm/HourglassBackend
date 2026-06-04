/*
 The tracker is the single owner of the usage-segment state machine. It no longer polls the
 foreground window itself; instead it passively consumes a stream of TrackerEvent values produced
 by three independent sources:
   - surveillant_win_event: a SetWinEventHook message pump that emits ForegroundChanged on every
     EVENT_SYSTEM_FOREGROUND, i.e. the moment the OS switches the active window.
   - surveillant_idle_event: a timer that emits IdleTick so we can re-evaluate Active/Idle, which has
     no native Win32 event.
   - surveillant_web_event: a pure UDP unpacker that emits WebUpdate(WebReportInfo) per browser
     report; this task applies the focused + pid policy and the fill/fork DB write.
 Because every mutation of `state` / `opt_app_info` and every database write happens on this one
 task, the foreground pid is matched against our own state and the previous cross-thread race
 (and the shared arc_curr_pid mutex) is removed by construction.
 */

use hg_common::{EventType, HgConfig, AppSnapshotInfo, DbHandlerWriter, TrackerEvent, WebReportInfo};
use crate::surveillant_idle_event::IdleEvent;
use crate::surveillant_web_event::WebEvent;
use crate::surveillant_win_event::WinEvent;

use std::path::Path;
use std::sync::Arc;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use anyhow::Context;
use tracing::{debug, trace};
use chrono::Utc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::{sleep_until, Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HWND};
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId};

#[derive(Debug)]
pub struct UserTracker {
    arc_config: Arc<HgConfig>,
    arc_db_handler: Arc<DbHandlerWriter>,

    // current segment info
    state: EventType,
    opt_app_info: Option<AppSnapshotInfo>,

    // foreground debounce: pending switch data + its commit deadline; both overwritten on each new switch
    opt_pending_fg: Option<(isize, i64)>,  // (hwnd_addr, at_ms) of the not-yet-committed switch
    opt_pending_timer: Option<Instant>,
    opt_tx_event: Option<UnboundedSender<TrackerEvent>>,
}

impl Drop for UserTracker {
    fn drop(&mut self) {
        // close last segment and start a new offline segment
        let now_ms = Utc::now().timestamp_millis();
        self.state = EventType::Offline;
        self.register_database(now_ms).expect("fail to register drop segment");

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
            opt_pending_fg: None,
            opt_pending_timer: None,
            opt_tx_event: None,
        }
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        let (tx_event, mut rx_event) = unbounded_channel::<TrackerEvent>();

        let _win_event = WinEvent::start(tx_event.clone()).expect("Failed to install foreground hook");
        let idle_event = IdleEvent::new(Arc::clone(&self.arc_config), tx_event.clone());
        let web_event = WebEvent::new(Arc::clone(&self.arc_config), tx_event.clone());
        self.opt_tx_event = Some(tx_event.clone());  // debounce timer posts CommitForeground through this
        drop(tx_event);

        self.seed_initial()?;

        tokio::select! {
            r = self.dispatch_events(&mut rx_event) => r,
            r = idle_event.run() => r,
            r = web_event.run() => r,
        }
    }

    // drain the event channel until it closes, dispatching each event to its handler
    async fn dispatch_events(&mut self, rx_event: &mut UnboundedReceiver<TrackerEvent>) -> anyhow::Result<()> {
        while let Some(event) = rx_event.recv().await {
            match event {
                TrackerEvent::ForegroundChanged { hwnd_addr, at_ms } => self.on_foreground(hwnd_addr, at_ms)?,
                TrackerEvent::IdleTick => self.on_idle_tick()?,
                TrackerEvent::WebUpdate(report) => self.on_web_update(report)?,
                TrackerEvent::CommitForeground => self.commit_foreground()?,
                TrackerEvent::Shutdown => { debug!("tracker received shutdown"); break; }
            }
        }

        Ok(())
    }

    // one-shot startup sample so the first segment carries the current app + idle state
    fn seed_initial(&mut self) -> anyhow::Result<()> {
        let now_ms = Utc::now().timestamp_millis();

        let last_input_ms = IdleEvent::get_last_input_ms(now_ms).context("Failed to get last input info")?;
        self.state = if now_ms - last_input_ms >= self.arc_config.idle_timeout_ms { EventType::Idle } else { EventType::Active };

        self.opt_app_info = Self::current_foreground_snapshot();

        self.register_database(now_ms)?;
        debug!("seeded initial segment with state [{}]", self.state);
        Ok(())
    }

    // foreground entry: (re)arm the debounce; a spawned timer posts CommitForeground back when it settles
    fn on_foreground(&mut self, hwnd_addr: isize, at_ms: i64) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_millis(self.arc_config.foreground_debounce_ms as u64);
        self.opt_pending_fg = Some((hwnd_addr, at_ms));
        self.opt_pending_timer = Some(deadline);

        if let Some(tx_event) = &self.opt_tx_event {
            let tx_event = tx_event.clone();
            tokio::spawn(async move {
                sleep_until(deadline).await;
                let _ = tx_event.send(TrackerEvent::CommitForeground);  // actor may be gone on shutdown -> ignore
            });
        }
        Ok(())
    }

    // debounce elapsed: commit the settled foreground; reject stale timer signals via the stored deadline
    fn commit_foreground(&mut self) -> anyhow::Result<()> {
        match (self.opt_pending_fg, self.opt_pending_timer) {
            (Some((hwnd_addr, at_ms)), Some(deadline)) if Instant::now() >= deadline => {
                self.opt_pending_fg = None;
                self.opt_pending_timer = None;

                let hwnd = hwnd_addr as HWND;
                let opt_foreground = Self::snapshot_from_hwnd(hwnd);
                let flag_foreground_switch = self.opt_app_info != opt_foreground;
                self.opt_app_info = opt_foreground;

                if flag_foreground_switch {
                    self.register_database(at_ms)?;
                    debug!("foreground switched, registered segment");
                }
            }
            _ => trace!("commit_foreground: stale or empty signal, skip"),
        }
        Ok(())
    }

    // apply one unpacked browser report: focused + pid gate against our own foreground, then DB update
    fn on_web_update(&self, report: WebReportInfo) -> anyhow::Result<()> {
        if !report.is_focused {
            debug!("web update ignored: focused=false");
            return Ok(());
        }

        let num_curr_pid = self.opt_app_info.as_ref().map_or(0, |info| info.win_pid);
        if report.browser_pid == 0 || num_curr_pid != report.browser_pid {
            debug!("web update pid mismatch: msg={} curr={}, drop", report.browser_pid, num_curr_pid);
            return Ok(());
        }

        let str_url = report.opt_url.unwrap_or_default();
        let str_title = report.opt_title.unwrap_or_default();
        self.arc_db_handler.apply_web_update(&str_url, &str_title, report.browser_pid, report.at_ms)?;

        debug!("apply_web_update dispatched: event={} url={} title={}", report.str_event, str_url, str_title);
        Ok(())
    }

    // re-evaluate Active/Idle on a timer poke and open a new segment only when the state flips
    fn on_idle_tick(&mut self) -> anyhow::Result<()> {
        let now_ms = Utc::now().timestamp_millis();

        let last_input_ms = IdleEvent::get_last_input_ms(now_ms).context("Failed to get last input info")?;
        let legacy_state = self.state;
        self.state = if now_ms - last_input_ms >= self.arc_config.idle_timeout_ms { EventType::Idle } else { EventType::Active };
        trace!("idle for {} ms", now_ms - last_input_ms);

        if legacy_state != self.state {
            // backdate the Active->Idle boundary to the last real input, otherwise stamp now
            let timestamp = if legacy_state == EventType::Active && self.state == EventType::Idle { last_input_ms } else { now_ms };
            self.register_database(timestamp)?;
            debug!("state changed [{}] -> [{}], registered segment", legacy_state, self.state);
        }
        Ok(())
    }

    fn register_database(&self, timestamp: i64) -> anyhow::Result<()> {
        self.arc_db_handler.update_segment(timestamp)?;
        self.arc_db_handler.register_segment(self.state, timestamp, &self.opt_app_info)?;

        debug!("register segment with state [{}]", self.state);
        Ok(())
    }

    fn current_foreground_snapshot() -> Option<AppSnapshotInfo> {
        unsafe { Self::snapshot_from_hwnd(GetForegroundWindow()) }
    }

    // resolve a given HWND into a snapshot
    fn snapshot_from_hwnd(hwnd: HWND) -> Option<AppSnapshotInfo> {
        unsafe {
            if hwnd.is_null() { return None; }

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

            debug!("record app snapshot: {:?}", ret_snapshot);
            Some(ret_snapshot)
        }
    }
}
