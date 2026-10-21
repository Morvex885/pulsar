// Minimal stderr logging with level filter (CS_LOG_LEVEL env: 0=debug..3=error).
#pragma once

#include <cstdio>
#include <cstring>
#include <ctime>

namespace cs {
int log_level();
void log_write(int level, const char *file, int line, const char *fmt, ...) __attribute__((format(printf, 4, 5)));
}  // namespace cs

#define LOGD(...) ::cs::log_write(0, __FILE__, __LINE__, __VA_ARGS__)
#define LOGI(...) ::cs::log_write(1, __FILE__, __LINE__, __VA_ARGS__)
#define LOGW(...) ::cs::log_write(2, __FILE__, __LINE__, __VA_ARGS__)
#define LOGE(...) ::cs::log_write(3, __FILE__, __LINE__, __VA_ARGS__)
