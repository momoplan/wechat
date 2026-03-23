use crate::config::LoggingConfig;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::Layer;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_logging(logging_config: &LoggingConfig) -> Result<(), Box<dyn std::error::Error>> {
    let log_dir = if logging_config.log_dir.trim().is_empty() {
        "logs"
    } else {
        logging_config.log_dir.trim()
    };
    std::fs::create_dir_all(log_dir)?;

    let base_filter = EnvFilter::try_from_default_env()
        .or_else(|_| {
            let level = logging_config.level.trim();
            if level.is_empty() {
                EnvFilter::try_new("info")
            } else {
                EnvFilter::try_new(level)
            }
        })
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let filter_directive = base_filter.to_string();
    let build_filter =
        || EnvFilter::try_new(filter_directive.clone()).unwrap_or_else(|_| EnvFilter::new("info"));

    let file_appender = RollingFileAppender::new(Rotation::DAILY, log_dir, "wechat.log");

    let console_layer = tracing_subscriber::fmt::layer()
        .event_format(tracing_subscriber::fmt::format().json())
        .fmt_fields(tracing_subscriber::fmt::format::JsonFields::new())
        .with_ansi(false)
        .with_file(true)
        .with_line_number(true)
        .with_filter(build_filter());

    let file_layer = tracing_subscriber::fmt::layer()
        .event_format(tracing_subscriber::fmt::format().json())
        .fmt_fields(tracing_subscriber::fmt::format::JsonFields::new())
        .with_ansi(false)
        .with_file(true)
        .with_line_number(true)
        .with_writer(file_appender)
        .with_filter(build_filter());

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .try_init()?;

    Ok(())
}
