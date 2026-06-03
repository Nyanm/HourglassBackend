use hg_common::{HgConfig, TrackerEvent};

use std::sync::Arc;
use std::time::Duration;
use anyhow::bail;
use tracing::debug;
use tokio::sync::mpsc::UnboundedSender;

use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

// periodic idle re-evaluation poker; shape mirrors UserTracker / WebEvent (new + run)
#[derive(Debug)]
pub struct IdleEvent {
    arc_config: Arc<HgConfig>,
    tx_event: UnboundedSender<TrackerEvent>,
}

impl IdleEvent {
    pub fn new(arc_config: Arc<HgConfig>, tx_event: UnboundedSender<TrackerEvent>) -> Self {
        Self { arc_config, tx_event }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let interval_ms = self.arc_config.idle_check_interval_ms as u64;
        let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));

        loop {
            ticker.tick().await;
            if self.tx_event.send(TrackerEvent::IdleTick).is_err() {
                debug!("idle ticker: receiver dropped, stopping");
                break;
            }
        }

        Ok(())
    }

    pub fn get_last_input_ms(now_ms: i64) -> anyhow::Result<i64> {
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
}
