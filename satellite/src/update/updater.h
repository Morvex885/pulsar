// pulsar updates from GitHub releases (Home Assistant "Firmware" update entity): finds the latest
// stable release, downloads pulsar-<arch>.bin, checks it against the release's SHA256SUMS and that
// it runs here (--version), then replaces the installed binary (kept as .prev). The caller exits
// the daemon afterwards; supervise.sh starts the new binary and rolls back to .prev if it
// crash-loops. Network work runs on a worker thread; results come back on the loop thread.
#pragma once

#include <atomic>
#include <functional>
#include <memory>
#include <string>
#include <thread>
#include <vector>

#include "core/loop.h"

namespace cs {

// Release tags are vMAJOR.MINOR.PATCH. A build describes itself (git describe) as the tag, as
// <tag>-N-g<hash>[-dirty] after it, or as a bare hash without tags.
struct Version {
  int v[3] = {0, 0, 0};
  bool exact = true;  // the tag itself, not a build after it
};
bool parse_version(const std::string &s, Version &out);
// latest is a newer release than current. current_known: current parsed (else latest != current
// decides, and automatic installs are off).
bool release_newer(const std::string &latest, const std::string &current, bool *current_known = nullptr);

struct Release {
  std::string version;   // tag_name
  std::string url;       // release page
  std::string bin_url;   // pulsar-<arch>.bin
  std::string sums_url;  // SHA256SUMS
};

class Updater {
 public:
  // repo: "owner/name" on GitHub.
  Updater(Loop &loop, std::string repo, std::string current_version);
  ~Updater();  // cancels and joins the worker

  bool busy() const { return busy_; }
  // err empty = ok. Ignored while busy.
  void check(std::function<void(const std::string &err, const Release &r)> done);
  // Replaces `binary`. fatal: the release itself is unusable (checksum, does not run here), as
  // opposed to a network failure worth retrying. progress: 0..100.
  void install(const Release &r, const std::string &binary, std::function<void(float)> progress,
               std::function<void(const std::string &err, bool fatal)> done);

  static const char *arch();  // asset suffix of this build: aarch64 / armhf
  // Other release assets installed with the binary, e.g. the radio's firmware images: `name` from
  // the same release, checked against SHA256SUMS, written to `path`. Releases without one are
  // installed without it.
  void add_extra_asset(std::string name, std::string path) { extras_.push_back({std::move(name), std::move(path)}); }

 private:
  void start(std::function<void()> job);
  void post(std::function<void()> fn);  // to the loop, dropped if the updater is gone

  Loop &loop_;
  std::string repo_, user_agent_;
  struct Extra {
    std::string name, path;
  };
  std::vector<Extra> extras_;  // set before the first install()
  std::thread worker_;
  std::atomic<bool> cancel_{false};
  bool busy_ = false;
  std::shared_ptr<int> alive_ = std::make_shared<int>(0);
};

}  // namespace cs
