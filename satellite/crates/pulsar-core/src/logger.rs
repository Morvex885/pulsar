//! `log` backend with the C++ daemon's behaviour: stderr lines "HH:MM:SS.mmm L file:line msg",
//! collapsing of identical consecutive records, and a bounded sink queue that the ESPHome
//! server streams to API clients (SubscribeLogs).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};
use tokio::sync::Notify;

use crate::sys;

/// The C++ daemon formatted into char[1024]: at most 1023 bytes of message survive.
const MSG_MAX: usize = 1023;
/// API clients that do not read fast enough lose the oldest lines.
const SINK_QUEUE_MAX: usize = 512;
/// A warning in a 30 fps path would fill the device's tmpfs log (`/tmp` is RAM); identical
/// consecutive records are counted instead and summarised at most this often.
const REPEAT_REPORT_MS: i64 = 30_000;
/// C++ level numbering (PULSAR_LOG_LEVEL): 0 = debug .. 3 = error; 4 = sink off.
const SINK_OFF: i32 = 4;
const TAGS: [char; 4] = ['D', 'I', 'W', 'E'];
const WARN: i32 = 2;

/// One line queued for API clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub level: Level,
    /// "[W][file.rs:12]: message"
    pub text: String,
}

/// Installs the logger. PULSAR_LOG_LEVEL (atoi: 0 = debug, 1 = info (default), 2 = warn,
/// 3 = error) filters stderr only; the sink has its own level.
pub fn init_logger() {
    let level = std::env::var("PULSAR_LOG_LEVEL").map_or(1, |v| atoi(&v));
    let mut g = global();
    g.core.stderr_level = level;
    update_max_level(&g.core);
    drop(g);
    // A second call keeps the first logger; the level above is updated either way.
    let _ = log::set_logger(&LOGGER);
}

/// Queues records at `min_level` and above for `take_log_lines`, independently of the stderr
/// level; `notify` fires when the queue goes from empty to non-empty. Either None turns the
/// sink off and discards queued lines and the dropped count.
pub fn set_log_sink(min_level: Option<Level>, notify: Option<Arc<Notify>>) {
    let mut g = global();
    g.core.set_sink(min_level, notify);
    update_max_level(&g.core);
}

/// Treats records whose target is `prefix` or lies under it (`prefix::…`) as pulsar's own, like
/// `pulsar*` targets: stderr at PULSAR_LOG_LEVEL and the sink. Tools call it with
/// `module_path!()` (their crate name does not start with "pulsar"); before or after
/// `init_logger`.
pub fn add_own_target(prefix: &'static str) {
    let mut own = OWN_TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    if !own.contains(&prefix) {
        own.push(prefix);
    }
}

/// Stderr's level from PULSAR_LOG_LEVEL (Info until `init_logger`), independent of the sink.
pub fn stderr_level() -> LevelFilter {
    filter_of(global().core.stderr_level)
}

/// Takes the queued lines; after an overflow the first one reports how many were dropped.
pub fn take_log_lines() -> Vec<LogLine> {
    global().core.take()
}

struct Global {
    core: Core,
    line: String,
}

static GLOBAL: Mutex<Global> = Mutex::new(Global {
    core: Core::new(1),
    line: String::new(),
});
static LOGGER: Logger = Logger;
/// Prefixes of `add_own_target`.
static OWN_TARGETS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

thread_local! {
    // Set while the sink's Notify runs on this thread: a record from there is dropped instead
    // of re-entering the logger.
    static IN_NOTIFY: Cell<bool> = const { Cell::new(false) };
    static MSG_BUF: RefCell<String> = const { RefCell::new(String::new()) };
}

fn global() -> MutexGuard<'static, Global> {
    GLOBAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// log's max level is the more verbose of stderr and sink, so disabled records are dropped in
/// the macro before formatting.
fn update_max_level(core: &Core) {
    log::set_max_level(filter_of(core.stderr_level.min(core.sink_level)));
}

fn filter_of(num: i32) -> LevelFilter {
    match num {
        i32::MIN..=0 => LevelFilter::Debug,
        1 => LevelFilter::Info,
        2 => LevelFilter::Warn,
        3 => LevelFilter::Error,
        _ => LevelFilter::Off,
    }
}

fn level_num(level: Level) -> i32 {
    match level {
        Level::Error => 3,
        Level::Warn => 2,
        Level::Info => 1,
        Level::Debug | Level::Trace => 0,
    }
}

fn level_of(num: i32) -> Level {
    match num {
        3 => Level::Error,
        2 => Level::Warn,
        1 => Level::Info,
        _ => Level::Debug,
    }
}

/// Records of other crates (rustls, tokio, …) are not pulsar's messages: stderr only.
fn is_foreign(target: &str) -> bool {
    if target.starts_with("pulsar") {
        return false;
    }
    let own = OWN_TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    !own.iter().any(|p| {
        target
            .strip_prefix(p)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

fn basename(file: &str) -> &str {
    file.rsplit('/').next().unwrap_or(file)
}

/// C atoi: optional white space and sign, then digits; anything else ends the number.
fn atoi(s: &str) -> i32 {
    let s = s.trim_start();
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut v: i32 = 0;
    for b in digits.bytes().take_while(u8::is_ascii_digit) {
        v = v.wrapping_mul(10).wrapping_add(i32::from(b - b'0'));
    }
    if neg { v.wrapping_neg() } else { v }
}

/// "HH:MM:SS.mmm L file:line msg\n"
fn format_line(
    buf: &mut String,
    hms: (i32, i32, i32),
    millis: u32,
    tag: char,
    base: &str,
    line: u32,
    msg: &str,
) {
    buf.clear();
    let (h, m, s) = hms;
    let _ = writeln!(
        buf,
        "{h:02}:{m:02}:{s:02}.{millis:03} {tag} {base}:{line} {msg}"
    );
}

/// Formats at most `MSG_MAX` bytes, cut at a character boundary; stops the formatter early so a
/// huge argument costs no more than the limit.
struct Capped<'a>(&'a mut String);

impl std::fmt::Write for Capped<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let room = MSG_MAX - self.0.len();
        if s.len() <= room {
            self.0.push_str(s);
            return Ok(());
        }
        let mut end = room;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push_str(&s[..end]);
        Err(std::fmt::Error)
    }
}

fn format_msg(buf: &mut String, args: std::fmt::Arguments<'_>) {
    buf.clear();
    let _ = std::fmt::write(&mut Capped(buf), args);
}

/// Where stderr lines go; the process writes them with a wall-clock time stamp, tests record
/// them.
trait Stderr {
    fn line(&mut self, level: i32, base: &str, line: u32, msg: &str);
}

struct ProcessStderr<'a>(&'a mut String);

impl Stderr for ProcessStderr<'_> {
    fn line(&mut self, level: i32, base: &str, line: u32, msg: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = i64::try_from(now.as_secs()).unwrap_or(0);
        let tag = TAGS[(level & 3) as usize];
        format_line(
            self.0,
            sys::local_hms(secs),
            now.subsec_millis(),
            tag,
            base,
            line,
            msg,
        );
        // Raw write(2), not std's stderr lock: this runs under GLOBAL, and code holding std's
        // stderr lock while logging would otherwise deadlock on the lock order.
        sys::write_stderr(self.0.as_bytes());
    }
}

/// The last record, for collapsing repeats.
#[derive(Default)]
struct Last {
    file: String,
    line: u32,
    level: i32,
    msg: String,
    to_sink: bool,
    repeats: u32,
    since_ms: i64,
}

/// Logger state without global side effects; time comes in as a parameter.
struct Core {
    stderr_level: i32,
    sink_level: i32,
    sink_notify: Option<Arc<Notify>>,
    sink_queue: VecDeque<LogLine>,
    sink_dropped: usize,
    last: Option<Last>,
}

impl Core {
    const fn new(stderr_level: i32) -> Self {
        Self {
            stderr_level,
            sink_level: SINK_OFF,
            sink_notify: None,
            sink_queue: VecDeque::new(),
            sink_dropped: 0,
            last: None,
        }
    }

    fn set_sink(&mut self, min_level: Option<Level>, notify: Option<Arc<Notify>>) {
        match (min_level, notify) {
            (Some(level), Some(notify)) => {
                self.sink_level = level_num(level);
                self.sink_notify = Some(notify);
            }
            _ => {
                self.sink_level = SINK_OFF;
                self.sink_notify = None;
                self.sink_queue.clear();
                self.sink_dropped = 0;
            }
        }
    }

    fn take(&mut self) -> Vec<LogLine> {
        let mut out = Vec::with_capacity(self.sink_queue.len() + 1);
        if self.sink_dropped > 0 {
            out.push(LogLine {
                level: Level::Warn,
                text: format!("[W][log.rs]: {} log lines dropped", self.sink_dropped),
            });
            self.sink_dropped = 0;
        }
        out.extend(self.sink_queue.drain(..));
        out
    }

    /// Whether a record passes the early filter (stderr or sink would take it).
    fn wanted(&self, level: i32, foreign: bool) -> bool {
        if foreign {
            level >= WARN && level >= self.stderr_level
        } else {
            level >= self.stderr_level || level >= self.sink_level
        }
    }

    /// Logs one record; returns true when the sink queue went from empty to non-empty.
    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        now_ms: i64,
        level: i32,
        file: &str,
        line: u32,
        msg: &str,
        foreign: bool,
        out: &mut dyn Stderr,
    ) -> bool {
        if !self.wanted(level, foreign) {
            return false;
        }
        let mut notify = false;
        if let Some(last) = &mut self.last
            && last.file == file
            && last.line == line
            && last.level == level
            && last.msg == msg
        {
            last.repeats += 1;
            if now_ms - last.since_ms >= REPEAT_REPORT_MS {
                notify |= self.flush_repeats(out);
                if let Some(last) = &mut self.last {
                    last.since_ms = now_ms;
                }
            }
            return notify;
        }
        notify |= self.flush_repeats(out);
        let last = self.last.get_or_insert_with(Last::default);
        last.file.clear();
        last.file.push_str(file);
        last.line = line;
        last.level = level;
        last.msg.clear();
        last.msg.push_str(msg);
        last.to_sink = !foreign;
        last.since_ms = now_ms;
        notify | self.emit(level, basename(file), line, msg, !foreign, out)
    }

    fn flush_repeats(&mut self, out: &mut dyn Stderr) -> bool {
        let Some(last) = self.last.take() else {
            return false;
        };
        let mut notify = false;
        if last.repeats > 0 {
            let msg = format!("last message repeated {} times", last.repeats);
            notify = self.emit(
                last.level,
                basename(&last.file),
                last.line,
                &msg,
                last.to_sink,
                out,
            );
        }
        self.last = Some(Last { repeats: 0, ..last });
        notify
    }

    fn emit(
        &mut self,
        level: i32,
        base: &str,
        line: u32,
        msg: &str,
        to_sink: bool,
        out: &mut dyn Stderr,
    ) -> bool {
        if level >= self.stderr_level {
            out.line(level, base, line, msg);
        }
        if !to_sink || level < self.sink_level || self.sink_notify.is_none() {
            return false;
        }
        let was_empty = self.sink_queue.is_empty();
        if self.sink_queue.len() >= SINK_QUEUE_MAX {
            self.sink_queue.pop_front();
            self.sink_dropped += 1;
        }
        let tag = TAGS[(level & 3) as usize];
        self.sink_queue.push_back(LogLine {
            level: level_of(level),
            text: format!("[{tag}][{base}:{line}]: {msg}"),
        });
        was_empty
    }
}

struct Logger;

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        let level = metadata.level();
        level != Level::Trace
            && level <= log::max_level()
            && (!is_foreign(metadata.target()) || level <= Level::Warn)
    }

    fn log(&self, record: &Record<'_>) {
        if IN_NOTIFY.get() || record.level() == Level::Trace {
            return;
        }
        let level = level_num(record.level());
        let foreign = is_foreign(record.target());
        if foreign && level < WARN {
            return;
        }
        let file = record.file().unwrap_or(record.target());
        let line = record.line().unwrap_or(0);
        // Formatting runs user Display impls outside the lock. One that logs itself, or a record
        // during thread teardown, gets a fresh buffer.
        let write = |buf: &mut String| write_record(buf, record.args(), level, file, line, foreign);
        let mut notify = None;
        let buffered = MSG_BUF
            .try_with(|cell| {
                cell.try_borrow_mut()
                    .map(|mut buf| notify = write(&mut buf))
                    .is_ok()
            })
            .unwrap_or(false);
        if !buffered {
            notify = write(&mut String::new());
        }
        if let Some(notify) = notify {
            IN_NOTIFY.set(true);
            notify.notify_one();
            IN_NOTIFY.set(false);
        }
    }

    fn flush(&self) {}
}

fn write_record(
    buf: &mut String,
    args: &std::fmt::Arguments<'_>,
    level: i32,
    file: &str,
    line: u32,
    foreign: bool,
) -> Option<Arc<Notify>> {
    format_msg(buf, *args);
    let mut g = global();
    let g = &mut *g;
    let now = sys::monotonic_ms();
    if g.core.write(
        now,
        level,
        file,
        line,
        buf,
        foreign,
        &mut ProcessStderr(&mut g.line),
    ) {
        g.core.sink_notify.clone()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Lines(Vec<String>);

    impl Stderr for Lines {
        fn line(&mut self, level: i32, base: &str, line: u32, msg: &str) {
            self.0
                .push(format!("{} {base}:{line} {msg}", TAGS[level as usize]));
        }
    }

    fn sink_core(stderr_level: i32, sink: Level) -> Core {
        let mut core = Core::new(stderr_level);
        core.set_sink(Some(sink), Some(Arc::new(Notify::new())));
        core
    }

    #[test]
    fn stderr_line_format() {
        let mut buf = String::new();
        format_line(&mut buf, (7, 3, 9), 5, 'W', "leds.rs", 42, "fb open failed");
        assert_eq!(buf, "07:03:09.005 W leds.rs:42 fb open failed\n");
        format_line(&mut buf, (23, 59, 59), 999, 'E', "x.rs", 0, "");
        assert_eq!(buf, "23:59:59.999 E x.rs:0 \n");
    }

    #[test]
    fn message_truncated_to_1023_bytes() {
        let mut buf = String::new();
        let long = "x".repeat(5000);
        format_msg(&mut buf, format_args!("a{long}"));
        assert_eq!(buf.len(), MSG_MAX);
        assert!(buf.starts_with("ax"));

        // A multi-byte character crossing the limit is dropped whole.
        let wide = "я".repeat(600);
        format_msg(&mut buf, format_args!("{wide}"));
        assert_eq!(buf.len(), 1022);
        assert!(buf.chars().all(|c| c == 'я'));

        format_msg(&mut buf, format_args!("short {}", 1));
        assert_eq!(buf, "short 1");
    }

    #[test]
    fn file_is_basename_and_levels_filter_stderr() {
        let mut core = Core::new(1);
        let mut out = Lines::default();
        core.write(
            0,
            0,
            "crates/pulsar-hw/src/leds.rs",
            10,
            "debug",
            false,
            &mut out,
        );
        core.write(
            0,
            2,
            "crates/pulsar-hw/src/leds.rs",
            11,
            "warn",
            false,
            &mut out,
        );
        assert_eq!(out.0, ["W leds.rs:11 warn"]);
    }

    #[test]
    fn repeats_collapse_and_report_every_30s() {
        let mut core = Core::new(0);
        let mut out = Lines::default();
        let f = "src/a.rs";
        core.write(0, 2, f, 1, "busy", false, &mut out);
        core.write(1_000, 2, f, 1, "busy", false, &mut out);
        core.write(29_999, 2, f, 1, "busy", false, &mut out);
        assert_eq!(out.0, ["W a.rs:1 busy"]);

        core.write(30_000, 2, f, 1, "busy", false, &mut out);
        assert_eq!(out.0[1..], ["W a.rs:1 last message repeated 3 times"]);

        // The next report window starts at the report.
        core.write(59_999, 2, f, 1, "busy", false, &mut out);
        assert_eq!(out.0.len(), 2);

        // Another record flushes the pending count first.
        core.write(60_000, 2, f, 2, "busy", false, &mut out);
        assert_eq!(
            out.0[2..],
            ["W a.rs:1 last message repeated 1 times", "W a.rs:2 busy"]
        );

        // Same text at another level is a different record, and nothing was pending.
        core.write(60_001, 3, f, 2, "busy", false, &mut out);
        assert_eq!(out.0[4..], ["E a.rs:2 busy"]);
    }

    #[test]
    fn repeat_report_reaches_the_sink() {
        let mut core = sink_core(4, Level::Info);
        let mut out = Lines::default();
        core.write(0, 1, "a.rs", 5, "tick", false, &mut out);
        core.write(10, 1, "a.rs", 5, "tick", false, &mut out);
        core.write(20, 1, "a.rs", 6, "tock", false, &mut out);
        assert!(out.0.is_empty());
        let texts: Vec<_> = core.take().into_iter().map(|l| l.text).collect();
        assert_eq!(
            texts,
            [
                "[I][a.rs:5]: tick",
                "[I][a.rs:5]: last message repeated 1 times",
                "[I][a.rs:6]: tock"
            ]
        );
    }

    #[test]
    fn sink_levels_and_foreign_records() {
        let mut core = sink_core(1, Level::Warn);
        let mut out = Lines::default();
        core.write(0, 1, "a.rs", 1, "info", false, &mut out);
        core.write(0, 3, "a.rs", 2, "error", false, &mut out);
        // Foreign records: stderr only, and only from Warn up.
        core.write(0, 1, "rustls/src/conn.rs", 3, "info", true, &mut out);
        core.write(0, 2, "rustls/src/conn.rs", 4, "warn", true, &mut out);
        assert_eq!(
            out.0,
            ["I a.rs:1 info", "E a.rs:2 error", "W conn.rs:4 warn"]
        );
        assert_eq!(
            core.take(),
            [LogLine {
                level: Level::Error,
                text: "[E][a.rs:2]: error".into()
            }]
        );

        // Debug reaches the sink even when stderr is at Error.
        let mut core = sink_core(3, Level::Debug);
        core.write(0, 0, "a.rs", 1, "dbg", false, &mut out);
        assert_eq!(
            core.take(),
            [LogLine {
                level: Level::Debug,
                text: "[D][a.rs:1]: dbg".into()
            }]
        );
        assert!(core.wanted(0, false));
        assert!(!core.wanted(2, true));
    }

    #[test]
    fn sink_overflow_and_notify_on_transition() {
        let mut core = sink_core(4, Level::Info);
        let mut out = Lines::default();
        let mut notified = 0;
        for i in 0..600 {
            if core.write(0, 1, "a.rs", 1, &format!("line {i}"), false, &mut out) {
                notified += 1;
            }
        }
        assert_eq!(notified, 1);
        let lines = core.take();
        assert_eq!(lines.len(), 513);
        assert_eq!(
            lines[0],
            LogLine {
                level: Level::Warn,
                text: "[W][log.rs]: 88 log lines dropped".into()
            }
        );
        assert_eq!(lines[1].text, "[I][a.rs:1]: line 88");
        assert_eq!(lines[512].text, "[I][a.rs:1]: line 599");

        assert!(core.take().is_empty());
        assert!(core.write(0, 1, "a.rs", 1, "again", false, &mut out));
        assert!(!core.write(0, 1, "a.rs", 1, "more", false, &mut out));
    }

    #[test]
    fn sink_off_discards_queue_and_dropped_count() {
        let mut core = sink_core(4, Level::Info);
        let mut out = Lines::default();
        for i in 0..520 {
            core.write(0, 1, "a.rs", 1, &format!("{i}"), false, &mut out);
        }
        core.set_sink(Some(Level::Info), None);
        assert!(core.take().is_empty());
        assert!(!core.write(0, 1, "a.rs", 1, "x", false, &mut out));
        assert!(core.take().is_empty());
    }

    #[test]
    fn own_targets_are_not_foreign() {
        assert!(is_foreign("wp_own_target_probe"));
        assert!(!is_foreign("pulsar_zigbee::link"));
        add_own_target("wp_own_target_probe");
        add_own_target("wp_own_target_probe");
        assert!(!is_foreign("wp_own_target_probe"));
        assert!(!is_foreign("wp_own_target_probe::sub"));
        assert!(is_foreign("wp_own_target_probex"));
        assert!(is_foreign("rustls::client"));
        assert_eq!(
            OWN_TARGETS
                .lock()
                .unwrap()
                .iter()
                .filter(|p| **p == "wp_own_target_probe")
                .count(),
            1
        );
    }

    #[test]
    fn atoi_like_c() {
        assert_eq!(atoi("2"), 2);
        assert_eq!(atoi(" 3\n"), 3);
        assert_eq!(atoi("-1"), -1);
        assert_eq!(atoi("abc"), 0);
        assert_eq!(atoi(""), 0);
    }
}
