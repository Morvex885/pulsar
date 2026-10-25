//! The installed logger end to end: the process-wide sink queue and its notification. One test
//! per binary, since the logger is global.

use std::sync::Arc;
use std::time::Duration;

use log::{Level, LevelFilter};
use pulsar_core::{init_logger, set_log_sink, stderr_level, take_log_lines};
use tokio::sync::Notify;

async fn notified(n: &Notify) -> bool {
    tokio::time::timeout(Duration::ZERO, n.notified())
        .await
        .is_ok()
}

#[tokio::test]
async fn sink_queue_and_notify() {
    init_logger();
    let notify = Arc::new(Notify::new());
    set_log_sink(Some(Level::Debug), Some(notify.clone()));
    // The sink's level opens the early filter even when stderr is less verbose.
    assert_eq!(log::max_level(), LevelFilter::Debug);
    // ... while stderr keeps PULSAR_LOG_LEVEL's (default Info).
    if std::env::var_os("PULSAR_LOG_LEVEL").is_none() {
        assert_eq!(stderr_level(), LevelFilter::Info);
    }

    // Debug keeps stderr quiet at the default level; targets must be pulsar's to reach the sink.
    let line = line!() + 1;
    log::debug!(target: "pulsar_test", "first {}", 1);
    assert!(notified(&notify).await);
    log::debug!(target: "pulsar_test", "second");
    assert!(!notified(&notify).await, "queue was already non-empty");
    log::trace!(target: "pulsar_test", "dropped");

    let lines = take_log_lines();
    let texts: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            format!("[D][log_sink.rs:{line}]: first 1"),
            format!("[D][log_sink.rs:{}]: second", line + 2)
        ]
    );
    assert!(lines.iter().all(|l| l.level == Level::Debug));

    log::debug!(target: "pulsar_test", "third");
    assert!(notified(&notify).await);

    set_log_sink(None, None);
    assert!(take_log_lines().is_empty());
    log::debug!(target: "pulsar_test", "not queued");
    assert!(take_log_lines().is_empty());
    assert!(!notified(&notify).await);
}
