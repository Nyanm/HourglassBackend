/*
 Entry point for the web_receiver Native Messaging Host.

 Everything here is best-effort: logging opens a plain file in cwd,
 config is located by walking up from the executable path, and any
 failure ends the process silently with exit code 0. The runtime loop
 then reads stdin frames and forwards them via UDP.

 stdout is owned by the Native Messaging protocol and must never be
 written to from this binary; all diagnostics go to the log file.

 Runtime loop for the Native Messaging Host. Reads length-prefixed JSON
 frames from stdin (the channel the Chromium-family browser opened
 when the extension called connectNative), augments each frame with a
 local receive timestamp, and forwards the result as one UDP datagram
 per frame to the hourglass backend.

 stdin EOF is the canonical "browser closed the port" signal and is
 treated as a clean shutdown. Per-frame parse / send failures emit a
 single warn line and the loop continues so a transient bad frame
 cannot tear down the port. Protocol-level breakage (truncated frame,
 oversized length header) returns Err so the process exits.

 stdout is reserved for the Native Messaging protocol itself and is
 NEVER written to from here; warn lines go to the tracing subscriber
 set up by main.
 */

use std::io::Read;
use std::net::UdpSocket;
use std::path::Path;
use std::sync::Mutex;
use anyhow::{bail, Context};
use tracing::warn;

use hg_common::HgConfig;

// guard against runaway walks (symlink cycles) when searching upward for config.yaml
const NUM_MAX_PARENT_WALK: usize = 8;
// Native Messaging hard cap per Chromium spec; defensive guard against bogus length headers
const NUM_MAX_FRAME_BYTES: usize = 1024 * 1024;
// any free local port is fine since we only send; loopback bind keeps traffic local
const STR_LOCAL_BIND_ADDR: &str = "127.0.0.1:0";

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

pub fn run(str_udp_target: &str) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(STR_LOCAL_BIND_ADDR).with_context(|| format!("bind local udp socket at {}", STR_LOCAL_BIND_ADDR))?;
    socket.connect(str_udp_target).with_context(|| format!("connect udp socket to {}", str_udp_target))?;

    let stdin = std::io::stdin();
    let mut handle_stdin = stdin.lock();

    loop {
        let opt_frame = read_frame(&mut handle_stdin).context("read native messaging frame")?;
        let Some(vec_frame) = opt_frame else { return Ok(()); };  // EOF or broken frame
        if let Err(e) = socket.send(&vec_frame) { warn!("forward frame failed: {:#}", e); }
    }
}

fn main() {
    // best-effort plain file logging in cwd; if open or subscriber init fails we just run without logs
    if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open("web_receiver.log") {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(Mutex::new(file))
            .with_ansi(false)
            .try_init();
    }

    // any failure on the config path means the process can do nothing useful, so exit silently
    let Ok(path_exe) = std::env::current_exe() else { return };
    let Some(dir_exe) = path_exe.parent() else { return };
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
    let Some(path_config) = opt_path_config else { return };
    let Some(str_path_config) = path_config.to_str() else { return };
    let Ok(config) = HgConfig::new(str_path_config) else { return };

    // runtime::run blocks on stdin until the browser closes the port; either outcome terminates quietly
    let _ = run(&config.web_receiver_udp_addr);
}
