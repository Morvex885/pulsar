#include "arbiter.h"

#include <arpa/inet.h>
#include <netinet/in.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <cmath>
#include <cstdio>
#include <cstring>

#include "core/log.h"

namespace cs {
namespace {

constexpr uint16_t kPort = 6054;
constexpr const char *kMagic = "pulsar-wake/1";
// Spotter confirmations of one utterance on stations a few metres apart land within ~100 ms of each
// other; the window adds Wi-Fi latency on top. It delays the earcon by as much.
constexpr int64_t kWindowMs = 250;
constexpr int64_t kLookbackMs = 500;  // a peer may confirm (and claim) before we do
// Broadcast over Wi-Fi is sent at a basic rate without ACKs or retries.
constexpr int64_t kResendMs = 40;
constexpr int kCopies = 3;
constexpr int64_t kKeepMs = 2000;

bool beats(const WakeArbiter::Claim &a, const WakeArbiter::Claim &b) {
  if (a.busy != b.busy) return a.busy;
  if (a.level_db != b.level_db) return a.level_db > b.level_db;
  return a.station > b.station;
}

}  // namespace

WakeArbiter::WakeArbiter(Loop &loop) : loop_(loop) {}

WakeArbiter::~WakeArbiter() {
  cancel();
  if (fd_ >= 0) {
    loop_.del_fd(fd_);
    close(fd_);
  }
}

bool WakeArbiter::start(std::string station) {
  station_ = std::move(station);
  const char *what = "socket";
  fd_ = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
  if (fd_ >= 0) {
    int one = 1;
    sockaddr_in sa{};
    sa.sin_family = AF_INET;
    sa.sin_port = htons(kPort);
    sa.sin_addr.s_addr = htonl(INADDR_ANY);
    // SO_REUSEADDR: a second binary on the same station (probe on another API port) also hears
    // the claims.
    if (setsockopt(fd_, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one) != 0)
      what = "SO_REUSEADDR";
    else if (setsockopt(fd_, SOL_SOCKET, SO_BROADCAST, &one, sizeof one) != 0)
      what = "SO_BROADCAST";
    else if (bind(fd_, reinterpret_cast<sockaddr *>(&sa), sizeof sa) != 0)
      what = "bind :6054";
    else
      what = nullptr;
  }
  if (what) {
    LOGW("arbiter: %s: %s; this station answers without arbitration", what, strerror(errno));
    if (fd_ >= 0) close(fd_);
    fd_ = -1;
    return false;
  }
  loop_.add_fd(fd_, EPOLLIN, [this](uint32_t) { on_readable(); });
  LOGI("arbiter: listening on udp :%u as '%s'", kPort, station_.c_str());
  return true;
}

void WakeArbiter::claim(const std::string &wake, float level_db, bool busy, DoneCb done) {
  cancel();
  if (fd_ < 0) {
    if (done) done(true, Claim{station_, wake, busy, level_db}, 0);
    return;
  }
  mine_ = Claim{station_, wake, busy, level_db};
  claim_ms_ = Loop::now_ms();
  send();
  copies_left_ = kCopies - 1;
  resend_timer_ = loop_.add_timer(
      kResendMs,
      [this] {
        send();
        if (--copies_left_ <= 0) {
          loop_.cancel_timer(resend_timer_);
          resend_timer_ = 0;
        }
      },
      kResendMs);
  if (done) {
    done_ = std::move(done);
    decide_timer_ = loop_.add_timer(kWindowMs, [this] {
      decide_timer_ = 0;
      decide();
    });
  }
}

void WakeArbiter::cancel() {
  if (resend_timer_) loop_.cancel_timer(resend_timer_);
  if (decide_timer_) loop_.cancel_timer(decide_timer_);
  resend_timer_ = decide_timer_ = 0;
  done_ = nullptr;
}

void WakeArbiter::send() {
  char buf[128];
  int n = snprintf(buf, sizeof buf, "%s %s %s %d %.1f", kMagic, mine_.station.c_str(), mine_.wake.c_str(),
                   mine_.busy ? 1 : 0, mine_.level_db);
  if (n <= 0 || (size_t)n >= sizeof buf) return;
  sockaddr_in to{};
  to.sin_family = AF_INET;
  to.sin_port = htons(kPort);
  to.sin_addr.s_addr = htonl(INADDR_BROADCAST);
  if (sendto(fd_, buf, (size_t)n, 0, reinterpret_cast<sockaddr *>(&to), sizeof to) < 0)
    LOGD("arbiter: sendto: %s", strerror(errno));
}

void WakeArbiter::on_readable() {
  char buf[256];
  for (;;) {
    ssize_t n = recv(fd_, buf, sizeof buf, 0);
    if (n < 0) {
      if (errno == EINTR) continue;
      if (errno != EAGAIN && errno != EWOULDBLOCK) LOGD("arbiter: recv: %s", strerror(errno));
      return;
    }
    if (n >= (ssize_t)sizeof buf - 1) continue;
    buf[n] = 0;
    char magic[16], station[32], wake[16];
    int busy = 0;
    float level = 0;
    if (sscanf(buf, "%15s %31s %15s %d %f", magic, station, wake, &busy, &level) != 5) continue;
    if (!std::isfinite(level) || level < -100.f || level > 0.f) continue;  // breaks the ordering
    if (strcmp(magic, kMagic) != 0) continue;
    if (station_ == station) continue;  // our own broadcast looped back

    int64_t now = Loop::now_ms();
    Claim c{station, wake, busy != 0, level};
    auto same = [&](const Received &r) {
      return r.claim.station == c.station && r.claim.wake == c.wake && r.claim.busy == c.busy &&
             r.claim.level_db == c.level_db && now - r.at_ms <= kResendMs * kCopies * 2;
    };
    if (std::any_of(recent_.begin(), recent_.end(), same)) continue;  // a resent copy
    recent_.erase(std::remove_if(recent_.begin(), recent_.end(),
                                 [&](const Received &r) { return now - r.at_ms > kKeepMs || r.claim.station == station; }),
                  recent_.end());
    recent_.push_back({c, now});
    LOGI("arbiter: claim from %s wake=%s busy=%d level=%.1f", station, wake, busy != 0, level);
  }
}

void WakeArbiter::decide() {
  Claim best = mine_;
  bool won = true;
  size_t peers = 0;
  // A peer that claimed more than a window before us decided before our claim existed: it (or
  // whoever beat it) answers already, so we yield whatever our level.
  const Claim *early = nullptr;
  for (const auto &r : recent_) {
    if (r.at_ms < claim_ms_ - kLookbackMs || r.claim.wake != mine_.wake) continue;
    peers++;
    if (r.at_ms < claim_ms_ - kWindowMs) {
      if (!early || beats(r.claim, *early)) early = &r.claim;
    } else if (beats(r.claim, best)) {
      best = r.claim;
      won = false;
    }
  }
  if (early) {
    best = *early;
    won = false;
  }
  DoneCb done = std::move(done_);
  done_ = nullptr;
  if (done) done(won, best, peers);
}

}  // namespace cs
