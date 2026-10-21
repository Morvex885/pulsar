// Minimal stderr logging with level filter (PULSAR_LOG_LEVEL env: 0=debug..3=error), an optional
// sink queue for streaming log lines to API clients, and collapsing of identical repeats.
#pragma once

#include <cstdio>
#include <cstring>
#include <ctime>
#include <functional>
#include <string>
#include <vector>

namespace cs {
int log_level();
void log_write(int level, const char *file, int line, const char *fmt, ...) __attribute__((format(printf, 4, 5)));

// Lines at or above min_level (0..3; 4 = off) are queued for log_take(), independently of
// PULSAR_LOG_LEVEL. notify runs on whichever thread logged, under the log lock, when the queue
// becomes non-empty; it must only hand off (e.g. Loop::post) and must not log itself.
struct LogLine {
  int level;
  std::string text;  // "[W][file.cpp:12]: message"
};
void log_set_sink(int min_level, std::function<void()> notify);
std::vector<LogLine> log_take();
}  // namespace cs

#define LOGD(...) ::cs::log_write(0, __FILE__, __LINE__, __VA_ARGS__)
#define LOGI(...) ::cs::log_write(1, __FILE__, __LINE__, __VA_ARGS__)
#define LOGW(...) ::cs::log_write(2, __FILE__, __LINE__, __VA_ARGS__)
#define LOGE(...) ::cs::log_write(3, __FILE__, __LINE__, __VA_ARGS__)
