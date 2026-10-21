#include "core/log.h"

#include <sys/time.h>

#include <atomic>
#include <cstdarg>
#include <cstdlib>
#include <mutex>

namespace cs {

namespace {

constexpr int kSinkOff = 4;
constexpr size_t kSinkQueueMax = 512;
// A LOGW in a 30 fps path would fill the 1 GB device's tmpfs log; identical consecutive lines
// are counted instead and summarised at most this often.
constexpr int64_t kRepeatReportMs = 30000;
const char kTags[] = "DIWE";

std::mutex mu;
std::atomic<int> sink_level{kSinkOff};
std::function<void()> sink_notify;  // guarded by mu
std::vector<LogLine> sink_queue;    // guarded by mu
size_t sink_dropped = 0;            // guarded by mu
thread_local bool in_notify = false;

struct Last {
  const char *file = nullptr;
  const char *base = "";
  int line = 0;
  int level = 0;
  std::string msg;
  unsigned repeats = 0;
  int64_t since_ms = 0;
};
Last last;  // guarded by mu

int64_t mono_ms() {
  timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return int64_t(ts.tv_sec) * 1000 + ts.tv_nsec / 1000000;
}

void emit_locked(int level, const char *base, int line, const char *msg) {
  if (level >= log_level()) {
    timeval tv;
    gettimeofday(&tv, nullptr);
    tm t;
    localtime_r(&tv.tv_sec, &t);
    fprintf(stderr, "%02d:%02d:%02d.%03ld %c %s:%d %s\n", t.tm_hour, t.tm_min, t.tm_sec, long(tv.tv_usec / 1000),
            kTags[level & 3], base, line, msg);
  }
  if (level < sink_level.load(std::memory_order_relaxed) || !sink_notify) return;
  const bool was_empty = sink_queue.empty();
  if (sink_queue.size() >= kSinkQueueMax) {
    sink_queue.erase(sink_queue.begin());
    sink_dropped++;
  }
  char head[96];
  snprintf(head, sizeof head, "[%c][%s:%d]: ", kTags[level & 3], base, line);
  sink_queue.push_back({level, std::string(head) + msg});
  if (was_empty) {
    in_notify = true;
    sink_notify();
    in_notify = false;
  }
}

void flush_repeats_locked() {
  if (!last.repeats) return;
  char buf[64];
  snprintf(buf, sizeof buf, "last message repeated %u times", last.repeats);
  emit_locked(last.level, last.base, last.line, buf);
  last.repeats = 0;
}

}  // namespace

int log_level() {
  static int lvl = [] {
    const char *e = getenv("PULSAR_LOG_LEVEL");
    return e ? atoi(e) : 1;
  }();
  return lvl;
}

void log_write(int level, const char *file, int line, const char *fmt, ...) {
  if (in_notify) return;  // the sink hand-off logged: dropping beats deadlocking on mu
  if (level < log_level() && level < sink_level.load(std::memory_order_relaxed)) return;
  char msg[1024];
  va_list ap;
  va_start(ap, fmt);
  vsnprintf(msg, sizeof msg, fmt, ap);
  va_end(ap);
  const char *base = strrchr(file, '/');
  base = base ? base + 1 : file;

  std::lock_guard<std::mutex> lk(mu);
  const int64_t now = mono_ms();
  if (file == last.file && line == last.line && level == last.level && last.msg == msg) {
    last.repeats++;
    if (now - last.since_ms >= kRepeatReportMs) {
      flush_repeats_locked();
      last.since_ms = now;
    }
    return;
  }
  flush_repeats_locked();
  last.file = file;
  last.base = base;
  last.line = line;
  last.level = level;
  last.msg = msg;
  last.since_ms = now;
  emit_locked(level, base, line, msg);
}

void log_set_sink(int min_level, std::function<void()> notify) {
  std::lock_guard<std::mutex> lk(mu);
  if (!notify) min_level = kSinkOff;
  sink_level.store(min_level, std::memory_order_relaxed);
  sink_notify = min_level < kSinkOff ? std::move(notify) : nullptr;
  if (!sink_notify) {
    sink_queue.clear();
    sink_dropped = 0;
  }
}

std::vector<LogLine> log_take() {
  std::lock_guard<std::mutex> lk(mu);
  std::vector<LogLine> out;
  out.swap(sink_queue);
  if (sink_dropped) {
    out.insert(out.begin(), {2, "[W][log.cpp]: " + std::to_string(sink_dropped) + " log lines dropped"});
    sink_dropped = 0;
  }
  return out;
}

}  // namespace cs
