/*
 Foreground-switch detection via SetWinEventHook. This is the producer half of the TrackerEvent
 stream and replaces the old per-second GetForegroundWindow poll.

 Two Win32 constraints shape this module:
   1. With WINEVENT_OUTOFCONTEXT (no DLL injection, the safe option), the hook callback is delivered
      through the *calling thread's* message queue. That thread must therefore run a classic
      GetMessage/TranslateMessage/DispatchMessage loop or the callback never fires. We give it a
      dedicated OS thread (not a tokio task, because GetMessageW blocks).
   2. The callback is a bare `extern "system"` function and cannot capture Rust state. We bridge it to
      the async world through a process-global OnceLock holding an UnboundedSender; the callback does
      only the cheapest possible work (capture hwnd + timestamp) and hands everything else to the
      tracker actor.

 Shutdown is cooperative: Drop posts WM_QUIT to the pump thread (which makes GetMessageW return 0),
 then UnhookWinEvent runs and the thread is joined.
 */

use hg_common::TrackerEvent;

use std::sync::OnceLock;
use std::sync::mpsc::channel;
use std::thread::JoinHandle;
use anyhow::{anyhow, Context};
use chrono::Utc;
use tracing::{debug, error, info};
use tokio::sync::mpsc::UnboundedSender;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage,
    CHILDID_SELF, EVENT_SYSTEM_FOREGROUND, MSG, OBJID_WINDOW,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_QUIT,
};

// process-global bridge from the C callback to the async tracker; set once by ForegroundHook::start
static EVENT_TX: OnceLock<UnboundedSender<TrackerEvent>> = OnceLock::new();

// WinEvent callback: filter to real foreground switches
unsafe extern "system" fn process_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND { return; }
    if hwnd.is_null() { return; }
    if id_object != OBJID_WINDOW || id_child != CHILDID_SELF as i32 { return; }

    let at_ms = Utc::now().timestamp_millis();
    if let Some(tx_event) = EVENT_TX.get() {
        let _ = tx_event.send(TrackerEvent::ForegroundChanged { hwnd_addr: hwnd as isize, at_ms });
    }
}

#[derive(Debug)]
pub struct ForegroundHook {
    thread_id: u32,  // pump thread id, used to PostThreadMessage(WM_QUIT) on drop
    opt_join_handle: Option<JoinHandle<()>>,
}

impl ForegroundHook {
    // install the foreground hook on a dedicated message-pump thread
    pub fn start(tx_event: UnboundedSender<TrackerEvent>) -> anyhow::Result<Self> {
        EVENT_TX.set(tx_event).map_err(|_| anyhow!("winevent sender already initialised"))?;  // enable global event tx

        let (tx_id, rx_id) = channel::<u32>();

        let opt_join_handle = Some(std::thread::spawn(move || unsafe {
            let thread_id = GetCurrentThreadId();
            if tx_id.send(thread_id).is_err() { return; }  // send thread id to parent thread through channel

            let hook = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                std::ptr::null_mut(),  // out-of-context: no DLL module
                Some(process_win_event),
                0,  // any process
                0,  // any thread
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            if hook.is_null() {
                error!("SetWinEventHook failed; foreground events disabled");
                return;
            }
            info!("winevent hook installed on thread [{}]", thread_id);

            // message loop is mandatory for WINEVENT_OUTOFCONTEXT delivery; GetMessageW returns 0 on WM_QUIT
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {  // meaningless catch
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            UnhookWinEvent(hook);
            debug!("winevent message loop exited, hook removed");
        }));

        let thread_id = rx_id.recv().context("receive winevent pump thread id")?;
        Ok(Self { thread_id, opt_join_handle })
    }
}

impl Drop for ForegroundHook {
    fn drop(&mut self) {
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0); }  // post WM_QUIT to unblock the pump
        if let Some(handle) = self.opt_join_handle.take() {
            let _ = handle.join();
        }
    }
}
