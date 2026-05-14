use std::sync::{atomic, Arc};
use tracing::{info, Level};

use hg_common::{DbHandler, HgConfig};

pub(crate) const CONFIG_PATH: &str = "config.yaml";

fn main() -> anyhow::Result<()> {
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

    // capture system signal
    let abool_running = Arc::new(atomic::AtomicBool::new(true));
    let _abool_running = Arc::clone(&abool_running);
    ctrlc::set_handler(move || {_abool_running.store(false, atomic::Ordering::SeqCst)})?;

    // load internal components
    let arc_db_handler = Arc::new(DbHandler::new(Arc::clone(&arc_config)).expect("Failed to initialize database"));
    

    Ok(())
}
