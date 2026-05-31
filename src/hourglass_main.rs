use std::fmt;
use std::sync::{Arc, Mutex};
use chrono::Local;
use tracing::{Event, Subscriber, info, Level};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::registry::LookupSpan;

use hg_common::{DbHandlerReader, DbHandlerWriter, HgConfig, TrackerEvent};
use hg_surveillant::{ForegroundHook, IdleTicker, UserTracker, WebListener};

use tokio::sync::mpsc::unbounded_channel;

pub(crate) const CONFIG_PATH: &str = "config.yaml";

// log header: HH:MM:SS.mmm in local time, level, last module-path segment
struct CompactLocalFmt;
impl<S, N> FormatEvent<S, N> for CompactLocalFmt
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> fmt::Result {
        let metadata = event.metadata();

        // local time, hh:mm:ss.mmm
        write!(writer, "{} ", Local::now().format("%H:%M:%S%.3f"))?;

        // fixed-width level so different rows align visually
        write!(writer, "{:>5} ", metadata.level())?;

        // last segment of module path; e.g. "hg_surveillant::surveillant_tracker" -> "surveillant_tracker"
        let str_target = metadata.target();
        let str_short = str_target.rsplit("::").next().unwrap_or(str_target);
        write!(writer, "{}: ", str_short)?;

        // delegate field/message rendering to the default formatter
        ctx.format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // load tracing logger
    let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("hourglass")
        .filename_suffix("log")
        .build("./log")?;
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_writer(non_blocking)
        .with_ansi(false)
        .event_format(CompactLocalFmt)
        .init();

    // load config
    let arc_config: Arc<HgConfig> = Arc::new(HgConfig::new(CONFIG_PATH)?);
    info!("deserialized config: {:?}", arc_config);
    
    // mutex pid for web info update
    let arc_curr_pid: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));

    // event channel: winevent pump + idle ticker (producers) -> tracker actor (sole consumer)
    let (tx_event, rx_event) = unbounded_channel::<TrackerEvent>();

    // load internal components
    let arc_db_writer = Arc::new(DbHandlerWriter::new(Arc::clone(&arc_config)).expect("Failed to initialize database writer"));
    let arc_db_reader = Arc::new(DbHandlerReader::new(Arc::clone(&arc_config)).expect("Failed to initialize database reader"));
    let mut tracker = UserTracker::new(Arc::clone(&arc_config), Arc::clone(&arc_db_writer), Arc::clone(&arc_curr_pid), rx_event);
    let web_listener = WebListener::new(Arc::clone(&arc_config), Arc::clone(&arc_db_writer), Arc::clone(&arc_curr_pid));
    let idle_ticker = IdleTicker::new(Arc::clone(&arc_config), tx_event.clone());

    // install the foreground hook; kept alive until end of scope so its Drop unhooks + joins the pump thread
    let _foreground_hook = ForegroundHook::start(tx_event.clone()).expect("Failed to install foreground hook");

    // activative
    tokio::select!{
        _ = tracker.run() => { }
        _ = idle_ticker.run() => { }
        _ = web_listener.run() => { }
        _ = tokio::signal::ctrl_c() => { }
    }

    Ok(())
}
