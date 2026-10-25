//! Tools' own log targets end to end: records of a target registered with `add_own_target`
//! (before or after `init_logger`) reach the sink like pulsar's; others stay foreign. One test
//! per binary, since the logger is global.

use log::Level;
use pulsar_core::{add_own_target, init_logger, set_log_sink, take_log_lines};

#[test]
fn own_targets_reach_the_sink() {
    add_own_target("early_tool");
    init_logger();
    add_own_target("late_tool");
    // The sink is on only with a notify.
    let notify = std::sync::Arc::new(tokio::sync::Notify::new());
    set_log_sink(Some(Level::Debug), Some(notify));

    log::info!(target: "other_tool", "foreign info");
    log::info!(target: "early_tool", "early");
    log::debug!(target: "late_tool::sub", "late");
    log::info!(target: "late_toolbox", "foreign too");
    let texts: Vec<_> = take_log_lines().into_iter().map(|l| l.text).collect();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(texts[0].ends_with("]: early"), "{texts:?}");
    assert!(texts[1].ends_with("]: late"), "{texts:?}");
}
