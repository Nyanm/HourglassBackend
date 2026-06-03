/*
 Pure unpacker for the loopback UDP channel fed by web_receiver. This module's only job is transport:
 receive a datagram, parse the self-contained JSON into WebReportInfo, and forward it to the tracker
 actor through the shared TrackerEvent channel. It no longer reads the foreground pid and no longer
 touches the database.

 Wire format is pure JSON: web_receiver parses the native messaging frame, injects the browser's pid
 into the top-level object as "browser_pid", and re-serialises. Each UDP datagram is therefore a single
 self-contained JSON object that tools like jq / wireshark can inspect without any custom framing.

 All policy now lives on the tracker actor (single owner of the segment state), which keeps the former
 cross-thread pid race from existing at all:
   1. focused filtering,
   2. browser_pid != 0 and equality against the actor's current foreground pid,
   3. empty-url skip and the fill / dedup / fork SQL.
 The six extension event tags (on_start, focus_gained, tab_activated, url_changed, page_loaded,
 tab_replaced) are forwarded verbatim; the tag is carried for diagnostics only.
 */

use hg_common::{HgConfig, TrackerEvent, WebReportInfo};

use std::sync::Arc;
use anyhow::Context;
use chrono::Utc;
use serde::Deserialize;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};


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
    tx_event: UnboundedSender<TrackerEvent>,  // forwards unpacked reports to the tracker actor
}

impl WebListener {
    pub fn new(arc_config: Arc<HgConfig>, tx_event: UnboundedSender<TrackerEvent>) -> Self {
        Self { arc_config, tx_event }
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

        let at_ms = Utc::now().timestamp_millis();
        let report = WebReportInfo {
            str_event: msg.event,
            is_focused: msg.focused,
            browser_pid: msg.browser_pid,
            opt_url: msg.url,
            opt_title: msg.title,
            at_ms,
        };
        debug!("unpacked web report: event={} focused={} pid={}", report.str_event, report.is_focused, report.browser_pid);

        self.tx_event.send(TrackerEvent::WebUpdate(report)).context("forward web update to tracker")?;
        Ok(())
    }
}
