#include "core/log.h"

#include <cstdarg>
#include <cstdlib>
#include <mutex>
#include <sys/time.h>

namespace cs {

int log_level() {
  static int lvl = [] {
    const char *e = getenv("CS_LOG_LEVEL");
    return e ? atoi(e) : 1;
  }();
  return lvl;
}

void log_write(int level, const char *file, int line, const char *fmt, ...) {
  if (level < log_level()) return;
  static std::mutex mu;
  static const char tags[] = "DIWE";
  timeval tv;
  gettimeofday(&tv, nullptr);
  tm t;
  localtime_r(&tv.tv_sec, &t);
  const char *base = strrchr(file, '/');
  base = base ? base + 1 : file;
  std::lock_guard<std::mutex> lk(mu);
  fprintf(stderr, "%02d:%02d:%02d.%03ld %c %s:%d ", t.tm_hour, t.tm_min, t.tm_sec, long(tv.tv_usec / 1000),
          tags[level & 3], base, line);
  va_list ap;
  va_start(ap, fmt);
  vfprintf(stderr, fmt, ap);
  va_end(ap);
  fputc('\n', stderr);
}

}  // namespace cs
