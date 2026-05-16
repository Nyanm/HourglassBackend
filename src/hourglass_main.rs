use std::sync::Arc;
use tracing::{info, Level};

use hg_common::{DbHandlerReader, DbHandlerWriter, HgConfig};
use hg_surveillant::UserTracker;

pub(crate) const CONFIG_PATH: &str = "config.yaml";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // load tracing logger
    let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("sot")
        .filename_suffix("log")
        .build("./log")?;
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let format = tracing_subscriber::fmt::format().with_level(true).with_target(true);
    tracing_subscriber::fmt()
        .with_max_level(Level::TRACE)
        .with_writer(non_blocking)
        .with_ansi(false)
        .event_format(format)
        .init();

    // load config
    let arc_config: Arc<HgConfig> = Arc::new(HgConfig::new(CONFIG_PATH)?);
    info!("deserialized config: {:?}", arc_config);

    // load internal components
    let arc_db_writer = Arc::new(DbHandlerWriter::new(Arc::clone(&arc_config)).expect("Failed to initialize database writer"));
    let arc_db_reader = Arc::new(DbHandlerReader::new(Arc::clone(&arc_config)).expect("Failed to initialize database reader"));
    let mut tracker = UserTracker::new(Arc::clone(&arc_config), Arc::clone(&arc_db_writer));

    // activative
    tokio::select!{
        _ = tracker.run() => { }
        _ = tokio::signal::ctrl_c() => { }
    }

    Ok(())
}
