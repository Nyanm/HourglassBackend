/*
 Listens on a loopback UDP socket for messages forwarded by web_receiver
 and writes the current page's url + tab title onto the in-progress
 usage segment.

 Wire format is pure JSON: web_receiver parses the native messaging
 frame, injects the browser's pid into the top-level object as
 "browser_pid", and re-serialises. Each UDP datagram is therefore a
 single self-contained JSON object that tools like jq / wireshark can
 inspect without any custom framing knowledge.

 Match policy:
   1. JSON must parse into the minimal expected shape; unknown extra
      fields are tolerated for forward compatibility with future
      extension versions.
   2. The browser_pid from the message must be non-zero (zero means
      web_receiver's parent-process lookup failed and the message has
      no verifiable origin).
   3. browser_pid must equal the pid the tracker most recently
      published into the shared snapshot. This guards against late
      packets from a browser that is no longer in the foreground.
   4. The SQL UPDATE itself also re-asserts the pid, defending against
      a tracker tick that lands between our match check and the
      database call.

 All six event tags emitted by the extension (on_start, focus_gained,
 tab_activated, url_changed, page_loaded, tab_replaced) are treated
 the same: any of them triggers an UPDATE. The event field is logged
 for diagnostics but does not gate the write.
 */

use std::sync::{Arc, Mutex};
use anyhow::Context;
use serde::Deserialize;
use tokio::net::UdpSocket;
use tracing::{debug, info, trace, warn};

use hg_common::{DbHandlerWriter, HgConfig};


// native messaging frames are capped at 1 MiB but real ones are far smaller
const NUM_RECV_BUF_BYTES: usize = 16 * 1024;


#[derive(Debug, Deserialize)]
struct WebReceiverMessage {
    event: String,
    focused: bool,
    browser_pid: u32,  // parent pid of web_receiver
    url: Option<String>,
    title: Option<String>,
}

#[derive(Debug)]
pub struct WebListener {
    arc_config: Arc<HgConfig>,
    arc_db_handler: Arc<DbHandlerWriter>,
    arc_curr_pid: Arc<Mutex<u32>>,  // 0 is the agreed sentinel
}

impl WebListener {
    pub fn new(arc_config: Arc<HgConfig>, arc_db_handler: Arc<DbHandlerWriter>, arc_curr_pid: Arc<Mutex<u32>>) -> Self {
        Self { arc_config, arc_db_handler, arc_curr_pid }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let str_bind = self.arc_config.web_receiver_udp_addr.as_str();
        let socket = UdpSocket::bind(str_bind).await
            .with_context(|| format!("bind udp socket at {}", str_bind))?;
        info!("web listener bound to {}", str_bind);

        let mut buf = vec![0u8; NUM_RECV_BUF_BYTES];
        loop {
            let (num_bytes, _addr) = socket.recv_from(&mut buf).await.context("udp recv")?;
            if let Err(e) = self.handle_packet(&buf[..num_bytes]) {
                warn!("handle udp packet failed: {:#}", e);  // eat the error
            }
        }
    }

    fn handle_packet(&self, packet: &[u8]) -> anyhow::Result<()> {
        let msg: WebReceiverMessage = serde_json::from_slice(packet).context("parse udp json")?;
        if !msg.focused { debug!("ignored: focused=false"); return Ok(()); }

        let num_curr_pid = *self.arc_curr_pid.lock().unwrap_or_else(|p| p.into_inner());
        if msg.browser_pid == 0 || num_curr_pid != msg.browser_pid {  // fail to query parent pid or incompatible pid (not current foreground)
            debug!("pid mismatch: msg={} curr={}, drop", msg.browser_pid, num_curr_pid);
            return Ok(());
        }

        let str_url = msg.url.unwrap_or_default();
        let str_title = msg.title.unwrap_or_default();

        let flag_applied = self.arc_db_handler.update_segment_web(&str_url, &str_title, msg.browser_pid)?;
        if !flag_applied { debug!("update affected 0 rows after race"); }

        debug!("update web info: event={}, url={}, title={}", msg.event, str_url, str_title);
        Ok(())
    }
}
