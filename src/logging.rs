use std::fs::OpenOptions;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, Layer};

const LOG_FILE: &str = "gtui_debug.log";

/// Initialize the tracing subscriber. When `debug` is true, logs are written
/// to a file; otherwise tracing is effectively disabled.
pub fn init(debug: bool) {
    if !debug {
        return;
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_FILE)
        .expect("failed to open log file");

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(file)
                .with_ansi(false)
                .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG),
        )
        .init();
}
