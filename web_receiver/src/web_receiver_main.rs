/*
 Entry point for the web_receiver Native Messaging Host.

 Everything here is best-effort: logging opens a plain file in cwd,
 config is located by walking up from the executable path, and any
 failure ends the process silently with exit code 0. The runtime loop
 then reads stdin frames and forwards them via UDP.

 stdout is owned by the Native Messaging protocol and must never be
 written to from this binary; all diagnostics go to the log file.

 Wire format toward hourglass is plain JSON: each native messaging
 frame body coming off stdin is parsed, the parent browser pid (looked
 up once at startup via the toolhelp32 snapshot) is injected as a
 "browser_pid" field, and the augmented object is re-serialised and
 sent as a single UDP datagram. Keeping the wire format pure JSON
 means standard tools (tcpdump, hexdump, jq) can inspect the traffic
 without knowing any custom framing rules.

 stdin EOF is the canonical "browser closed the port" signal and is
 treated as a clean shutdown. Per-frame parse / serialize / send
 failures emit a single warn line and the loop continues so a
 transient bad frame cannot tear down the port. Protocol-level
 breakage on stdin (truncated frame, oversized length header) returns
 Err so the process exits.

 stdout is reserved for the Native Messaging protocol itself and is
 NEVER written to from here; warn lines go to the tracing subscriber
 set up by main.
 */

use std::collections::HashMap;
use std::io::Read;
use std::net::UdpSocket;
use std::path::Path;
use std::sync::Mutex;
use anyhow::{bail, Context};
use tracing::{debug, info, warn};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

use hg_common::HgConfig;

// guard against runaway walks (symlink cycles) when searching upward for config.yaml
const NUM_MAX_PARENT_WALK: usize = 8;
// Native Messaging hard cap per Chromium spec; defensive guard against bogus length headers
const NUM_MAX_FRAME_BYTES: usize = 1024 * 1024;
// any free local port is fine since we only send; loopback bind keeps traffic local
const STR_LOCAL_BIND_ADDR: &str = "127.0.0.1:0";
// key under which the browser pid is injected into each forwarded JSON object
const STR_BROWSER_PID_KEY: &str = "browser_pid";
// hard cap on how many parent hops we walk while resolving the topmost browser ancestor
const NUM_MAX_ANCESTRY_HOPS: usize = 10;
// exe basename (lowercased) we treat as Chromium-family browsers when walking the parent chain
const VEC_BROWSER_EXES: &[&str] = &["chrome.exe", "msedge.exe", "brave.exe"];

fn read_frame<R: Read>(reader: &mut R) -> anyhow::Result<Option<Vec<u8>>> {
    // UnexpectedEof at the very start of a frame is the legitimate "browser closed the port" signal
    let mut buf_len = [0u8; 4];
    match reader.read_exact(&mut buf_len) {  // consume the native message header
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("read frame length"),
    }

    let num_len = u32::from_le_bytes(buf_len) as usize;
    if num_len > NUM_MAX_FRAME_BYTES { bail!("frame length {} exceeds limit {}", num_len, NUM_MAX_FRAME_BYTES); }
    if num_len == 0 { return Ok(Some(Vec::new())); }  // empty frame from browser

    let mut vec_body = vec![0u8; num_len];  // real data
    reader.read_exact(&mut vec_body).context("read frame body")?;
    Ok(Some(vec_body))
}

// one-shot snapshot of all processes; values are (parent_pid, exe_basename_as_seen_by_toolhelp)
fn snapshot_process_map() -> Option<HashMap<u32, (u32, String)>> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE { return None; }

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

        let mut map_pid_parent: HashMap<u32, (u32, String)> = HashMap::new();
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                // szExeFile is a fixed-size null-terminated UTF-16 buffer; find the terminator manually
                let num_len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                let str_exe = String::from_utf16_lossy(&entry.szExeFile[..num_len]);
                map_pid_parent.insert(entry.th32ProcessID, (entry.th32ParentProcessID, str_exe));  // self pid-> parent pid, OWN exe
                if Process32NextW(snapshot, &mut entry) == 0 { break; }
            }
        }
        CloseHandle(snapshot);
        Some(map_pid_parent)
    }
}

// resolves the topmost browser-family ancestor pid by walking the process tree; 0 means "unknown / not under a browser"
fn lookup_browser_pid() -> u32 {
    let Some(map) = snapshot_process_map() else {
        warn!("Toolhelp32 snapshot failed; browser_pid stays 0");
        return 0;
    };

    let num_pid_self = std::process::id();  // pid of web_receiver
    let str_self_exe = map.get(&num_pid_self).map(|(_, n)| n.as_str()).unwrap_or("<unknown>");
    info!("self pid={} exe={}", num_pid_self, str_self_exe);

    let mut num_pid_browser: u32 = 0;
    let mut num_pid_cursor = num_pid_self;
    for num_hop in 1..=NUM_MAX_ANCESTRY_HOPS {
        // map entry holds (parent_pid, OWN exe); to learn the parent's exe we lookup the parent entry too
        let Some(&(num_pid_parent, _)) = map.get(&num_pid_cursor) else { break };
        if num_pid_parent == 0 || num_pid_parent == num_pid_cursor { break; }
        let str_parent_exe = map.get(&num_pid_parent).map(|(_, n)| n.clone()).unwrap_or_else(|| "<gone>".to_string());

        let str_lower = str_parent_exe.to_ascii_lowercase();
        let flag_is_browser = VEC_BROWSER_EXES.iter().any(|s| *s == str_lower);
        info!("hop {}: cursor={} parent={} ({}) is_browser={}", num_hop, num_pid_cursor, num_pid_parent, str_parent_exe, flag_is_browser);

        if flag_is_browser {
            num_pid_browser = num_pid_parent;
        }
        num_pid_cursor = num_pid_parent;
    }

    info!("resolved browser_pid={}", num_pid_browser);
    num_pid_browser
}

// returns Ok(num_bytes_sent) so the caller can log throughput per frame at debug level
fn forward_frame(socket: &UdpSocket, vec_frame: &[u8], num_browser_pid: u32) -> anyhow::Result<usize> {
    if vec_frame.is_empty() { return Ok(0); }

    let mut value: serde_json::Value = serde_json::from_slice(vec_frame).context("parse extension json")?;
    if let serde_json::Value::Object(map) = &mut value {
        map.insert(STR_BROWSER_PID_KEY.to_string(), serde_json::Value::from(num_browser_pid));  // add parent pid info
        debug!("frame info: {:#?}", map);
    }
    let vec_out = serde_json::to_vec(&value).context("serialize augmented json")?;
    let num_sent = socket.send(&vec_out).context("udp send")?;
    Ok(num_sent)
}

pub fn run(str_udp_target: &str) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(STR_LOCAL_BIND_ADDR).with_context(|| format!("bind local udp socket at {}", STR_LOCAL_BIND_ADDR))?;
    socket.connect(str_udp_target).with_context(|| format!("connect udp socket to {}", str_udp_target))?;
    info!("udp socket bound at {} connected to {}", STR_LOCAL_BIND_ADDR, str_udp_target);

    // pid is constant over receiver's lifetime; resolve once. lookup_browser_pid walks the parent chain so
    // that the topmost browser ancestor wins (Chromium spawns native hosts under a utility subprocess).
    let num_browser_pid = lookup_browser_pid();

    let stdin = std::io::stdin();
    let mut handle_stdin = stdin.lock();

    info!("entering native messaging loop");
    let mut num_frames_ok: u64 = 0;
    loop {
        let opt_frame = read_frame(&mut handle_stdin).context("read native messaging frame")?;
        let Some(vec_frame) = opt_frame else {
            info!("stdin closed (browser ended port); frames forwarded = {}", num_frames_ok);
            return Ok(());
        };
        match forward_frame(&socket, &vec_frame, num_browser_pid) {
            Ok(num_sent) => {
                num_frames_ok += 1;
                debug!("forwarded frame: in={}B out={}B browser_pid={}", vec_frame.len(), num_sent, num_browser_pid);
            }
            Err(e) => warn!("forward frame failed: {:#}", e),
        }
    }
}

fn main() {
    // log path is anchored on the exe so browser-spawned and cmd-launched runs both land in the same file
    let path_log = std::env::current_exe().ok()
        .and_then(|p| p.parent().map(|d| d.join("web_receiver.log")));
    if let Some(path_log) = path_log {
        if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path_log) {
            let _ = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(Mutex::new(file))
                .with_ansi(false)
                .try_init();
        }
    }
    info!("web_receiver starting (pid={})", std::process::id());

    // any failure on the config path means the process can do nothing useful, so exit silently
    let Ok(path_exe) = std::env::current_exe() else {
        warn!("current_exe() failed; aborting");
        return;
    };
    info!("self exe path: {:?}", path_exe);
    let Some(dir_exe) = path_exe.parent() else {
        warn!("exe path has no parent; aborting");
        return;
    };
    let mut opt_cursor: Option<&Path> = Some(dir_exe);
    let mut opt_path_config = None;
    for _ in 0..NUM_MAX_PARENT_WALK {
        let Some(dir_now) = opt_cursor else { break };
        let path_candidate = dir_now.join("config.yaml");
        if path_candidate.is_file() {
            opt_path_config = Some(path_candidate);
            break;
        }
        opt_cursor = dir_now.parent();
    }
    let Some(path_config) = opt_path_config else {
        warn!("config.yaml not found above {:?}; aborting", dir_exe);
        return;
    };
    let Some(str_path_config) = path_config.to_str() else {
        warn!("config path is not utf-8: {:?}", path_config);
        return;
    };
    info!("loading config from {}", str_path_config);
    let config = match HgConfig::new(str_path_config) {
        Ok(c) => c,
        Err(e) => { warn!("HgConfig::new failed: {:#}", e); return; }
    };
    info!("config loaded: udp_target={}", config.web_receiver_udp_addr);

    // runtime::run blocks on stdin until the browser closes the port; either outcome terminates quietly
    match run(&config.web_receiver_udp_addr) {
        Ok(()) => info!("web_receiver exiting normally"),
        Err(e) => warn!("web_receiver exiting due to error: {:#}", e),
    }
}
