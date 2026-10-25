//! Set by the Makefile: `git describe --tags --match 'v*' --always --dirty` (release tags only)
//! and HEAD's commit date.

/// `pulsar --version`, Home Assistant DeviceInfo and the "Version" entity; the updater compares
/// it with the release tag.
pub const VERSION: &str = match option_env!("PULSAR_VERSION") {
    Some(v) => v,
    None => "unknown",
};

/// "Mmm dd yyyy, hh:mm:ss" (C++ `__DATE__ ", " __TIME__`), HA DeviceInfo compilation_time.
pub const BUILD_TIME: &str = match option_env!("PULSAR_BUILD_TIME") {
    Some(v) => v,
    None => "unknown",
};
