// Internal helpers for the hw module (sysfs files kept open, small reads).
#pragma once

#include <fcntl.h>
#include <unistd.h>

#include <cstdio>
#include <string>

namespace cs::hwutil {

// Reads a whole small file (sysfs/procfs); returns false on error.
inline bool read_file(const std::string &path, std::string &out) {
  int fd = ::open(path.c_str(), O_RDONLY | O_CLOEXEC);
  if (fd < 0) return false;
  out.clear();
  char buf[4096];
  ssize_t n;
  while ((n = ::read(fd, buf, sizeof buf)) > 0) out.append(buf, size_t(n));
  ::close(fd);
  return n == 0;
}

inline bool write_file(const std::string &path, const std::string &val) {
  int fd = ::open(path.c_str(), O_WRONLY | O_CLOEXEC);
  if (fd < 0) return false;
  bool ok = ::write(fd, val.data(), val.size()) == ssize_t(val.size());
  ::close(fd);
  return ok;
}

// Writes an integer to an already-open sysfs attribute (pwrite at offset 0).
inline bool write_int_fd(int fd, int v) {
  char b[16];
  int n = snprintf(b, sizeof b, "%d", v);
  return ::pwrite(fd, b, size_t(n), 0) == n;
}

inline std::string trim(std::string s) {
  while (!s.empty() && (s.back() == '\n' || s.back() == ' ' || s.back() == '\r')) s.pop_back();
  return s;
}

}  // namespace cs::hwutil
