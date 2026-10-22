#include "update/updater.h"

#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <spawn.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <cctype>
#include <cstdio>
#include <cstring>
#include <sstream>

#include "core/log.h"
#include "update/https.h"

extern char **environ;

namespace cs {
namespace {

constexpr size_t kMaxBinary = 64u << 20;
constexpr size_t kMaxSmall = 1u << 20;
constexpr int kVersionTimeoutMs = 10000;

// Tag names go into URLs and file contents: plain characters only.
bool safe_tag(const std::string &t) {
  if (t.empty() || t.size() > 64) return false;
  for (char c : t)
    if (!isalnum(static_cast<unsigned char>(c)) && c != '.' && c != '-' && c != '_' && c != '+') return false;
  return true;
}

bool write_file(const std::string &path, const std::string &data, mode_t mode) {
  int fd = open(path.c_str(), O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, mode);
  if (fd < 0) return false;
  size_t off = 0;
  while (off < data.size()) {
    ssize_t n = write(fd, data.data() + off, data.size() - off);
    if (n <= 0) {
      close(fd);
      return false;
    }
    off += size_t(n);
  }
  bool ok = fsync(fd) == 0 && fchmod(fd, mode) == 0;
  return close(fd) == 0 && ok;
}

bool read_file(const std::string &path, std::string &out) {
  FILE *f = fopen(path.c_str(), "rb");
  if (!f) return false;
  char buf[65536];
  size_t n;
  while ((n = fread(buf, 1, sizeof buf, f)) > 0) out.append(buf, n);
  bool ok = !ferror(f);
  fclose(f);
  return ok;
}

// Output of `<path> --version` (first line), "" if it did not run within the timeout.
std::string run_version(const std::string &path) {
  int p[2];
  if (pipe2(p, O_CLOEXEC) != 0) return "";
  posix_spawn_file_actions_t fa;
  posix_spawn_file_actions_init(&fa);
  posix_spawn_file_actions_adddup2(&fa, p[1], 1);
  posix_spawnattr_t attr;
  posix_spawnattr_init(&attr);
  sigset_t none;
  sigemptyset(&none);
  posix_spawnattr_setsigmask(&attr, &none);  // the daemon blocks SIGTERM in every thread
  posix_spawnattr_setflags(&attr, POSIX_SPAWN_SETSIGMASK);
  char *argv[] = {const_cast<char *>(path.c_str()), const_cast<char *>("--version"), nullptr};
  pid_t pid = -1;
  int rc = posix_spawn(&pid, path.c_str(), &fa, &attr, argv, environ);
  posix_spawn_file_actions_destroy(&fa);
  posix_spawnattr_destroy(&attr);
  close(p[1]);
  std::string out;
  if (rc == 0) {
    const int64_t deadline = Loop::now_ms() + kVersionTimeoutMs;
    auto left = [&] { return int(std::max<int64_t>(0, deadline - Loop::now_ms())); };
    pollfd pf{p[0], POLLIN, 0};
    char buf[256];
    while (out.size() < 1024 && poll(&pf, 1, left()) == 1) {
      ssize_t n = read(p[0], buf, sizeof buf);
      if (n <= 0) break;
      out.append(buf, size_t(n));
    }
    // EOF comes while the child is still exiting: wait for it within the same deadline.
    int st = 0;
    pid_t w;
    while ((w = waitpid(pid, &st, WNOHANG)) == 0 && left() > 0) usleep(10000);
    if (w != pid) {
      kill(pid, SIGKILL);
      waitpid(pid, &st, 0);
      out.clear();
    } else if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) {
      out.clear();
    }
  }
  close(p[0]);
  return out.substr(0, out.find('\n'));
}

// The hex digest for `name` in a sha256sum listing ("<hex>  <name>" or "<hex> *<name>").
std::string expected_sha(const std::string &sums, const std::string &name) {
  std::istringstream in(sums);
  std::string line;
  while (std::getline(in, line)) {
    if (!line.empty() && line.back() == '\r') line.pop_back();
    size_t sp = line.find(' ');
    if (sp != 64) continue;
    size_t name_at = line.find_first_not_of(' ', sp);
    if (name_at == std::string::npos) continue;
    std::string file = line.substr(name_at);
    if (!file.empty() && file[0] == '*') file.erase(0, 1);
    if (file == name) return line.substr(0, 64);
  }
  return "";
}

}  // namespace

bool parse_version(const std::string &s, Version &out) {
  const char *p = s.c_str();
  if (*p == 'v') p++;
  int n = 0;
  if (sscanf(p, "%d.%d.%d%n", &out.v[0], &out.v[1], &out.v[2], &n) != 3) return false;
  out.exact = p[n] == '\0';
  return out.exact || p[n] == '-';
}

bool release_newer(const std::string &latest, const std::string &current, bool *current_known) {
  Version l, c;
  bool known = parse_version(current, c);
  if (current_known) *current_known = known;
  if (!parse_version(latest, l)) return false;
  if (!known) return latest != current;
  for (int i = 0; i < 3; i++)
    if (l.v[i] != c.v[i]) return l.v[i] > c.v[i];
  return false;  // the same release, or a build after it
}

const char *Updater::arch() {
#if defined(__aarch64__)
  return "aarch64";
#elif defined(__arm__)
  return "armhf";
#else
  return "host";
#endif
}

Updater::Updater(Loop &loop, std::string repo, std::string current_version)
    : loop_(loop), repo_(std::move(repo)), user_agent_("User-Agent: pulsar/" + current_version) {}

Updater::~Updater() {
  cancel_ = true;
  if (worker_.joinable()) worker_.join();
}

void Updater::post(std::function<void()> fn) {
  std::weak_ptr<int> alive = alive_;
  loop_.post([alive, fn = std::move(fn)] {
    if (alive.lock()) fn();
  });
}

void Updater::start(std::function<void()> job) {
  if (worker_.joinable()) worker_.join();  // the previous job has posted its result already
  busy_ = true;
  worker_ = std::thread(std::move(job));
}

void Updater::check(std::function<void(const std::string &err, const Release &r)> done) {
  if (busy_) return;
  start([this, done = std::move(done)] {
    Release r;
    std::string err;
    // github.com redirects releases/latest to the newest stable release (drafts and pre-releases
    // excluded), or to the release list when there is none. Unlike api.github.com it has no
    // 60-requests-per-hour limit per public IP, which speakers behind a shared VPN/CGNAT exit hit.
    const std::string base = "https://github.com/" + repo_ + "/releases/";
    HttpsResponse resp = https_get(base + "latest", {user_agent_}, kMaxSmall, {}, &cancel_, 0);
    const std::string marker = "/releases/tag/";
    size_t at = resp.location.find(marker);
    if (resp.status == 0) {
      err = resp.error;
    } else if (resp.status < 300 || resp.status >= 400) {
      err = "github.com: HTTP " + std::to_string(resp.status);
    } else if (at == std::string::npos) {
      err = "no release published yet";
    } else if (r.version = resp.location.substr(at + marker.size()); !safe_tag(r.version)) {
      err = "unexpected release tag in " + resp.location;
    } else {
      r.url = base + "tag/" + r.version;
      r.bin_url = base + "download/" + r.version + "/pulsar-" + arch() + ".bin";
      r.sums_url = base + "download/" + r.version + "/SHA256SUMS";
    }
    post([this, done, err, r] {
      busy_ = false;
      done(err, r);
    });
  });
}

void Updater::install(const Release &r, const std::string &binary, std::function<void(float)> progress,
                      std::function<void(const std::string &err, bool fatal)> done) {
  if (busy_) return;
  start([this, r, binary, progress = std::move(progress), done = std::move(done), extras = extras_] {
    std::string err;
    bool fatal = false;
    const std::string asset = std::string("pulsar-") + arch() + ".bin";
    const std::string dir = binary.substr(0, binary.rfind('/'));
    const std::string staged = dir + "/pulsar.new";
    [&] {
      int last = -1;
      HttpsResponse bin = https_get(
          r.bin_url, {user_agent_}, kMaxBinary,
          [&](size_t got, size_t total) {
            int pct = total ? int(got * 95 / total) : 0;
            if (pct == last) return;
            last = pct;
            post([progress, pct] { progress(float(pct)); });
          },
          &cancel_);
      // 404: the release has no such asset; retrying later would not help.
      if (bin.status != 200) {
        err = asset + ": " + (bin.status ? "HTTP " + std::to_string(bin.status) : bin.error);
        fatal = bin.status == 404;
        return;
      }
      HttpsResponse sums = https_get(r.sums_url, {user_agent_}, kMaxSmall, {}, &cancel_);
      if (sums.status != 200) {
        err = "SHA256SUMS: " + (sums.status ? "HTTP " + std::to_string(sums.status) : sums.error);
        fatal = sums.status == 404;
        return;
      }
      std::string want = expected_sha(sums.body, asset), got = sha256_hex(bin.body);
      if (want.empty() || got != want) {
        err = want.empty() ? "SHA256SUMS lists no " + asset : asset + ": checksum mismatch";
        fatal = true;
        return;
      }
      if (!write_file(staged, bin.body, 0755)) {
        err = "cannot write " + staged + ": " + strerror(errno);
        return;
      }
      std::string says = run_version(staged);
      if (says != "pulsar " + r.version) {
        unlink(staged.c_str());
        err = asset + " does not run here as " + r.version + (says.empty() ? "" : " (reports '" + says + "')");
        fatal = true;
        return;
      }
      // Extras are downloaded and checked first and replaced only together with the binary: a new
      // radio image next to the old daemon would be written with the old daemon's protocol.
      std::vector<std::string> extras_staged;  // x.path + ".new", parallel to `extras`
      auto drop_staged = [&] {
        unlink(staged.c_str());
        for (const std::string &s : extras_staged)
          if (!s.empty()) unlink(s.c_str());
      };
      for (const Extra &x : extras) {
        extras_staged.emplace_back();
        const std::string url = r.sums_url.substr(0, r.sums_url.rfind('/') + 1) + x.name;
        HttpsResponse extra = https_get(url, {user_agent_}, kMaxBinary, {}, &cancel_);
        if (extra.status == 404) continue;  // a release without it
        if (extra.status != 200) {
          drop_staged();
          err = x.name + ": " + (extra.status ? "HTTP " + std::to_string(extra.status) : extra.error);
          return;
        }
        want = expected_sha(sums.body, x.name);
        if (want.empty() || sha256_hex(extra.body) != want) {
          drop_staged();
          err = want.empty() ? "SHA256SUMS lists no " + x.name : x.name + ": checksum mismatch";
          fatal = true;
          return;
        }
        if (!write_file(x.path + ".new", extra.body, 0644)) {
          drop_staged();
          err = "cannot write " + x.path + ".new: " + strerror(errno);
          return;
        }
        extras_staged.back() = x.path + ".new";
      }
      // The running binary becomes .prev: supervise.sh restores it if the new one crash-loops.
      std::string cur;
      if (!read_file(binary, cur) || !write_file(dir + "/pulsar.prev.tmp", cur, 0755) ||
          rename((dir + "/pulsar.prev.tmp").c_str(), (dir + "/pulsar.prev").c_str()) != 0) {
        drop_staged();
        err = "cannot keep the current binary as .prev: " + std::string(strerror(errno));
        return;
      }
      for (size_t i = 0; i < extras.size(); i++) {
        if (extras_staged[i].empty()) continue;
        if (rename(extras_staged[i].c_str(), extras[i].path.c_str()) != 0) {
          drop_staged();
          err = "cannot replace " + extras[i].path + ": " + strerror(errno);
          return;
        }
        extras_staged[i].clear();
      }
      if (rename(staged.c_str(), binary.c_str()) != 0) {
        unlink(staged.c_str());
        err = "cannot replace " + binary + ": " + strerror(errno);
        return;
      }
      sync();
    }();
    post([this, done, err, fatal] {
      busy_ = false;
      done(err, fatal);
    });
  });
}

}  // namespace cs
